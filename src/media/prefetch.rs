//! Background-prefetch wrapper for remote media sources.
//!
//! The decoder reads synchronously from the media source on the playback
//! thread; without this layer a network stall parks the playback main loop
//! for up to the per-read timeout, the output ring runs dry and playback
//! sits silent on a "playing" UI. The filler thread owns the source and
//! keeps a bounded buffer ahead of the decoder, so short network jitter
//! never reaches the decode path at all, and the playback loop only blocks
//! once the buffer is fully drained by a longer outage.
//!
//! The filler also reconnects after fill errors (a lazy zero-delta seek
//! makes the range source re-request from its current position), turning
//! a silently dead TCP connection into a fresh ranged request.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use symphonia::core::io::MediaSource;

/// Fill ahead up to this many bytes: ~6 s of 320 kbps or ~16 s of 128 kbps
/// audio. Bounds the memory cost per playing track.
const HIGH_WATER_BYTES: usize = 256 * 1024;
/// One fill step, taken outside every lock so a stalled read never blocks
/// the consumer.
const FILL_CHUNK: usize = 16 * 1024;
/// How long a consumer read may wait for data before the stream is declared
/// stalled (surfaces as an IO error, so the track is skipped like any other
/// decode failure). Reconnects happen inside this window; a recoverable
/// outage resumes playback seamlessly.
const STALL_BUDGET: Duration = Duration::from_secs(30);
/// Backoff between fill attempts after an error.
const RETRY_BACKOFF: Duration = Duration::from_millis(200);

/// Bumps on every seek: in-flight fill results from the old stream position
/// are discarded instead of appended after the buffer was cleared.
type Epoch = u64;

struct FillState {
    buf: Vec<u8>,
    /// Consumer read cursor into `buf`. Reads copy from `buf[read_pos..]`
    /// instead of draining from the front (which memmoved the remaining
    /// bytes on every read); the buffer is compacted once the cursor passes
    /// its midpoint, amortizing the move to ≤1 byte per byte consumed.
    read_pos: usize,
    epoch: Epoch,
    /// Absolute target of the latest seek, applied by the filler thread:
    /// in-flight reads from before the seek are discarded by epoch, but
    /// they still advance the shared source — only re-applying the seek
    /// here re-anchors the stream.
    pending_seek: Option<u64>,
    eof: bool,
    shutdown: bool,
}

impl FillState {
    fn clear_stream(&mut self) {
        self.buf.clear();
        self.read_pos = 0;
        self.eof = false;
    }
}

struct Shared<S> {
    /// One inner operation at a time; the filler holds it only across a
    /// single `read`, so a consumer `seek` waits at most one network step.
    source: Mutex<S>,
    state: Mutex<FillState>,
    signal: Condvar,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Fills the buffer ahead of the decoder. Owns the source; exits at the
/// next checkpoint after [`PrefetchSource::drop`] signals shutdown.
///
/// Fill errors trigger a reconnect (zero-delta seek → fresh ranged request)
/// and are retried with ramping backoff until the shutdown or a seek
/// resets the stream; whether the outage outlives the consumer's stall
/// budget is decided on the consumer side alone.
fn fill_loop<S: Read + Seek + Send>(shared: Arc<Shared<S>>, retry_backoff: Duration) {
    let mut consecutive_errors = 0u32;
    loop {
        // Wait for work: a seek to apply, or buffer below the high water
        // mark. my_epoch tags this fill pass so results from before a
        // newer seek are discarded.
        let (my_epoch, seek_target) = {
            let mut st = lock(&shared.state);
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
                st = shared
                    .signal
                    .wait(st)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        };
        if let Some(target) = seek_target {
            let mut source = lock(&shared.source);
            let _ = source.seek(SeekFrom::Start(target));
        }

        let mut chunk = [0u8; FILL_CHUNK];
        let read = {
            let mut source = lock(&shared.source);
            source.read(&mut chunk)
        };
        match read {
            Ok(0) => {
                let mut st = lock(&shared.state);
                if st.epoch == my_epoch {
                    st.eof = true;
                    shared.signal.notify_all();
                }
            }
            Ok(n) => {
                consecutive_errors = 0;
                let mut st = lock(&shared.state);
                if st.epoch == my_epoch {
                    st.buf.extend_from_slice(&chunk[..n]);
                    shared.signal.notify_all();
                }
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors == 1 || consecutive_errors % 5 == 0 {
                    tracing::warn!(
                        consecutive = consecutive_errors,
                        error = %e,
                        "media stream fill failed; reconnecting"
                    );
                }
                // Zero-delta seek: drops the dead connection and makes the
                // next read re-request from the current position.
                {
                    let mut source = lock(&shared.source);
                    let _ = source.seek(SeekFrom::Current(0));
                }
                thread::sleep(retry_backoff * consecutive_errors.min(10) as u32);
            }
        }
    }
}

/// A [`MediaSource`] that reads through a bounded background-filled buffer.
pub struct PrefetchSource<S> {
    shared: Arc<Shared<S>>,
    /// Logical position of the next byte `read` returns.
    pos: u64,
    byte_len: Option<u64>,
    seekable: bool,
    stall_budget: Duration,
}

impl<S: MediaSource + 'static> PrefetchSource<S> {
    pub fn new(source: S) -> Self {
        Self::with_budget(source, STALL_BUDGET, RETRY_BACKOFF)
    }

    fn with_budget(source: S, stall_budget: Duration, retry_backoff: Duration) -> Self {
        // Snapshot the static source properties: symphonia consults them on
        // the consumer side, while the source itself lives on the filler.
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
            }),
            signal: Condvar::new(),
        });
        let filler_shared = Arc::clone(&shared);
        // A failed spawn must degrade, not panic: this runs on the playback
        // thread per online track open, and a panic here kills playback for
        // good (no one restarts the thread). Same tradeoff as audio_engine's
        // prepare-thread fallback — lose the buffering, keep the music. With
        // the shutdown flag set, reads go straight to the source instead of
        // waiting on a buffer no filler will ever fill.
        if thread::Builder::new()
            .name("media-prefetch".into())
            .spawn(move || fill_loop(filler_shared, retry_backoff))
            .is_err()
        {
            tracing::warn!("media prefetch thread spawn failed; reading remote stream unbuffered");
            lock(&shared.state).shutdown = true;
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
        let mut st = lock(&self.shared.state);
        loop {
            let available = st.buf.len() - st.read_pos;
            if available > 0 {
                let n = available.min(buf.len());
                buf[..n].copy_from_slice(&st.buf[st.read_pos..st.read_pos + n]);
                st.read_pos += n;
                // Amortized compaction: only move the remaining bytes once
                // the cursor passes the midpoint — at most one byte moved
                // per byte consumed, instead of a full memmove per read.
                let consumed = st.read_pos;
                if consumed >= st.buf.len() / 2 {
                    st.buf.drain(..consumed);
                    st.read_pos = 0;
                }
                self.pos += n as u64;
                self.shared.signal.notify_all();
                return Ok(n);
            }
            if st.eof {
                return Ok(0);
            }
            if st.shutdown {
                // No filler thread (spawn failed): nothing will ever fill the
                // buffer, so read the source directly — unbuffered, blocking
                // for the source's own read timeout.
                drop(st);
                let n = lock(&self.shared.source).read(buf)?;
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
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
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
        // No filler thread (spawn failed): apply the seek to the source
        // directly instead of queueing it for a filler that does not exist.
        {
            let st = lock(&self.shared.state);
            if st.shutdown {
                drop(st);
                lock(&self.shared.source).seek(SeekFrom::Start(target))?;
                self.pos = target;
                return Ok(target);
            }
        }
        // Lazy re-anchor, mirroring the range source's own seek: the filler
        // applies the target before its next read, so no in-flight read can
        // leave the stream positioned past the target.
        let mut st = lock(&self.shared.state);
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
        lock(&self.shared.state).shutdown = true;
        self.shared.signal.notify_all();
        // The filler exits at its next checkpoint. Detached on purpose:
        // joining could block the playback thread for one inner read
        // timeout (≤30 s) if it is parked mid-network-read.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// In-memory source with injectable read failures; mirrors the lazy
    /// re-request semantics of the range source (seek never touches data).
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
        // Two failed reads, then the stream works again — the filler must
        // reconnect (zero-delta seek) and keep the consumer running.
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
