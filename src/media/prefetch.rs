//! 远程媒体源的后台预取包装层。
//!
//! 解码器在播放线程上同步读取媒体源；没有这一层时，一次网络卡停会把
//! 播放主循环按住最长单次读取超时的时间，输出环被抽干，UI 显示"播放中"
//! 却一片静音。填充线程独占数据源，在解码器前方维持一个有界缓冲，
//! 于是短暂的网络抖动根本到不了解码路径，播放循环只有在较长断连把缓冲
//! 完全抽干时才阻塞一次。
//!
//! 填充线程还会在填充出错后重连（一次惰性的零位移 seek 让 range 源从
//! 当前位置重新发请求），把一条已静默死掉的 TCP 连接变成一次全新的
//! ranged 请求。

use crate::sync::{RecoveredCondvar, RecoveredMutex};
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use symphonia::core::io::MediaSource;

/// 向前填充的高水位：约 50 秒 FLAC（~1 Mbps）或约 2.5 分钟 320 kbps 音频。
/// 达到高水位后填充线程主动挂起底层数据源连接（若数据源支持），释放空闲连接，
/// 避免服务端 Keep-Alive 超时强制 RST/FIN 断连与被动重连抖动。
const HIGH_WATER_BYTES: usize = 6 * 1024 * 1024;
/// 后台恢复填充的低水位触发阈值：约 16 秒 FLAC 或约 50 秒 320 kbps 音频。
/// 当消费导致未读缓冲跌破该线时，提前唤醒填充线程发起下一次续传。
/// 留有 2 MB 安全垫，足以完全吸收网络重连与握手延迟，杜绝解码器欠载。
const LOW_WATER_BYTES: usize = 2 * 1024 * 1024;
/// 单次填充步长，在任何锁之外取，因此卡停的读取永不阻塞 consumer。
const FILL_CHUNK: usize = 16 * 1024;

/// 缓冲最终稳定在高水位附近；一次性预留省掉起播阶段约 9 次 realloc——
/// 每次都在 state 锁内搬运已缓冲字节，与 consumer 争锁。已知长度的短源
/// 只按其长度预留，避免为几 KB 的流白占 4MB。
fn initial_buffer_capacity(byte_len: Option<u64>) -> usize {
    byte_len
        .map(|len| {
            usize::try_from(len)
                .unwrap_or(usize::MAX)
                .min(HIGH_WATER_BYTES + FILL_CHUNK)
        })
        .unwrap_or(HIGH_WATER_BYTES + FILL_CHUNK)
        .max(FILL_CHUNK)
}
/// consumer 的一次读取在判定流卡停之前最多可等待多久（以 IO 错误浮出，
/// 于是该曲目像任何其他解码失败一样被跳过）。重连发生在这个窗口之内；
/// 可恢复的断连会无缝续上播放。
const STALL_BUDGET: Duration = Duration::from_secs(30);
/// 出错后两次填充尝试之间的退避时长。
const RETRY_BACKOFF: Duration = Duration::from_millis(200);

/// 填充循环视角下的源健康度：重试是否永远不可能成功。
///
/// 被拒绝的签名 URL（流中途再次请求收到 HTTP 4xx）会一直被拒绝；填充线程
/// 必须抛出致命错误，而不是空转到 consumer 的卡停预算耗尽。默认：永不判死。
pub trait FillSource: Read + Seek + Send {
    fn url_dead(&self) -> bool {
        false
    }

    /// 当填充缓冲达到高水位并准备进入长休眠时通知源。
    /// 支持主动挂起的源（如 HTTP Range 源）可在此主动断开空闲连接，
    /// 避免服务端超时 RST 产生被动报错。默认无操作。
    fn suspend_idle(&mut self) {}
}

/// 每次 seek 都会自增：已在途的、来自旧流位置的填充结果被丢弃，
/// 而不是在缓冲已清空之后追加进去。
type Epoch = u64;

struct FillState {
    buf: Vec<u8>,
    /// consumer 在 `buf` 中的读取游标。读取从 `buf[read_pos..]` 拷贝，
    /// 而不是从头抽取（那样每读一次都要 memmove 剩余字节）；游标越过
    /// 缓冲中点后压缩一次，把搬运均摊为 ≤1 字节/已消费字节。
    read_pos: usize,
    epoch: Epoch,
    /// 最近一次 seek 的绝对目标，由填充线程落地：seek 之前已在途的读取
    /// 会按 epoch 丢弃，但它们仍然推进了共享的源——只有在这里重新施加
    /// seek 才能把流重新锚定。
    pending_seek: Option<u64>,
    eof: bool,
    shutdown: bool,
    /// 是否正处于积极填充周期：起播或跌破低水位后置 true，填满高水位后置 false。
    filling: bool,
    /// 源永久不可用时由填充线程设置：下一次 consumer 读取会立刻把它当作
    /// IO 错误返回，而不是在注定不会成功的重试上耗完整个卡停预算。
    fatal: Option<String>,
}

impl FillState {
    fn clear_stream(&mut self) {
        self.buf.clear();
        self.read_pos = 0;
        self.eof = false;
        self.filling = true;
    }
}

struct Shared<S> {
    /// 一次只做一项内部操作；填充线程只在单次 `read` 期间持有它，
    /// 因此 consumer 的 `seek` 最多等待一个网络步长。
    source: Mutex<S>,
    state: Mutex<FillState>,
    signal: Condvar,
}

/// 在解码器前方填充缓冲。独占数据源；[`PrefetchSource::drop`] 置位关停标志后，
/// 线程在下一个检查点退出。
///
/// 填充出错会触发一次重连（零位移 seek → 全新的 ranged 请求），并以递增
/// 退避重试，直到关停或某次 seek 重置了流；断连是否超出 consumer 的卡停
/// 预算，只在 consumer 一侧判定。自报已死的源（刷新也救不回来的被拒 URL）
/// 立即结束重试循环：致命错误交给 consumer，让它在几秒内跳过该曲目，
/// 而不是先播 30 秒静音。
fn fill_loop<S: FillSource>(shared: Arc<Shared<S>>, retry_backoff: Duration) {
    // 在线流生产者线程与解码泵同登记 MMCSS：2026-10-21 在线播放实测单 run
    // 累计 5262 次 callback underrun（本地回放零欠载），网络填充是播放链条
    // 上唯一未登记的生产环节；登记失败静默降级。守卫绑定本函数存活期——
    // 线程逐曲目重建，退出时 Drop 成对归还登记句柄（mmcss 模块契约）。
    let _mmcss_registration = crate::devices::mmcss::register_playback_thread();
    let mut consecutive_errors = 0u32;
    // 每轮只取一个 chunk，栈缓冲提到循环外复用：省掉填充热路径上每 16KB
    // 一次的零初始化。
    let mut chunk = [0u8; FILL_CHUNK];
    loop {
        // 等待有活干：有待施加的 seek，或缓冲低于高水位。
        // my_epoch 给本轮填充打标签，早于更新的 seek 的结果会被丢弃。
        let (my_epoch, seek_target) = {
            let mut st = shared.state.lock_recovered();
            loop {
                if st.shutdown {
                    return;
                }
                if let Some(target) = st.pending_seek.take() {
                    st.filling = true;
                    break (st.epoch, Some(target));
                }
                if st.filling && st.buf.len() - st.read_pos < HIGH_WATER_BYTES && !st.eof {
                    break (st.epoch, None);
                }
                st = shared.signal.wait_recovered(st);
            }
        };
        if let Some(target) = seek_target {
            let mut source = shared.source.lock_recovered();
            let _ = source.seek(SeekFrom::Start(target));
        }

        let read = {
            let mut source = shared.source.lock_recovered();
            source.read(&mut chunk)
        };
        match read {
            Ok(0) => {
                let mut st = shared.state.lock_recovered();
                if st.epoch == my_epoch {
                    st.eof = true;
                    st.filling = false;
                    shared.signal.notify_all();
                }
            }
            Ok(n) => {
                consecutive_errors = 0;
                let (reached_high_water, is_same_epoch) = {
                    let mut st = shared.state.lock_recovered();
                    if st.epoch == my_epoch {
                        st.buf.extend_from_slice(&chunk[..n]);
                        if st.buf.len() - st.read_pos >= HIGH_WATER_BYTES {
                            st.filling = false;
                        }
                        shared.signal.notify_all();
                        (!st.filling, true)
                    } else {
                        (false, false)
                    }
                };
                if is_same_epoch && reached_high_water {
                    let mut source = shared.source.lock_recovered();
                    source.suspend_idle();
                }
            }
            Err(e) => {
                let dead = {
                    let mut source = shared.source.lock_recovered();
                    let dead = source.url_dead();
                    // 零位移 seek：丢掉死连接，并让下一次 read 从当前位置重新请求。
                    let _ = source.seek(SeekFrom::Current(0));
                    dead
                };
                if dead {
                    // fatal 描述源健康度而非流位置（与守卫 buf/eof 的 epoch
                    // 不同层，粘滞语义本就跨 epoch）：死 URL 上任何 seek 都只
                    // 会再次被拒，无恢复能力。若按 epoch 门控，"死 URL + 失败
                    // 读在途 + buffer 非空时 seek" 的竞态会让填充线程无痕退
                    // 出——不设 fatal 也不留存活线程，consumer 每次 read 吃满
                    // 停滞预算才报错。eof/buf 的 epoch 门控保持不变。
                    let mut st = shared.state.lock_recovered();
                    st.fatal = Some(format!(
                        "remote URL rejected and could not be refreshed: {e}"
                    ));
                    shared.signal.notify_all();
                    return;
                }
                consecutive_errors += 1;
                if consecutive_errors == 1 || consecutive_errors.is_multiple_of(5) {
                    tracing::warn!(
                        consecutive = consecutive_errors,
                        error = %e,
                        "media stream fill failed; reconnecting"
                    );
                }
                // 可响应式中断退避：退避期间若用户拖拽 seek、切歌或关停流，立即响应退出或重定位，
                // 彻底避免原本不可中断的 thread::sleep 让切歌/seek 在网络抖动期白白卡住多达 2 秒。
                let backoff = retry_backoff * consecutive_errors.min(10);
                let st = shared.state.lock_recovered();
                if !st.shutdown && st.pending_seek.is_none() {
                    let _ = shared.signal.wait_timeout_recovered(st, backoff);
                }
            }
        }
    }
}

/// 经有界的后台填充缓冲读取的 [`MediaSource`]。
pub struct PrefetchSource<S> {
    shared: Arc<Shared<S>>,
    /// 下一次 `read` 返回的首字节的逻辑位置。
    pos: u64,
    byte_len: Option<u64>,
    seekable: bool,
    stall_budget: Duration,
}

impl<S: MediaSource + FillSource + 'static> PrefetchSource<S> {
    pub fn new(source: S) -> Self {
        Self::with_budget(source, STALL_BUDGET, RETRY_BACKOFF)
    }

    fn with_budget(source: S, stall_budget: Duration, retry_backoff: Duration) -> Self {
        // 先取源的静态属性快照：symphonia 在 consumer 侧查询这些，
        // 而源本体住在填充线程那边。
        let byte_len = source.byte_len();
        let seekable = source.is_seekable();
        let shared = Arc::new(Shared {
            source: Mutex::new(source),
            state: Mutex::new(FillState {
                buf: Vec::with_capacity(initial_buffer_capacity(byte_len)),
                read_pos: 0,
                epoch: 0,
                pending_seek: None,
                eof: false,
                shutdown: false,
                filling: true,
                fatal: None,
            }),
            signal: Condvar::new(),
        });
        let filler_shared = Arc::clone(&shared);
        // 启动失败必须降级而不是 panic：每打开一首在线曲目都会在播放线程上
        // 走到这里，此处 panic 会永久杀死播放（没有谁会重启这个线程）。
        // 与 audio_engine 的准备线程回退是同一取舍——牺牲缓冲，保住音乐。
        // 关停标志置位后，读取直通数据源，不再等一个永远没人填的缓冲。
        if thread::Builder::new()
            .name("media-prefetch".into())
            .spawn(move || fill_loop(filler_shared, retry_backoff))
            .is_err()
        {
            tracing::warn!("media prefetch thread spawn failed; reading remote stream unbuffered");
            shared.state.lock_recovered().shutdown = true;
        }
        Self {
            shared,
            pos: 0,
            byte_len,
            seekable,
            stall_budget,
        }
    }
}

impl<S: Read + Seek + Send> Read for PrefetchSource<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let deadline = Instant::now() + self.stall_budget;
        let mut st = self.shared.state.lock_recovered();
        loop {
            let available = st.buf.len() - st.read_pos;
            if available > 0 {
                let n = available.min(buf.len());
                buf[..n].copy_from_slice(&st.buf[st.read_pos..st.read_pos + n]);
                st.read_pos += n;
                // 均摊式压缩：只有游标越过中点才搬运剩余字节——
                // 每个已消费字节至多搬 1 字节，而不是每次读取整段 memmove。
                let consumed = st.read_pos;
                if consumed >= st.buf.len() / 2 {
                    st.buf.drain(..consumed);
                    st.read_pos = 0;
                }
                self.pos += n as u64;
                // 双水位滞回：只有当消费导致可用缓冲跌破低水位，且当前处于挂起状态时，
                // 才重新开启填充周期并唤醒填充线程。高低水位之间留有 4 MB 的消费窗口，
                // 稳态下全曲仅需少数几次按需拉取，彻底消灭连接闲置超时。
                let remaining = st.buf.len() - st.read_pos;
                if remaining < LOW_WATER_BYTES && !st.filling && !st.eof {
                    st.filling = true;
                    self.shared.signal.notify_all();
                }
                return Ok(n);
            }
            if st.eof {
                return Ok(0);
            }
            if let Some(msg) = st.fatal.as_deref() {
                return Err(io::Error::other(msg.to_string()));
            }
            if st.shutdown {
                // 没有填充线程（启动失败）：缓冲永远不会被填，所以直接读源
                // ——无缓冲，按源自身的读取超时阻塞。
                drop(st);
                let n = self.shared.source.lock_recovered().read(buf)?;
                self.pos += n as u64;
                return Ok(n);
            }
            // 纵深防御：缓冲抽空等待数据供给时，确保填充线程处于积极填充态，
            // 避免因任何瞬态竞态让填充线程停留在挂起态而导致消费者空耗卡停预算。
            if !st.filling && !st.eof {
                st.filling = true;
                self.shared.signal.notify_all();
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::other(format!(
                    "remote stream stalled: no data for {:?}",
                    self.stall_budget
                )));
            }
            st = self
                .shared
                .signal
                .wait_timeout_recovered(st, deadline - now)
                .0;
        }
    }
}

impl<S: Read + Seek + Send> Seek for PrefetchSource<S> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::Current(delta) => self.pos.checked_add_signed(delta),
            SeekFrom::End(delta) => self.byte_len.and_then(|len| len.checked_add_signed(delta)),
        };
        let Some(target) = target else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek target is out of range",
            ));
        };
        // 没有填充线程（启动失败）：把 seek 直接施加到源上，
        // 不要再排给一个不存在的填充线程。
        {
            let st = self.shared.state.lock_recovered();
            if st.shutdown {
                drop(st);
                self.shared
                    .source
                    .lock_recovered()
                    .seek(SeekFrom::Start(target))?;
                self.pos = target;
                return Ok(target);
            }
        }
        // 惰性重新锚定，与 range 源自身的 seek 同一套语义：填充线程在下一次
        // read 之前施加目标位置，所以不会有已在途的读取把流停在目标之后。
        let mut st = self.shared.state.lock_recovered();
        st.epoch += 1;
        st.pending_seek = Some(target);
        st.clear_stream();
        self.shared.signal.notify_all();
        self.pos = target;
        Ok(target)
    }
}

impl<S: Read + Seek + Send> MediaSource for PrefetchSource<S> {
    fn is_seekable(&self) -> bool {
        self.seekable
    }

    fn byte_len(&self) -> Option<u64> {
        self.byte_len
    }
}

impl<S> Drop for PrefetchSource<S> {
    fn drop(&mut self) {
        self.shared.state.lock_recovered().shutdown = true;
        self.shared.signal.notify_all();
        // 填充线程在它的下一个检查点退出。刻意 detach：若它正卡在一次网络
        // read 中间，join 会让播放线程多等一个内部 read 超时（≤30 秒）。
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 可注入读取失败的内存源；复刻 range 源的惰性重发语义
    /// （seek 从不触碰数据）。
    struct FakeSource {
        data: Vec<u8>,
        pos: usize,
        fail_reads: Arc<AtomicUsize>,
        suspends: Arc<AtomicUsize>,
    }

    impl FakeSource {
        fn new(len: usize, fail_reads: Arc<AtomicUsize>) -> Self {
            Self {
                data: (0..len).map(|i| i as u8).collect(),
                pos: 0,
                fail_reads,
                suspends: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl Read for FakeSource {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.fail_reads.load(Ordering::SeqCst) > 0 {
                self.fail_reads.fetch_sub(1, Ordering::SeqCst);
                return Err(io::Error::other("injected failure"));
            }
            let n = buf.len().min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    impl Seek for FakeSource {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.pos = match pos {
                SeekFrom::Start(o) => o as usize,
                SeekFrom::Current(d) => (self.pos as i64 + d) as usize,
                SeekFrom::End(d) => (self.data.len() as i64 + d) as usize,
            };
            Ok(self.pos as u64)
        }
    }

    impl MediaSource for FakeSource {
        fn is_seekable(&self) -> bool {
            true
        }
        fn byte_len(&self) -> Option<u64> {
            Some(self.data.len() as u64)
        }
    }

    impl FillSource for FakeSource {
        fn suspend_idle(&mut self) {
            self.suspends.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn open(len: usize, fail_reads: Arc<AtomicUsize>) -> PrefetchSource<FakeSource> {
        PrefetchSource::with_budget(
            FakeSource::new(len, fail_reads),
            Duration::from_secs(2),
            Duration::from_millis(1),
        )
    }

    fn read_exact_or_eof<S: Read>(source: &mut S, out: &mut Vec<u8>, want: usize) {
        let mut chunk = vec![0u8; 8192];
        while out.len() < want {
            let n = source.read(&mut chunk).expect("read should not fail");
            assert!(n > 0, "premature EOF at {}/{}", out.len(), want);
            out.extend_from_slice(&chunk[..n]);
        }
    }

    #[test]
    fn initial_capacity_tracks_source_length() {
        assert_eq!(initial_buffer_capacity(Some(1024)), FILL_CHUNK);
        assert_eq!(initial_buffer_capacity(Some(512 * 1024)), 512 * 1024);
        assert_eq!(initial_buffer_capacity(None), HIGH_WATER_BYTES + FILL_CHUNK);
        assert_eq!(
            initial_buffer_capacity(Some(u64::MAX)),
            HIGH_WATER_BYTES + FILL_CHUNK
        );
    }

    #[test]
    fn prefetch_buffer_is_preallocated_before_fill() {
        let source = open(512 * 1024, Arc::new(AtomicUsize::new(0)));
        assert!(
            source.shared.state.lock_recovered().buf.capacity() >= 512 * 1024,
            "起播阶段不应在 state 锁内反复扩容"
        );
    }

    #[test]
    fn prefetches_and_delivers_to_eof() {
        const LEN: usize = 512 * 1024;
        let mut source = open(LEN, Arc::new(AtomicUsize::new(0)));
        let mut got = Vec::new();
        read_exact_or_eof(&mut source, &mut got, LEN);
        assert_eq!(got, (0..LEN).map(|i| i as u8).collect::<Vec<_>>());
        assert_eq!(source.read(&mut [0u8; 16]).unwrap(), 0, "EOF");
    }

    #[test]
    fn seek_repositions_and_discards_stale_fill() {
        const LEN: usize = 512 * 1024;
        let mut source = open(LEN, Arc::new(AtomicUsize::new(0)));
        let mut got = Vec::new();
        read_exact_or_eof(&mut source, &mut got, 64 * 1024);

        assert_eq!(source.seek(SeekFrom::Start(16 * 1024)).unwrap(), 16 * 1024);
        got.clear();
        read_exact_or_eof(&mut source, &mut got, 16 * 1024);
        assert_eq!(
            got,
            (16 * 1024..32 * 1024).map(|i| i as u8).collect::<Vec<_>>()
        );

        assert_eq!(
            source.seek(SeekFrom::End(-1024)).unwrap(),
            (LEN - 1024) as u64
        );
        got.clear();
        read_exact_or_eof(&mut source, &mut got, 1024);
        assert_eq!(
            got,
            ((LEN - 1024)..LEN).map(|i| i as u8).collect::<Vec<_>>()
        );
    }

    #[test]
    fn transient_error_recovers_via_reconnect() {
        // 两次读取失败，之后流恢复正常——填充线程必须重连（零位移 seek）
        // 并让 consumer 持续运行。
        let mut source = open(128 * 1024, Arc::new(AtomicUsize::new(2)));
        let mut got = Vec::new();
        read_exact_or_eof(&mut source, &mut got, 128 * 1024);
        assert_eq!(got, (0..128 * 1024).map(|i| i as u8).collect::<Vec<_>>());
    }

    #[test]
    fn persistent_error_surfaces_after_stall_budget() {
        let mut source = open(1024, Arc::new(AtomicUsize::new(usize::MAX)));
        let started = Instant::now();
        let err = source
            .read(&mut [0u8; 16])
            .expect_err("persistent failure must surface");
        assert!(started.elapsed() >= Duration::from_millis(1900));
        assert!(err.to_string().contains("stalled"), "{err}");
    }

    #[test]
    fn dual_watermark_suspends_at_high_water_and_resumes_at_low_water() {
        // 创建超过两倍高水位的源（16MB）：
        const TOTAL: usize = 16 * 1024 * 1024;
        let suspends = Arc::new(AtomicUsize::new(0));
        let fake = FakeSource {
            data: (0..TOTAL).map(|i| (i % 256) as u8).collect(),
            pos: 0,
            fail_reads: Arc::new(AtomicUsize::new(0)),
            suspends: Arc::clone(&suspends),
        };
        let mut source =
            PrefetchSource::with_budget(fake, Duration::from_secs(2), Duration::from_millis(1));

        // 等待填充线程达到高水位并主动挂起：
        let deadline = Instant::now() + Duration::from_secs(2);
        while suspends.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            suspends.load(Ordering::SeqCst) >= 1,
            "达到高水位必须触发 suspend_idle"
        );

        // 此时已挂起。消费 2MB（从 6MB 降至 4MB，仍高于低水位 2MB）：
        let mut sink = vec![0u8; 2 * 1024 * 1024];
        source.read_exact(&mut sink).expect("read should succeed");

        // 短暂等待，验证依然没有新的拉取周期（未跌破低水位不唤醒）：
        let before_low_water_suspends = suspends.load(Ordering::SeqCst);
        thread::sleep(Duration::from_millis(50));
        assert_eq!(
            suspends.load(Ordering::SeqCst),
            before_low_water_suspends,
            "未跌破低水位前不应过早开启新填充"
        );

        // 再消费 2.5MB（剩余缓冲约 1.5MB < LOW_WATER_BYTES=2MB）：
        let mut sink2 = vec![0u8; 2560 * 1024];
        source.read_exact(&mut sink2).expect("read should succeed");

        // 此时跌破低水位，填充线程必须被唤醒并将缓冲再次推满高水位并再次 suspend：
        let resume_deadline = Instant::now() + Duration::from_secs(2);
        while suspends.load(Ordering::SeqCst) == before_low_water_suspends
            && Instant::now() < resume_deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            suspends.load(Ordering::SeqCst) > before_low_water_suspends,
            "跌破低水位后必须重新唤醒并补满到高水位后再次 suspend"
        );
    }
}
