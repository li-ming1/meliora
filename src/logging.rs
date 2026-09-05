use std::{
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use file_rotate::{
    ContentLimit, FileRotate,
    compression::Compression,
    suffix::{AppendTimestamp, FileLimit},
};
use tracing_subscriber::{
    Layer,
    fmt::{self, MakeWriter},
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

// Span lifecycle events (NEW/CLOSE) are deliberately not emitted: on
// tracing-subscriber 0.3 they bypass the per-layer EnvFilter entirely, so
// gpui's hot `#[instrument]` spans (sum_tree cursor seek, ~1 pair per list
// scroll tick) flood the rotating log even with `sum_tree=warn` in place.
const DEFAULT_LOG_FILTER: &str = "info,symphonia=warn,zbus=warn,sum_tree=warn";
const LOG_FILE_NAME: &str = "meliora.log";
const MAX_LOG_FILE_SIZE: usize = 1024 * 1024;
const MAX_LOG_FILES: usize = 4;

type RotatingLogFile = FileRotate<AppendTimestamp>;
type SharedLogFile = Arc<Mutex<Option<RotatingLogFile>>>;

static ACTIVE_LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static LOG_FILE: OnceLock<SharedLogFile> = OnceLock::new();

/// Initializes logging to stderr and, when available, to a rotating log file.
pub fn init() -> anyhow::Result<()> {
    let env = tracing_subscriber::EnvFilter::builder().parse(filter_value())?; // inform user they have a malformed filter
    let active_log_path = default_active_log_path();
    let _ = ACTIVE_LOG_PATH.set(active_log_path.clone());
    let file_writer = open_file_make_writer(&active_log_path);

    if let Some(writer) = &file_writer {
        let _ = LOG_FILE.set(writer.file.clone());
    }

    let stderr_layer = fmt::layer()
        .with_writer(StderrMakeWriter)
        .with_ansi(io::stderr().is_terminal())
        .with_thread_names(true) // nice to have until we replace with tasks
        .with_timer(fmt::time::uptime()) // date's useless
        .with_filter(env.clone());
    let file_layer = file_writer.map(|writer| {
        fmt::layer()
            .with_writer(writer)
            .with_ansi(false)
            .with_thread_names(true)
            .with_timer(fmt::time::uptime())
            .with_filter(env)
    });

    let subscriber = tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer);

    #[cfg(feature = "console")]
    let subscriber = subscriber.with(console_subscriber::spawn());

    subscriber.init();
    install_panic_hook();
    Ok(())
}

/// Installs a panic hook that funnels panics into the rotating log file
/// (thread, location, payload, backtrace) so crashes leave a trace instead of
/// dying silently with the message lost on stderr.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".into());
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".into()
        };
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("<unnamed>").to_string();
        let backtrace = std::backtrace::Backtrace::force_capture();

        tracing::error!(
            thread = %name,
            location = %location,
            payload = %payload,
            backtrace = %format!("{backtrace}"),
            "PANIC captured by panic hook"
        );
        flush();

        default_hook(info);
    }));
}

/// Flushes stderr and the active log file.
pub fn flush() {
    let _ = io::stderr().flush();

    if let Some(file) = LOG_FILE.get()
        && let Ok(mut state) = file.lock()
        && let Some(state) = state.as_mut()
    {
        let _ = state.flush();
        let _ = fs::File::open(active_log_path()).and_then(|file| file.sync_data());
    }
}

pub fn active_log_path() -> PathBuf {
    ACTIVE_LOG_PATH
        .get()
        .cloned()
        .unwrap_or_else(default_active_log_path)
}

fn default_active_log_path() -> PathBuf {
    active_log_path_in(&crate::paths::log_dir())
}

fn active_log_path_in(log_dir: &Path) -> PathBuf {
    log_dir.join(LOG_FILE_NAME)
}

fn filter_value() -> String {
    ["MELIORA_LOG", "RUST_LOG"] // prefer Meliora-specific variable
        .iter() // find the first one that's set at all
        .find_map(|key| std::env::var(key).ok()) // even if it's empty
        .filter(|value| !value.is_empty()) // NOW we can check is_empty and use default
        .unwrap_or_else(|| DEFAULT_LOG_FILTER.to_owned())
}

fn open_file_make_writer(log_path: &Path) -> Option<FileMakeWriter> {
    fs::create_dir_all(log_path.parent()?).ok()?;
    Some(FileMakeWriter::new(
        new_log_rotate(log_path)?,
        log_path.to_path_buf(),
    ))
}

/// Builds a fresh rotating sink. Opened in append mode by file-rotate, so a
/// rebuild never truncates what is already on disk.
fn new_log_rotate(log_path: &Path) -> Option<RotatingLogFile> {
    Some(FileRotate::new(
        log_path,
        AppendTimestamp::default(FileLimit::MaxFiles(MAX_LOG_FILES)),
        ContentLimit::BytesSurpassed(MAX_LOG_FILE_SIZE),
        Compression::None,
        None,
    ))
}

/// Creates stderr writers for the tracing stderr layer.
#[derive(Clone, Copy)]
struct StderrMakeWriter;

impl<'a> MakeWriter<'a> for StderrMakeWriter {
    type Writer = StderrWriter;

    fn make_writer(&'a self) -> Self::Writer {
        StderrWriter {
            buffer: Vec::with_capacity(256),
        }
    }
}

/// Buffers a single formatted log record before writing it to stderr.
struct StderrWriter {
    buffer: Vec<u8>,
}

impl StderrWriter {
    fn commit(&mut self) {
        if self.buffer.is_empty() {
            return;
        }

        let buffer = std::mem::take(&mut self.buffer);
        let _ = write_stderr(&buffer);
    }
}

impl Write for StderrWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.commit();
        Ok(())
    }
}

impl Drop for StderrWriter {
    fn drop(&mut self) {
        self.commit();
    }
}

fn write_stderr(buffer: &[u8]) -> io::Result<()> {
    let mut stderr = io::stderr().lock();
    stderr.write_all(buffer)
}

/// Creates file writers that share access to the rotating log file state.
#[derive(Clone)]
struct FileMakeWriter {
    file: SharedLogFile,
    /// Path of the active log, kept so a failed write can rebuild the sink.
    path: PathBuf,
}

impl FileMakeWriter {
    fn new(file: RotatingLogFile, path: PathBuf) -> Self {
        Self {
            file: Arc::new(Mutex::new(Some(file))),
            path,
        }
    }
}

impl<'a> MakeWriter<'a> for FileMakeWriter {
    type Writer = FileWriter;

    fn make_writer(&'a self) -> Self::Writer {
        FileWriter {
            file: self.file.clone(),
            path: self.path.clone(),
            buffer: Vec::with_capacity(256),
        }
    }
}

/// Buffers a single formatted log record before writing it to the shared file.
struct FileWriter {
    file: SharedLogFile,
    path: PathBuf,
    buffer: Vec<u8>,
}

impl FileWriter {
    fn commit(&mut self) {
        if self.buffer.is_empty() {
            return;
        }

        let buffer = std::mem::take(&mut self.buffer);
        let Ok(mut state) = self.file.lock() else {
            return;
        };

        let failed = match state.as_mut() {
            Some(state) => state.write_all(&buffer).is_err(),
            None => false,
        };

        if failed {
            // A transient error (AV lock, brief FS hiccup) used to kill file
            // logging for the rest of the session with no trace of why. Reopen
            // the sink so a hiccup only costs the one in-flight record; only
            // give up entirely if the path can no longer be opened.
            *state = new_log_rotate(&self.path);
        }
    }
}

impl Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.commit();
        Ok(())
    }
}

impl Drop for FileWriter {
    fn drop(&mut self) {
        self.commit();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;
    use tracing_subscriber::fmt::writer::BoxMakeWriter;

    fn create_test_dir() -> TestDir {
        TestDir::new("meliora-log-test")
    }

    #[derive(Clone)]
    enum TestStderrMakeWriter {
        Capture(Arc<Mutex<Vec<u8>>>),
        Fail,
    }

    impl TestStderrMakeWriter {
        fn capture(buffer: Arc<Mutex<Vec<u8>>>) -> Self {
            Self::Capture(buffer)
        }

        fn fail() -> Self {
            Self::Fail
        }
    }

    impl<'a> MakeWriter<'a> for TestStderrMakeWriter {
        type Writer = TestStderrWriter;

        fn make_writer(&'a self) -> Self::Writer {
            TestStderrWriter {
                sink: self.clone(),
                buffer: Vec::with_capacity(256),
            }
        }
    }

    struct TestStderrWriter {
        sink: TestStderrMakeWriter,
        buffer: Vec<u8>,
    }

    impl TestStderrWriter {
        fn commit(&mut self) {
            if self.buffer.is_empty() {
                return;
            }

            let buffer = std::mem::take(&mut self.buffer);
            let _ = match &self.sink {
                TestStderrMakeWriter::Capture(stderr) => match stderr.lock() {
                    Ok(mut stderr) => stderr.write_all(&buffer),
                    Err(_) => Err(io::Error::other("test stderr lock poisoned")),
                },
                TestStderrMakeWriter::Fail => Err(io::Error::other("test stderr failure")),
            };
        }
    }

    impl Write for TestStderrWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.commit();
            Ok(())
        }
    }

    impl Drop for TestStderrWriter {
        fn drop(&mut self) {
            self.commit();
        }
    }

    #[test]
    fn active_log_path_uses_standard_file_name() {
        let dir = create_test_dir();
        assert_eq!(
            super::active_log_path_in(dir.path()),
            dir.join(LOG_FILE_NAME)
        );
    }

    fn log_with_layers(stderr_writer: BoxMakeWriter, file_writer: Option<FileMakeWriter>) {
        let subscriber = tracing_subscriber::registry()
            .with(fmt::layer().with_writer(stderr_writer).without_time())
            .with(file_writer.map(|writer| {
                fmt::layer()
                    .with_writer(writer)
                    .with_ansi(false)
                    .without_time()
            }));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("integration log test");
        });
    }

    /// File sink setup failures leave stderr-only logging available.
    #[test]
    fn file_logging_failure_falls_back_to_stderr_only() {
        let dir = create_test_dir();
        let file_path = dir.join("not-a-directory");
        let log_path = active_log_path_in(dir.path());
        let invalid_log_path = file_path.join(LOG_FILE_NAME);
        fs::write(&file_path, b"x").unwrap();

        assert!(open_file_make_writer(&log_path).is_some());
        assert!(open_file_make_writer(&invalid_log_path).is_none());
    }

    /// A non-terminal stderr sink still allows the log file to be written.
    #[test]
    fn non_tty_stderr_still_writes_to_log_file() {
        let dir = create_test_dir();
        let active_path = active_log_path_in(dir.path());
        let stderr_buffer = Arc::new(Mutex::new(Vec::new()));

        log_with_layers(
            BoxMakeWriter::new(TestStderrMakeWriter::capture(stderr_buffer.clone())),
            open_file_make_writer(&active_path),
        );

        let stderr = String::from_utf8(stderr_buffer.lock().unwrap().clone()).unwrap();
        let file = fs::read_to_string(&active_path).unwrap();

        assert!(stderr.contains("integration log test"));
        assert!(file.contains("integration log test"));
    }

    #[test]
    fn failing_stderr_still_writes_to_log_file() {
        let dir = create_test_dir();
        let active_path = active_log_path_in(dir.path());

        log_with_layers(
            BoxMakeWriter::new(TestStderrMakeWriter::fail()),
            open_file_make_writer(&active_path),
        );

        let file = fs::read_to_string(&active_path).unwrap();
        assert!(file.contains("integration log test"));
    }
}
