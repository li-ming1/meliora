use std::time::{Duration, Instant};

use rtrb::{Consumer, Producer, RingBuffer};

pub const DEFAULT_BUFFER_FRAMES: usize = 8192;

// 环形缓冲 IO 原语，由管线（解码 -> 播放交接）与设备输出级共用。
// 它们与所服务的通道缓冲类型放在一起，因此 media 层不需要反向依赖
// `devices`。
//
/// 环形缓冲 producer 在缓冲写满时，两次重试之间休眠多久。
pub const RING_WRITE_PARK: Duration = Duration::from_millis(1);
/// `write_bounded_planar` 面对写满的环形缓冲会重试多久，
/// 之后放弃并上报 [`RingWriteTimeout`]。
pub const RING_WRITE_DEADLINE: Duration = Duration::from_millis(250);

/// 环形缓冲的 consumer 在写截止时间内没有继续抽取。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingWriteTimeout {
    /// 截止时间到期前已写入每条环的采样数。
    pub written: usize,
}

/// 在当前能容纳的范围内写入等长平面（plane）的采样，绝不阻塞也不休眠。
/// 返回实际写入的总采样数（可能小于 `total`）。consumer 在*同一*线程抽取时
/// 用它——那种场合下阻塞式重试循环永远无法推进。
pub fn try_write_planar<T: Copy>(
    producers: &mut [Producer<T>],
    planes: &[&[T]],
    total: usize,
) -> usize {
    let mut written = 0;

    while written < total {
        let writable = producers
            .iter()
            .map(Producer::slots)
            .min()
            .unwrap_or(0)
            .min(total - written);

        if writable == 0 {
            break;
        }

        for (producer, plane) in producers.iter_mut().zip(planes) {
            if let Ok(chunk) = producer.write_chunk_uninit(writable) {
                chunk.fill_from_iter(plane[written..written + writable].iter().copied());
            }
        }
        written += writable;
    }

    written
}

/// 把等长平面的前 `total` 个采样同步写入各自的 producer，
/// 使各通道永不失步
pub fn write_bounded_planar<T: Copy>(
    producers: &mut [Producer<T>],
    planes: &[&[T]],
    total: usize,
) -> Result<(), RingWriteTimeout> {
    let mut written = try_write_planar(producers, planes, total);
    let deadline = Instant::now() + RING_WRITE_DEADLINE;

    while written < total {
        if Instant::now() >= deadline {
            return Err(RingWriteTimeout { written });
        }
        std::thread::sleep(RING_WRITE_PARK);
        // 从上次停止处续写：`try_write_planar` 是从所给平面的起始处开始计数的，
        // 所以要把剩余的子切片交给它——再次传完整平面会把 packet 的前缀
        // 重复拷进环里（听感上是静电噪声）
        // 用栈上分配覆盖现实的通道数：环写满期间每 1ms 重试都会走到这里，
        // 每次重试堆分配一个 Vec 是贴近实时路径上的分配抖动。
        let mut remaining = smallvec::SmallVec::<[&[T]; 8]>::with_capacity(planes.len());
        remaining.extend(planes.iter().map(|plane| &plane[written..]));
        written += try_write_planar(producers, &remaining, total - written);
    }

    Ok(())
}

pub fn read_available<T: Copy>(consumer: &mut Consumer<T>, data: &mut [T]) -> usize {
    let readable = consumer.slots().min(data.len());
    if readable == 0 {
        return 0;
    }
    let Ok(chunk) = consumer.read_chunk(readable) else {
        return 0;
    };
    let (first, second) = chunk.as_slices();
    data[..first.len()].copy_from_slice(first);
    data[first.len()..first.len() + second.len()].copy_from_slice(second);
    let read = first.len() + second.len();
    chunk.commit_all();
    read
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeResult {
    Decoded { frames: usize, rate: u32 },
    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelMismatch {
    pub expected: usize,
    pub got: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteError {
    /// 平面数量与 producer 集合不一致。
    ChannelMismatch(ChannelMismatch),
    /// 平面长度不全相等，继续写入会让各通道失步。
    UnequalPlanes { min: usize, max: usize },
    /// consumer 在写截止时间前停止抽取，丢失了 `dropped` 帧。
    Timeout { dropped: usize },
}

pub struct ChannelBuffers {
    buffers: Vec<(Producer<f64>, Consumer<f64>)>,
    channel_count: usize,
    buffer_size: usize,
}

impl ChannelBuffers {
    pub fn new(channel_count: usize, buffer_size: usize) -> Self {
        let buffers = (0..channel_count)
            .map(|_| RingBuffer::new(buffer_size))
            .collect();
        Self {
            buffers,
            channel_count,
            buffer_size,
        }
    }

    pub fn split(self) -> (ChannelProducers, ChannelConsumers) {
        let mut producers = Vec::with_capacity(self.channel_count);
        let mut consumers = Vec::with_capacity(self.channel_count);

        for (producer, consumer) in self.buffers {
            producers.push(producer);
            consumers.push(consumer);
        }

        (
            ChannelProducers {
                producers,
                channel_count: self.channel_count,
            },
            ChannelConsumers {
                consumers,
                channel_count: self.channel_count,
                staging: (0..self.channel_count)
                    .map(|_| Vec::with_capacity(self.buffer_size))
                    .collect(),
            },
        )
    }
}

pub struct ChannelProducers {
    producers: Vec<Producer<f64>>,
    channel_count: usize,
}

impl ChannelProducers {
    /// 依据本 producer 集合校验一组平面采样——通道数匹配、平面等长——并返回
    /// 公共平面长度。
    fn checked_plane_len(&self, samples: &[&[f64]]) -> Result<usize, WriteError> {
        if samples.len() != self.channel_count {
            return Err(WriteError::ChannelMismatch(ChannelMismatch {
                expected: self.channel_count,
                got: samples.len(),
            }));
        }

        let min = samples.iter().map(|s| s.len()).min().unwrap_or(0);
        let max = samples.iter().map(|s| s.len()).max().unwrap_or(0);
        if min != max {
            return Err(WriteError::UnequalPlanes { min, max });
        }

        Ok(min)
    }

    pub fn write_slices(&mut self, samples: &[&[f64]]) -> Result<(), WriteError> {
        let min = self.checked_plane_len(samples)?;

        write_bounded_planar(&mut self.producers, samples, min).map_err(|t| WriteError::Timeout {
            dropped: min - t.written,
        })
    }

    /// [`write_slices`] 的非阻塞版本：写入能容下的部分，其余按丢弃上报。
    /// 当 consumer 在同一线程抽取时（解码器 → 重采样器）必须用它——阻塞版本
    /// 会对着一个「只有等我们返回才能运行」的 consumer 空转。
    pub fn write_slices_nonblocking(&mut self, samples: &[&[f64]]) -> Result<(), WriteError> {
        let min = self.checked_plane_len(samples)?;

        let written = try_write_planar(&mut self.producers, samples, min);
        if written < min {
            return Err(WriteError::Timeout {
                dropped: min - written,
            });
        }
        Ok(())
    }

    /// [`write_slices`] 的非阻塞版本：写入能容下的部分，其余按丢弃上报。
    /// 当 consumer 在同一线程抽取时（解码器 → 重采样器）必须用它——阻塞版本
    /// 会对着一个「只有等我们返回才能运行」的 consumer 空转。
    pub fn write_vecs_nonblocking(&mut self, samples: &[Vec<f64>]) -> Result<(), WriteError> {
        if samples.len() != self.channel_count {
            return Err(WriteError::ChannelMismatch(ChannelMismatch {
                expected: self.channel_count,
                got: samples.len(),
            }));
        }

        let slices: smallvec::SmallVec<[&[f64]; 8]> = samples.iter().map(Vec::as_slice).collect();
        self.write_slices_nonblocking(&slices)
    }

    /// 此刻无需阻塞就能写入每个通道的帧数（各通道剩余空闲空间的最小值）。
    pub fn available(&self) -> usize {
        self.producers
            .iter()
            .map(Producer::slots)
            .min()
            .unwrap_or(0)
    }

    /// 本 producer 集合建好时的通道数。
    pub fn channel_count(&self) -> usize {
        self.channel_count
    }
}

pub struct ChannelConsumers {
    consumers: Vec<Consumer<f64>>,
    channel_count: usize,
    staging: Vec<Vec<f64>>,
}

impl ChannelConsumers {
    pub fn potentially_available(&self) -> usize {
        let available = self
            .consumers
            .iter()
            .map(Consumer::slots)
            .min()
            .unwrap_or(0);

        available.min(self.staging.first().map(|s| s.capacity()).unwrap_or(0))
    }

    /// 尽力读取至多 `max_count` 个采样，返回实际读到的数量。
    /// 不需要预先知道确切数量时优先用这个方法。
    pub fn try_read_to_staging(&mut self, max_count: usize) -> usize {
        let count = self
            .consumers
            .iter()
            .map(Consumer::slots)
            .min()
            .unwrap_or(0)
            .min(max_count);

        if count == 0 {
            for staging in &mut self.staging {
                staging.clear();
            }
            return 0;
        }

        for channel in 0..self.channel_count {
            let staging = &mut self.staging[channel];
            staging.clear();
            match self.consumers[channel].read_chunk(count) {
                Ok(chunk) => {
                    let (first, second) = chunk.as_slices();
                    staging.extend_from_slice(first);
                    staging.extend_from_slice(second);
                    chunk.commit_all();
                }
                // 不可能发生（count 取各通道 slots 的最小值），但用静音把各平面
                // 补齐成等长，好让下游通道不失步
                Err(_) => staging.resize(count, 0.0),
            }
        }

        count
    }

    /// 本 consumer 集合建好时的通道数。
    pub fn channel_count(&self) -> usize {
        self.channel_count
    }

    pub fn staging(&self) -> &[Vec<f64>] {
        &self.staging
    }

    /// 丢弃环形缓冲里全部已缓冲采样。仅当 producer 侧没有并发写入时才安全，
    /// 单播放线程的情形满足这一条。
    pub fn drain(&mut self) {
        for consumer in &mut self.consumers {
            let slots = consumer.slots();
            if slots > 0
                && let Ok(chunk) = consumer.read_chunk(slots)
            {
                chunk.commit_all();
            }
        }
        for staging in &mut self.staging {
            staging.clear();
        }
    }
}

/// 音频管线：解码器输出 ->（重采样器）->（混音器）-> 设备输入。
/// 所有采样都以 f64 传输，对任何源格式都是无损的。
pub struct AudioPipeline {
    pub decoder_output: ChannelProducers,
    pub resampler_input: ChannelConsumers,
    /// 由重采样器交给混音器的逐通道输出缓冲。一次性预分配，
    /// （希望）因此永不需要 resize（从而避免额外分配）。
    pub resampler_output: Vec<Vec<f64>>,
    pub device_input_producers: ChannelProducers,
    pub device_input: ChannelConsumers,
    pub source_rate: u32,
    pub target_rate: u32,
    /// 源（解码器）侧的通道数。
    pub source_channel_count: usize,
    /// 设备侧的通道数。
    pub device_channel_count: usize,
    /// `device_input` 环的容量（帧）。按一个最坏处理周期的重采样输出量设定，
    /// 使单次写入绝不写爆它。
    pub device_input_capacity: usize,
    /// `decoder_output` 环的容量（帧）：按能装下一个最坏解码 packet
    /// （流的 `max_frames_per_packet`，下限为 [`DEFAULT_BUFFER_FRAMES`]）设定，
    /// 于是完整 packet 总能装下。解码侧每个周期的读取上限必须用同一个值，
    /// 否则大 packet 会在环里留下残余，下一次写入无处可放。
    pub decoder_buffer_frames: usize,
}

/// 在换算后的帧数之上追加的余量，让这个界保持为「单个处理周期输出量」的
/// 安全上界。
const FRAME_BOUND_HEADROOM_FRAMES: usize = 1024;

/// 单个处理周期能从重采样器交给混音器/设备级的帧数上界。
pub fn output_frame_bound(source_rate: u32, target_rate: u32, buffer_frames: usize) -> usize {
    let scaled = (buffer_frames as u64 * u64::from(target_rate))
        .div_ceil(u64::from(source_rate.max(1))) as usize;
    scaled.max(buffer_frames) + FRAME_BOUND_HEADROOM_FRAMES
}

impl AudioPipeline {
    pub fn new(
        source_channel_count: usize,
        device_channel_count: usize,
        source_rate: u32,
        target_rate: u32,
        buffer_frames: usize,
        min_device_input_capacity: usize,
    ) -> Self {
        let (decoder_output, resampler_input) =
            ChannelBuffers::new(source_channel_count, buffer_frames).split();

        // 设备输入环必须能吞下整整一个周期的重采样输出（重采样器最多读
        // `buffer_frames`，且可能升采样），这样单次写入就不会阻塞在同一个
        // 线程的 consumer 上。尊重 `min_device_input_capacity` 可让环保持在
        // 历史最大曲目所需的容量：当每曲的界随之变化（它随源采样率缩放）时，
        // 恒定的分配大小让堆复用同一批内存块，而不是每次换曲都新开一段。
        let device_input_capacity = output_frame_bound(source_rate, target_rate, buffer_frames)
            .max(min_device_input_capacity);
        let (device_input_producers, device_input) =
            ChannelBuffers::new(device_channel_count, device_input_capacity).split();

        Self {
            decoder_output,
            resampler_input,
            resampler_output: (0..source_channel_count)
                .map(|_| Vec::with_capacity(device_input_capacity))
                .collect(),
            device_input_producers,
            device_input,
            source_rate,
            target_rate,
            source_channel_count,
            device_channel_count,
            device_input_capacity,
            decoder_buffer_frames: buffer_frames,
        }
    }

    /// 清空 重采样器→混音器 的交接缓冲，但不归还其容量。
    pub fn clear_resampler_output(&mut self) {
        for ch in &mut self.resampler_output {
            ch.clear();
        }
    }

    /// 把 重采样器→混音器 的交接缓冲扩容到每通道容 `frames` 帧，
    /// 这样最坏单周期输出超出初始估计的重采样器也不会在播放中途 realloc。
    /// 在重采样器创建时（曲目开始）调用，那里分配是允许的。
    pub fn ensure_resampler_output_capacity(&mut self, frames: usize) {
        for ch in &mut self.resampler_output {
            if ch.capacity() < frames {
                ch.reserve(frames - ch.len());
            }
        }
    }

    /// 按当前 packet 大小（`frame_duration`）判断，设备输入环是否还有余量
    /// 吞下再一个解码周期的输出。
    ///
    /// 若余量不足，解码线程可能卡停并丢音。
    pub fn can_accept_decode(&self, frame_duration: usize) -> bool {
        let needed = output_frame_bound(self.source_rate, self.target_rate, frame_duration)
            .min(self.device_input_capacity);
        self.device_input_producers.available() >= needed
    }

    /// 丢弃管线环形缓冲里全部已缓冲音频，使播放中跳转定位能立刻被听到，
    /// 而不是等陈旧缓冲抽干之后。
    pub fn flush_buffers(&mut self) {
        self.resampler_input.drain();
        self.device_input.drain();
        self.clear_resampler_output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rtrb::RingBuffer;
    use std::time::Duration;

    #[test]
    fn write_slices_rejects_wrong_channel_count() {
        let (mut producers, _consumers) = ChannelBuffers::new(2, 64).split();

        // 平面数量与 producer 不匹配时报错，而不是 panic。
        let one: [&[f64]; 1] = [&[0.0; 4]];
        assert_eq!(
            producers.write_slices(&one),
            Err(WriteError::ChannelMismatch(ChannelMismatch {
                expected: 2,
                got: 1
            }))
        );

        // 数量匹配时仍能正常写入。
        let two: [&[f64]; 2] = [&[0.0; 4], &[0.0; 4]];
        assert!(producers.write_slices(&two).is_ok());
    }

    #[test]
    fn write_slices_rejects_unequal_planes() {
        let (mut producers, _consumers) = ChannelBuffers::new(2, 64).split();

        let planes: [&[f64]; 2] = [&[0.0; 4], &[0.0; 3]];
        assert_eq!(
            producers.write_slices(&planes),
            Err(WriteError::UnequalPlanes { min: 3, max: 4 })
        );
    }

    #[test]
    fn write_slices_reports_timeout_instead_of_dropping_silently() {
        let (mut producers, _consumers) = ChannelBuffers::new(1, 8).split();

        // 采样比环能装的还多且无人抽取：截止时间必须以带丢失帧数的错误浮出，
        // 而不是静默返回 Ok
        let planes: [&[f64]; 1] = [&[0.0; 16]];
        assert_eq!(
            producers.write_slices(&planes),
            Err(WriteError::Timeout { dropped: 8 })
        );
    }

    #[test]
    fn write_slices_nonblocking_never_parks_and_reports_dropped() {
        let (mut producers, mut consumers) = ChannelBuffers::new(1, 8).split();

        // 装得下：完整写入，Ok
        let planes: [&[f64]; 1] = [&[0.0; 8]];
        assert!(producers.write_slices_nonblocking(&planes).is_ok());

        // 环已写满、同线程 consumer 无法抽取：必须立即返回并上报剩余量，
        // 而不是一路睡到截止时间
        let overflow: [&[f64]; 1] = [&[1.0; 4]];
        assert_eq!(
            producers.write_slices_nonblocking(&overflow),
            Err(WriteError::Timeout { dropped: 4 })
        );

        // consumer 抽干之后，剩下的又能装下了
        assert_eq!(consumers.try_read_to_staging(8), 8);
        assert!(producers.write_slices_nonblocking(&overflow).is_ok());
    }

    #[test]
    fn drain_empties_the_ring() {
        let (mut producers, mut consumers) = ChannelBuffers::new(2, 64).split();
        producers
            .write_vecs_nonblocking(&[vec![1.0; 16], vec![1.0; 16]])
            .unwrap();
        assert!(consumers.potentially_available() > 0);

        consumers.drain();
        assert_eq!(consumers.potentially_available(), 0);
    }

    #[test]
    fn flush_buffers_clears_pipeline() {
        let mut pipeline = AudioPipeline::new(2, 2, 44_100, 44_100, 64, 0);

        pipeline
            .device_input_producers
            .write_vecs_nonblocking(&[vec![1.0; 16], vec![1.0; 16]])
            .unwrap();
        assert!(pipeline.device_input.potentially_available() > 0);

        pipeline.flush_buffers();

        assert_eq!(pipeline.device_input.potentially_available(), 0);
    }

    /// 环小于负载会迫使阻塞写入休眠并重试；续写必须追加平面的剩余部分，
    /// 而不是重抄它的前缀（回归：重复的前缀听感上是静电噪声）。
    #[test]
    fn write_bounded_planar_retries_continue_at_the_write_offset() {
        const TOTAL: usize = 10;
        let (mut producer, mut consumer) = RingBuffer::<u32>::new(8);

        let drainer = std::thread::spawn(move || {
            let mut out = Vec::with_capacity(TOTAL);
            let mut staging = [0u32; 4];
            while out.len() < TOTAL {
                let n = read_available(&mut consumer, &mut staging);
                out.extend_from_slice(&staging[..n]);
                if n == 0 {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            out
        });

        let plane: Vec<u32> = (0..TOTAL as u32).collect();
        write_bounded_planar(std::slice::from_mut(&mut producer), &[&plane], TOTAL)
            .expect("consumer drains concurrently, write must complete");

        let out = drainer.join().unwrap();
        assert_eq!(out, plane, "retry continuations must preserve sample order");
    }
}
