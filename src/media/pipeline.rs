use rtrb::{Consumer, Producer, RingBuffer};

use crate::devices::util::{try_write_planar, write_bounded_planar};

pub const DEFAULT_BUFFER_FRAMES: usize = 8192;

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
    /// The number of planes didn't match the producer set.
    ChannelMismatch(ChannelMismatch),
    /// The planes weren't all the same length, writing would desync the channels.
    UnequalPlanes { min: usize, max: usize },
    /// The consumer stopped draining before the write deadline. `dropped` frames were lost.
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
    pub fn write_slices(&mut self, samples: &[&[f64]]) -> Result<(), WriteError> {
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

        write_bounded_planar(&mut self.producers, samples, min).map_err(|t| WriteError::Timeout {
            dropped: min - t.written,
        })
    }

    /// Non-blocking variant of [`write_slices`]: writes whatever fits and reports the rest as
    /// dropped. Required when the consumer drains on the same thread (decoder → resampler), where
    /// the blocking version would spin against a consumer that can never run until we return.
    pub fn write_slices_nonblocking(&mut self, samples: &[&[f64]]) -> Result<(), WriteError> {
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

        let written = try_write_planar(&mut self.producers, samples, min);
        if written < min {
            return Err(WriteError::Timeout { dropped: min - written });
        }
        Ok(())
    }

    /// Non-blocking variant of [`write_slices`]: writes whatever fits and reports the rest as
    /// dropped. Required when the consumer drains on the same thread (decoder → resampler), where
    /// the blocking version would spin against a consumer that can never run until we return.
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

    /// Frames that can be written to every channel right now without blocking (the minimum free
    /// space across channels).
    pub fn available(&self) -> usize {
        self.producers
            .iter()
            .map(Producer::slots)
            .min()
            .unwrap_or(0)
    }

    /// Number of channels this producer set was built for.
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

    /// Try to read up to `max_count` samples, returning actual count read.
    /// This is the preferred method when you don't need to know the exact count beforehand.
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
                // can't happen (count is the min of every channel's slots), but keep the planes
                // equal-length with silence rather than desyncing the channels downstream
                Err(_) => staging.resize(count, 0.0),
            }
        }

        count
    }

    /// Number of channels this consumer set was built for.
    pub fn channel_count(&self) -> usize {
        self.channel_count
    }

    pub fn staging(&self) -> &[Vec<f64>] {
        &self.staging
    }

    /// Discard all buffered samples in the ring. Only safe when the producer side isn't writing
    /// concurrently, which holds on the single playback thread.
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

/// The audio pipeline: decoder output -> (resampler) -> (mixer) -> device input. All samples
/// travel as f64, which is lossless for every source format.
pub struct AudioPipeline {
    pub decoder_output: ChannelProducers,
    pub resampler_input: ChannelConsumers,
    /// Per-channel output buffer handed from the resampler to the mixer. Pre-allocated once,
    /// (hopefully) meaning it never needs to be resized (which avoids extra allocations).
    pub resampler_output: Vec<Vec<f64>>,
    pub device_input_producers: ChannelProducers,
    pub device_input: ChannelConsumers,
    pub source_rate: u32,
    pub target_rate: u32,
    /// Channel count of the source (decoder) side.
    pub source_channel_count: usize,
    /// Channel count of the device side.
    pub device_channel_count: usize,
    /// Capacity, in frames, of the `device_input` ring. Sized to hold a worst-case cycle's
    /// resampler output so a single write never overruns it.
    pub device_input_capacity: usize,
}

/// Upper bound on the frames one processing cycle can hand from the resampler
/// to the mixer/device stage.
pub fn output_frame_bound(source_rate: u32, target_rate: u32, buffer_frames: usize) -> usize {
    let scaled = (buffer_frames as u64 * u64::from(target_rate))
        .div_ceil(u64::from(source_rate.max(1))) as usize;
    scaled.max(buffer_frames) + 1024
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

        // The device-input ring must be able to absorb one full cycle's resampler output (the
        // resampler reads up to `buffer_frames` and can upsample), so a single write never blocks
        // on a same-thread consumer. Honoring `min_device_input_capacity` keeps the ring sized to
        // the largest capacity a previous track needed: when the per-track bound varies (it scales
        // with the source rate), a constant allocation size lets the heap reuse the same blocks
        // instead of growing a fresh segment on every track change.
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
        }
    }

    /// Clear the resampler→mixer handoff buffer without freeing its capacity.
    pub fn clear_resampler_output(&mut self) {
        for ch in &mut self.resampler_output {
            ch.clear();
        }
    }

    /// Grow the resampler→mixer handoff buffer to hold `frames` per channel, so a resampler whose
    /// worst-case cycle output exceeds the initial estimate never reallocates it mid-playback.
    /// Called at resampler creation (track start), where allocating is fine.
    pub fn ensure_resampler_output_capacity(&mut self, frames: usize) {
        for ch in &mut self.resampler_output {
            if ch.capacity() < frames {
                ch.reserve(frames - ch.len());
            }
        }
    }

    /// Whether the device-input ring has room to absorb another decode cycle's worth of output,
    /// given the current packet size (`frame_duration`).
    ///
    /// If we can't, it might cause the decode thread to stall and drop audio.
    pub fn can_accept_decode(&self, frame_duration: usize) -> bool {
        let needed = output_frame_bound(self.source_rate, self.target_rate, frame_duration)
            .min(self.device_input_capacity);
        self.device_input_producers.available() >= needed
    }

    /// Drop all buffered audio in the pipeline ring buffers, so a seek while playing is heard
    /// immediately instead of after the stale buffers drain.
    pub fn flush_buffers(&mut self) {
        self.resampler_input.drain();
        self.device_input.drain();
        self.clear_resampler_output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_slices_rejects_wrong_channel_count() {
        let (mut producers, _consumers) = ChannelBuffers::new(2, 64).split();

        // a plane count that doesn't match the producer errors instead of panicking.
        let one: [&[f64]; 1] = [&[0.0; 4]];
        assert_eq!(
            producers.write_slices(&one),
            Err(WriteError::ChannelMismatch(ChannelMismatch {
                expected: 2,
                got: 1
            }))
        );

        // the matching count still writes fine.
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

        // more samples than the ring holds with nobody draining: the deadline must surface as an
        // error naming the dropped frames, not a silent Ok
        let planes: [&[f64]; 1] = [&[0.0; 16]];
        assert_eq!(
            producers.write_slices(&planes),
            Err(WriteError::Timeout { dropped: 8 })
        );
    }

    #[test]
    fn write_slices_nonblocking_never_parks_and_reports_dropped() {
        let (mut producers, mut consumers) = ChannelBuffers::new(1, 8).split();

        // fits: full write, Ok
        let planes: [&[f64]; 1] = [&[0.0; 8]];
        assert!(producers.write_slices_nonblocking(&planes).is_ok());

        // ring full, same-thread consumer cannot drain: must return immediately with the
        // remainder reported instead of sleeping to the deadline
        let overflow: [&[f64]; 1] = [&[1.0; 4]];
        assert_eq!(
            producers.write_slices_nonblocking(&overflow),
            Err(WriteError::Timeout { dropped: 4 })
        );

        // after the consumer drains, the leftover fits again
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
}
