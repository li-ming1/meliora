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

// 刻意不发出 span 生命周期事件（NEW/CLOSE）：在 tracing-subscriber 0.3 上
// 它们完全绕过每层各自的 EnvFilter，于是 gpui 那些热的 `#[instrument]`
// span（sum_tree 游标 seek，列表滚动每 tick 约一对）即便已写了
// `sum_tree=warn` 仍会灌满轮转日志。
// `symphonia_bundle_flac` 对每个实际帧数不足声明帧数的流都告警——酷狗的
// 转码文件每首都这样（2 小时会话 68 次），而 Meliora 自己已经记录 EOF 与
// 解码失败，故把该 crate 的逐流数据特性压到只有 error 级。
// EnvFilter 取更具体的前缀匹配，其余 symphonia crate 仍保持 warn。
const DEFAULT_LOG_FILTER: &str =
    "info,symphonia=warn,symphonia_bundle_flac=error,zbus=warn,sum_tree=warn";
const LOG_FILE_NAME: &str = "meliora.log";
const MAX_LOG_FILE_SIZE: usize = 1024 * 1024;
const MAX_LOG_FILES: usize = 4;

type RotatingLogFile = FileRotate<AppendTimestamp>;
type SharedLogFile = Arc<Mutex<Option<RotatingLogFile>>>;

static ACTIVE_LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static LOG_FILE: OnceLock<SharedLogFile> = OnceLock::new();

/// 初始化日志：输出到 stderr，并尽可能同时写入轮转日志文件。
pub fn init() -> anyhow::Result<()> {
    // 坏过滤器绝不能让日志初始化整体失败：windows_subsystem 下没有控制台，
    // `?` 会同时摧毁 stderr 与轮转日志两条输出——而坏的 MELIORA_LOG 恰是
    // 文档化的调试入口。回退默认过滤器，解析错误随后作为首条记录写进日志。
    let (env, malformed_filter) =
        match tracing_subscriber::EnvFilter::builder().parse(filter_value()) {
            Ok(env) => (env, None),
            Err(err) => (
                tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER),
                Some(err),
            ),
        };
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
    if let Some(err) = malformed_filter {
        tracing::warn!(
            %err,
            "invalid MELIORA_LOG/RUST_LOG filter; falling back to default filter"
        );
    }
    install_panic_hook();
    Ok(())
}

/// 安装一个 panic 钩子，把 panic 归拢进轮转日志文件（线程、位置、payload、
/// 回溯），让崩溃留下痕迹，而不是消息丢在 stderr 上悄无声息地死掉。
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".into());
        // `payload_as_str` 同时覆盖 `&'static str` 与 `String` 两种 payload；
        // 其余情况落到非文本占位符
        let payload = info
            .payload_as_str()
            .unwrap_or("<non-string panic payload>");
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

/// 刷出 stderr 与当前日志文件。
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
    ["MELIORA_LOG", "RUST_LOG"] // 优先用应用自己的变量
        .iter() // 取第一个"存在"的
        .find_map(|key| std::env::var(key).ok()) // 哪怕它是空串
        .filter(|value| !value.is_empty()) // 到这一步才判空，空值回落到默认过滤器
        .unwrap_or_else(|| DEFAULT_LOG_FILTER.to_owned())
}

fn open_file_make_writer(log_path: &Path) -> Option<FileMakeWriter> {
    fs::create_dir_all(log_path.parent()?).ok()?;
    Some(FileMakeWriter::new(
        new_log_rotate(log_path)?,
        log_path.to_path_buf(),
    ))
}

/// 构建一个全新的轮转落盘通道。file-rotate 以追加模式打开，
/// 因此重建永远不会截断磁盘上已有的内容。
fn new_log_rotate(log_path: &Path) -> Option<RotatingLogFile> {
    Some(FileRotate::new(
        log_path,
        AppendTimestamp::default(FileLimit::MaxFiles(MAX_LOG_FILES)),
        ContentLimit::BytesSurpassed(MAX_LOG_FILE_SIZE),
        Compression::None,
        None,
    ))
}

/// 为 tracing 的 stderr 层创建 stderr writer。
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

/// 先缓冲单条已格式化的日志记录，再写入 stderr。
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

/// 创建共享轮转日志文件状态的 file writer。
#[derive(Clone)]
struct FileMakeWriter {
    file: SharedLogFile,
    /// 当前日志的路径，保留它是为了在写入失败时重建落盘通道。
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

/// 先缓冲单条已格式化的日志记录，再写入共享文件。
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
            // 过去一次瞬时错误（杀毒软件占用、文件系统短暂抖动）会让本次会话
            // 余下时间的文件日志彻底失效且不留原因。改成重开落盘通道：抖动只为
            // 当前这一条在途记录买单；只有路径再也打不开时才彻底放弃。
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

    /// 文件落盘通道搭建失败时，仅 stderr 的日志仍可用。
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

    /// stderr 不是终端时，日志文件照常写入。
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
