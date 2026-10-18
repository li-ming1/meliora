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

/// 向前填充至多这么多字节：约 8 秒 FLAC（~1 Mbps）、约 25 秒 320 kbps
/// 或约 64 秒 128 kbps 音频；播放一首曲子时实时占用约 1 MB。
/// 它与填充重连节奏一并定标，好让服务端瞬时卡停被无痕吸收：缓冲能撑过
/// 重连窗口，解码器到达缓冲底部之前新的 ranged 请求就把缓冲填回去了。
/// 旧的 256 KB 高水位只够约 2 秒 FLAC——一次 13 秒的服务端暂停在
/// 一个会话里记下了 981 次回调欠载（2026-10-03），既没有填充错误也没换曲，
/// 之后自行恢复。
const HIGH_WATER_BYTES: usize = 1024 * 1024;
/// 单次填充步长，在任何锁之外取，因此卡停的读取永不阻塞 consumer。
const FILL_CHUNK: usize = 16 * 1024;
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
    /// 源永久不可用时由填充线程设置：下一次 consumer 读取会立刻把它当作
    /// IO 错误返回，而不是在注定不会成功的重试上耗完整个卡停预算。
    fatal: Option<String>,
}

impl FillState {
    fn clear_stream(&mut self) {
        self.buf.clear();
        self.read_pos = 0;
        self.eof = false;
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
    let mut consecutive_errors = 0u32;
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
                    break (st.epoch, Some(target));
                }
                if st.buf.len() - st.read_pos < HIGH_WATER_BYTES && !st.eof {
                    break (st.epoch, None);
                }
                st = shared.signal.wait_recovered(st);
            }
        };
        if let Some(target) = seek_target {
            let mut source = shared.source.lock_recovered();
            let _ = source.seek(SeekFrom::Start(target));
        }

        let mut chunk = [0u8; FILL_CHUNK];
        let read = {
            let mut source = shared.source.lock_recovered();
            source.read(&mut chunk)
        };
        match read {
            Ok(0) => {
                let mut st = shared.state.lock_recovered();
                if st.epoch == my_epoch {
                    st.eof = true;
                    shared.signal.notify_all();
                }
            }
            Ok(n) => {
                consecutive_errors = 0;
                let mut st = shared.state.lock_recovered();
                if st.epoch == my_epoch {
                    st.buf.extend_from_slice(&chunk[..n]);
                    shared.signal.notify_all();
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
                    let mut st = shared.state.lock_recovered();
                    if st.epoch == my_epoch {
                        st.fatal = Some(format!(
                            "remote URL rejected and could not be refreshed: {e}"
                        ));
                        shared.signal.notify_all();
                    }
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
                thread::sleep(retry_backoff * consecutive_errors.min(10));
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
                buf: Vec::new(),
                read_pos: 0,
                epoch: 0,
                pending_seek: None,
                eof: false,
                shutdown: false,
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
                // 只有当本次抽取让缓冲从"不低于高水位"跨到"低于高水位"时才唤醒
                // 填充线程：这是唯一能解开停在 `signal.wait` 上的填充线程的跃迁
                // （它的谓词是 `available < HIGH_WATER`，每次 wait 前都在 state 锁内
                // 重新检查，所以一次跨界绝不会被漏掉）。抽取后仍低于水位的读取，
                // 填充线程本来就在填；抽取后仍高于水位的读取，对等待中的填充线程
                // 毫无改变。于是稳态下每个补满周期只有一次 notify，
                // 而不是每次解码读取一次。
                if available >= HIGH_WATER_BYTES
                    && st.buf.len() - st.read_pos < HIGH_WATER_BYTES
                    && !st.eof
                {
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
    }

    impl FakeSource {
        fn new(len: usize, fail_reads: Arc<AtomicUsize>) -> Self {
            Self {
                data: (0..len).map(|i| i as u8).collect(),
                pos: 0,
                fail_reads,
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

    impl FillSource for FakeSource {}

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
}
