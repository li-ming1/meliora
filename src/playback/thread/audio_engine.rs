use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, TryRecvError},
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error, info, trace_span, warn};

use crate::{
    devices::{
        format::{ChannelSpec, FormatInfo},
        mix::{ChannelMixer, MixOptions},
        resample::Resampler,
    },
    media::{
        errors::{PlaybackStartError, SeekError},
        pipeline::{AudioPipeline, DEFAULT_BUFFER_FRAMES, DecodeResult, output_frame_bound},
        traits::MediaStream,
    },
    playback::{
        dsp::{equalizer::EqualizerProcessor, spectrum::SpectrumTap},
        events::PlaybackEvent,
        thread::media_controller::{CompleteMetadata, PreparedMedia},
    },
    settings::{equalizer::EqualizerSettings, playback::PlaybackSettings},
};

use super::device_controller::{DeviceController, DeviceError};
use super::media_controller::MediaController;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineState {
    /// 未加载媒体，引擎空闲。
    Idle,
    Playing,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DrainState {
    Inactive,
    Draining { cycles: u32 },
    Drained,
}

const MAX_DRAIN_CYCLES: u32 = 1024;

/// 允许的连续重建次数上限，超过就放弃并跳到下一曲。
const MAX_REBUILD_ATTEMPTS: u32 = 8;

/// 一次 consume 失败之后、设备流再次重建之前的首个推迟窗口。WriteTimeout 每个
/// 周期都复现的设备过去会被每个周期重建一次（实测 77 s 内 95 次）；该窗口按连续
/// 失败周期翻倍，上限 [`DEVICE_RECREATE_BACKOFF_MAX`]。一次干净的 consume 只有
/// 持续超过当前窗口才算终结这一轮故障——复现之间的短暂恢复绝不能重置 `failures`，
/// 否则退避永远升不上去（实测：~0.7 s 节奏连转 8 次，全部 `failures=0`）。consume
/// 尝试本身仍每个周期执行——它的超时天然给循环定拍——被推迟的只是重建的空转。
const DEVICE_RECREATE_BACKOFF_INITIAL: std::time::Duration = std::time::Duration::from_secs(2);
const DEVICE_RECREATE_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(15);

/// 当申报的格式有误时，覆盖音频管线的默认行为。
#[derive(Debug, Clone, Default)]
struct PipelineOverrides {
    source_spec: Option<ChannelSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineCycleResult {
    Continue,
    Eof,
    /// 出现致命解码错误——应跳到下一曲。
    FatalError(String),
    /// 无事可做——不在播放态，或没有可用的流。
    NothingToDo,
}

#[derive(Debug)]
pub enum EngineError {
    NoPipeline,
    /// 取媒体信息失败。
    MediaError(String),
    DecodeError(String),
    DeviceError(String),
    InvalidState(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EngineError::NoPipeline => write!(f, "No audio pipeline configured"),
            EngineError::MediaError(s) => write!(f, "Media error: {}", s),
            EngineError::DecodeError(s) => write!(f, "Decode error: {}", s),
            EngineError::DeviceError(s) => write!(f, "Device error: {}", s),
            EngineError::InvalidState(s) => write!(f, "Invalid state: {}", s),
        }
    }
}

impl std::error::Error for EngineError {}

pub struct AudioEngine {
    media: MediaController,
    device: DeviceController,
    pipeline: Option<AudioPipeline>,
    resampler: Option<Resampler>,
    /// 源声道重采样输出与设备声道输入之间的混音器。
    mixer: Option<ChannelMixer>,
    /// 参数化均衡器，跑在混音后的设备采样率块上。
    eq: EqualizerProcessor,
    /// 夹在均衡级两端的频谱抽头，受分析器的 UI 开关门控。
    tap: SpectrumTap,
    /// 通往 UI 的事件通道，用于上报设备流的采样率。
    events_tx: UnboundedSender<PlaybackEvent>,
    /// 最近一次上报给 UI 的采样率，只在变化时发事件。
    reported_sample_rate: u32,
    state: EngineState,
    drain: DrainState,
    /// 连续出现"重建后仍没有解码产出音频"的次数；见 [`MAX_REBUILD_ATTEMPTS`]。
    rebuild_attempts: u32,
    /// 至今所有曲目需要的最大设备输入容量。管线按曲目重建，其环形缓冲/暂存/交接
    /// 缓冲都随源采样率伸缩；把它们统一按这个下限取尺寸，可让每曲的分配大小恒定，
    /// 于是堆复用同一批块而不再爬升（即 [mem] 探针的逐曲 committed 增长曲线）。
    pipeline_capacity_floor: usize,
    /// `(下次允许重建的时刻, 连续失败的 consume 周期数)`，为设备重建定拍；
    /// 见 [`DEVICE_RECREATE_BACKOFF_INITIAL`]。
    device_recreate_defer: Option<(std::time::Instant, u32)>,
    /// 为无缝衔接前进而预打开的下一曲，由 [`AudioEngine::prepare_next`]
    /// 在后台线程填入。
    prepared: Option<PreparedMedia>,
    /// 已在途的后台预备的接收端（若有）。
    prepare_rx: Option<mpsc::Receiver<PrepareOutcome>>,
    /// 交给已在途预备线程的路径（与 `prepare_rx` 同步）。
    prepare_request: Option<PathBuf>,
    /// 刚被队列变更使预备失效的路径，连同它被丢弃的时刻：`prepare_next` 在
    /// [`PREPARE_COOLDOWN`] 内跳过对同一路径的重新 spawn，于是预备窗口内的一阵
    /// 队列编辑不会每次编辑都 spawn 一整套打开。
    dropped_prepare: Option<(PathBuf, std::time::Instant)>,
    /// 上一次后台预备失败的路径，连同失败时刻：`prepare_next` 在
    /// [`PREPARE_FAILURE_COOLDOWN`] 内跳过对同一路径的重新 spawn。没有它，一个
    /// URL 永远打不开的下一曲（例如刷新也救不回来的过期签名 URL）会在它成为
    /// 当前曲之前每个播放周期被重复请求一次。以*被请求的*路径为键：刷新重试失败
    /// 时结果里的 path 是新 URL，而调用方会一直请求队列里那个过期 URL，直到
    /// 回写落地。
    failed_prepare: Option<(PathBuf, std::time::Instant)>,
    /// 已在途预备线程的取消旗标：预备失效时（队列变更、停止）置位，让被丢弃的
    /// 预备不再为一个没人会读的结果继续付提供者刷新调用与第二次完整打开。
    prepare_cancel: Option<Arc<AtomicBool>>,
}

/// 队列变更使预备失效后，同一首下一曲重新预备要推迟多久
/// （见 [`AudioEngine::dropped_prepare`]）。
const PREPARE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(2);

/// 后台预备失败后，同一路径重新预备要推迟多久。要长到让永远打不开的下一曲每个
/// 冷却只花一次请求、而非每个播放周期一次；也要短到让瞬时网络故障很快重新拿到
/// 无缝衔接覆盖。
const PREPARE_FAILURE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(30);

/// 送回播放线程的后台预备结果。
struct PrepareOutcome {
    path: PathBuf,
    result: Result<(Box<dyn MediaStream>, Option<u64>), String>,
}

/// 交给后台预备的重试钩子：打开下一曲的媒体失败时（过期的签名 URL），钩子从
/// 提供者重新取一个新路径。由播放线程构造——它拥有队列与音质设置；引擎保持对
/// 提供者无感——预备线程只见闭包，于是重试（一次阻塞的提供者调用，受调用方自己
/// 的超时约束）跑在那个专用 std 线程上，拖不住播放循环。同一个钩子会被克隆进
/// 已打开的流作为其流中刷新（见 `MediaUrlRefresh`），所以可共享的 `Arc`/`Fn`
/// 形态是必需项而非偶然。
pub struct OnlineRefresh {
    pub retry: crate::media::http_source::MediaUrlRefresh,
}

/// 一次已完成的后台预备，经 [`AudioEngine::poll_prepared`] 上报，让播放线程能把
/// 靠刷新救回的路径回写到当初请求预备的那个队列条目。
pub struct PrepareReport {
    /// 请求预备时给出的路径。
    pub requested: PathBuf,
    /// 已预备流的最终路径：on-open-error 刷新产出新 URL 时与 `requested` 不同。
    pub path: PathBuf,
    /// 是否打开了流（直接打开，或经刷新重试之后）。
    pub succeeded: bool,
}

impl AudioEngine {
    pub fn new(events_tx: UnboundedSender<PlaybackEvent>, tap: SpectrumTap) -> Self {
        Self {
            media: MediaController::new(),
            device: DeviceController::new(),
            pipeline: None,
            resampler: None,
            mixer: None,
            // 建流时会重新同步到真实的流格式
            eq: EqualizerProcessor::new(48_000.0, 2),
            tap,
            events_tx,
            reported_sample_rate: 0,
            state: EngineState::Idle,
            drain: DrainState::Inactive,
            rebuild_attempts: 0,
            pipeline_capacity_floor: 0,
            device_recreate_defer: None,
            prepared: None,
            prepare_rx: None,
            prepare_request: None,
            dropped_prepare: None,
            failed_prepare: None,
            prepare_cancel: None,
        }
    }

    /// 初始化音频引擎的各提供者并创建初始设备流。
    ///
    /// 启动时调用一次。
    pub fn initialize(&mut self) -> Result<(), EngineError> {
        self.device.initialize_provider();

        if let Err(e) = self.device.create_stream(None) {
            error!("Failed to create initial stream: {:?}", e);
            return Err(EngineError::DeviceError(format!(
                "Failed to create initial stream: {:?}",
                e
            )));
        }

        self.sync_eq_format();

        Ok(())
    }

    pub fn state(&self) -> EngineState {
        self.state
    }

    /// 下一曲的流是否已预备或正在预备。为逐周期的 `maybe_prepare_next` 路径准备的
    /// 廉价检查：下一流一旦在途，调用方就能整个跳过队列锁与 `PathBuf` 克隆。
    /// 队列变更会清空 `prepared`/`prepare_rx`，所以有值就绝不会对应过期路径。
    pub fn is_next_prepared(&self) -> bool {
        self.prepare_rx.is_some() || self.prepared.is_some()
    }

    /// `prepare_rejected` 与 `prepare_next` 共用的准入检查：已预备/已在途，
    /// 或处于丢弃/失败冷却之内。
    fn prepare_blocked(&self, path: &Path) -> bool {
        if self.prepare_rx.is_some() || self.prepared.as_ref().is_some_and(|p| p.path == path) {
            return true;
        }
        // 这曲的预备不久前刚被队列变更失效：推迟到编辑安定下来，而不是每次编辑
        // 都重开一次流（连接+探测，或一次白费的 HTTP 请求）。
        if let Some((dropped, at)) = &self.dropped_prepare
            && dropped == path
            && at.elapsed() < PREPARE_COOLDOWN
        {
            return true;
        }
        // 该路径最近一次预备失败（URL 已死、文件读不了）：退避，而不是每个周期
        // 都付一次完整的打开尝试。
        if let Some((failed, at)) = &self.failed_prepare
            && failed == path
            && at.elapsed() < PREPARE_FAILURE_COOLDOWN
        {
            return true;
        }
        false
    }

    /// `prepare_next` 此刻是否会拒绝 `path`。让调用方在付费之前先跳过构造刷新钩子
    /// ——在 30 s 失败冷却期间，那意味着每个解码周期一次队列锁、一次注册表扫描
    /// 和一次闭包分配。
    pub fn prepare_rejected(&self, path: &Path) -> bool {
        self.prepare_blocked(path)
    }

    /// 在后台线程预打开 `path` 的媒体流，让曲终时的换曲不必等待连接+探测（在线源）。
    /// `on_open_error` 给预备装上一枚一次性的 URL 刷新重试：打开失败时（过期的签名
    /// URL），钩子取一个新路径并对它再跑一次打开——全程在预备线程上，因此自救发生
    /// 在预备提前窗口之内，而不是阻塞切换本身。`None`（本地文件、无法识别的流）
    /// 保持原有行为不变。该曲目已预备或正在预备时本调用被忽略。
    pub fn prepare_next(&mut self, path: &Path, on_open_error: Option<OnlineRefresh>) {
        if self.prepare_blocked(path) {
            return;
        }
        // 为另一首曲目预备的流按定义已经过期。
        self.prepared = None;

        let (tx, rx) = mpsc::channel();
        self.prepare_rx = Some(rx);
        let path = path.to_path_buf();
        self.prepare_request = Some(path.clone());
        // 在途中失效的预备（队列变更、停止）绝不能为一个没人会读的结果继续付
        // 提供者刷新调用与第二次完整打开；首次打开无法被打断，所以旗标只在剩下
        // 的两个决策点检查。
        let cancel = Arc::new(AtomicBool::new(false));
        self.prepare_cancel = Some(Arc::clone(&cancel));
        // 同一个钩子兼作已打开流的流中刷新。
        let source_refresh = on_open_error.as_ref().map(|hook| Arc::clone(&hook.retry));
        let spawned = std::thread::Builder::new()
            .name("media-prepare".into())
            .spawn(move || {
                // 过期的签名 URL：首次打开失败，钩子重新取一个新路径，并对它
                // 重试打开一次。结果里上报最终路径，好让调用方把它回写给队列。
                let (path, result) = match MediaController::open_stream(&path, source_refresh) {
                    Ok(stream) => (path, Ok(stream)),
                    Err(err) => {
                        if cancel.load(Ordering::Relaxed) {
                            return;
                        }
                        match on_open_error {
                            Some(hook) => match (hook.retry)() {
                                Some(fresh) => {
                                    if cancel.load(Ordering::Relaxed) {
                                        return;
                                    }
                                    let result = MediaController::open_stream(
                                        &fresh,
                                        Some(Arc::clone(&hook.retry)),
                                    )
                                    .map_err(|e| e.to_string());
                                    (fresh, result)
                                }
                                None => (path, Err(err.to_string())),
                            },
                            None => (path, Err(err.to_string())),
                        }
                    }
                };
                let _ = tx.send(PrepareOutcome { path, result });
            });
        if spawned.is_err() {
            // 没有预备线程；切换时退回普通打开路径。
            self.prepare_rx = None;
            self.prepare_request = None;
        }
    }

    /// 收取已完成的后台预备。每个播放周期调用一次；每个完成的预备（成功或失败）
    /// 都返回一份报告，好让调用方把靠刷新救回的路径回写给队列。
    pub fn poll_prepared(&mut self) -> Option<PrepareReport> {
        let Some(rx) = &self.prepare_rx else {
            return None;
        };
        match rx.try_recv() {
            Ok(outcome) => {
                self.prepare_rx = None;
                self.prepare_cancel = None;
                // 失败退避与刷新回写都以被请求的路径为键：带刷新重试时结果里的
                // path 是*新的* URL，而 `prepare_next` 的几道守卫（以及要修复的
                // 队列条目）比对的是——并且还会是——被请求的那个路径。
                let requested = self
                    .prepare_request
                    .take()
                    .unwrap_or_else(|| outcome.path.clone());
                match outcome.result {
                    Ok((stream, duration_ms)) => {
                        info!(
                            path = %outcome.path.display(),
                            "next track pre-opened for gapless advance"
                        );
                        self.prepared = Some(PreparedMedia {
                            path: outcome.path.clone(),
                            stream,
                            duration_ms,
                        });
                        Some(PrepareReport {
                            requested,
                            path: outcome.path,
                            succeeded: true,
                        })
                    }
                    Err(e) => {
                        // 切换时会重跑普通打开路径（连同它的 URL 刷新重试），
                        // 日志也在那里打。
                        warn!("pre-opening next track failed: {e}");
                        // 给这条路径记退避：不留记录，下一个周期就会再次请求
                        // 同一个打不开的 URL（它还在 next 期间每个周期一次真实
                        // HTTP 往返）。
                        self.failed_prepare = Some((requested.clone(), std::time::Instant::now()));
                        Some(PrepareReport {
                            requested,
                            path: outcome.path,
                            succeeded: false,
                        })
                    }
                }
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.prepare_rx = None;
                self.prepare_request = None;
                self.prepare_cancel = None;
                None
            }
        }
    }

    /// 丢弃所有预打开的媒体；在队列变化（下一曲可能已不是预备的那首）或播放
    /// 停止时调用。
    pub fn drop_prepared(&mut self) {
        // 仍在途的预备必须停止为一个没人会读的结果继续付提供者刷新调用与第二次
        // 完整打开。
        if let Some(cancel) = &self.prepare_cancel {
            cancel.store(true, Ordering::Relaxed);
        }
        self.prepare_cancel = None;
        // 记住刚刚失效的是什么，好让 `prepare_next` 在队列编辑安定期间对同一路径
        // 的重新 spawn 节流。
        let dropped = self
            .prepare_request
            .take()
            .or_else(|| self.prepared.as_ref().map(|prepared| prepared.path.clone()));
        if let Some(path) = dropped {
            self.dropped_prepare = Some((path, std::time::Instant::now()));
        }
        self.prepared = None;
        self.prepare_rx = None;
    }

    /// 仅当队列的下一条条目不再与（正在）预备的内容匹配时才丢弃预打开的媒体。
    /// 不改动下一曲的那些编辑——元数据刷新、别处重排、无关新增——否则会让一个
    /// 有效的预打开作废，并在 [`PREPARE_COOLDOWN`] 窗口之后重付一次完整打开。
    /// `next` 匹配*被请求的*路径时保留在途的预备；匹配*已上报的*路径（已回写的、
    /// 靠刷新救回的 URL）时保留已完成的预备。
    pub fn drop_prepared_unless_next(&mut self, next: Option<&Path>) {
        let matches = |prepared_path: &Path| next == Some(prepared_path);
        let stale = match (&self.prepare_request, &self.prepared) {
            (Some(requested), _) => !matches(requested),
            (None, Some(prepared)) => !matches(&prepared.path),
            (None, None) => return,
        };
        if stale {
            self.drop_prepared();
        }
    }

    pub fn open(
        &mut self,
        path: &Path,
        preserve_resampler: bool,
        refresh: Option<crate::media::http_source::MediaUrlRefresh>,
    ) -> Result<Option<u64>, PlaybackStartError> {
        debug!("AudioEngine: Opening track '{}'", path.display());

        self.drain = DrainState::Inactive;
        self.rebuild_attempts = 0;

        if !preserve_resampler {
            self.reset_resampler();
        }

        let mut recreation_required = false;

        if self.state != EngineState::Playing
            && self.device.has_stream()
            && let Err(err) = self.device.reset()
        {
            warn!("Failed to reset device, forcing recreation: {:?}", err);
            recreation_required = true;
        }

        if self.device.has_stream()
            && let Err(err) = self.device.play()
        {
            warn!("Failed to play device, forcing recreation: {:?}", err);
            recreation_required = true;
        }

        // 保留重采样器以供无缝复用；混音器按每曲的声道布局重建。
        self.pipeline = None;
        self.mixer = None;

        // 无缝衔接快路径：下一曲已在后台预打开（连接 + 探测 + 启动都已在线下完成）。
        let duration_ms = match self.prepared.take() {
            Some(prepared) if prepared.path == path => self.media.attach_prepared(prepared)?,
            Some(stale) => {
                // 只有队列变更能改写目标；它的 QueueUpdated 已经丢弃了预备的流，
                // 所以这里出现陈旧且不匹配的流很罕见——直接丢掉即可。
                drop(stale);
                self.media.open(path, refresh.clone())?
            }
            None => self.media.open(path, refresh.clone())?,
        };

        if recreation_required {
            if let Err(e) = self.device.create_stream(None) {
                error!("Failed to recreate stream: {:?}", e);
                return Err(PlaybackStartError::StreamError(format!(
                    "Failed to recreate stream: {:?}",
                    e
                )));
            }
            self.sync_eq_format();

            if let Err(e) = self.device.play() {
                error!("Device was recreated and we still can't play: {:?}", e);
                self.stop();
                return Err(PlaybackStartError::StreamError(format!(
                    "Device was recreated but playback could not start: {e:?}"
                )));
            }
        }

        self.state = EngineState::Playing;

        if let Some(device_format) = self.device.current_format().cloned() {
            if let Err(e) = self.setup_pipeline(&device_format, PipelineOverrides::default()) {
                self.stop();
                return Err(PlaybackStartError::MediaError(format!(
                    "Failed to set up audio pipeline: {e}"
                )));
            }

            // 解码第一个数据包；若真实格式与申报格式不符，就尝试重建管线。
            let mut attempts = 0;
            loop {
                match self.process_decode_resample() {
                    Ok(DecodeStepResult::Continue) | Ok(DecodeStepResult::Eof) => break,
                    Ok(DecodeStepResult::Rebuild(overrides)) => {
                        attempts += 1;
                        if attempts > MAX_REBUILD_ATTEMPTS {
                            self.stop();
                            return Err(PlaybackStartError::MediaError(
                                "audio format kept changing while priming the pipeline".to_string(),
                            ));
                        }
                        if let Err(e) = self.rebuild_pipeline(overrides) {
                            self.stop();
                            return Err(PlaybackStartError::MediaError(format!(
                                "Failed to rebuild audio pipeline: {e}"
                            )));
                        }
                    }
                    Ok(DecodeStepResult::FatalError(msg)) => {
                        self.stop();
                        return Err(PlaybackStartError::MediaError(msg));
                    }
                    Err(e) => {
                        self.stop();
                        return Err(PlaybackStartError::MediaError(e.to_string()));
                    }
                }
            }
        }

        Ok(duration_ms)
    }

    /// 恢复播放。
    ///
    /// 处于暂停时，这会恢复设备流。
    /// 空闲且没有媒体时返回错误。
    pub fn play(&mut self) -> Result<(), EngineError> {
        match self.state {
            EngineState::Playing => Ok(()),
            EngineState::Paused => {
                // 恢复时走拆掉并重建流，而不是对一条久停的流直接调 play()：某些
                // 后端（Windows WASAPI）在暂停后会静默停止产出音频，直到流被重建。
                // 跳转定位路径本就会重建，这就是拖动进度条能把哑掉的播放"叫醒"的原因。
                if self.device.has_stream() {
                    if let Err(err) = self.device.reset() {
                        warn!(
                            "Failed to reset stream on resume, recreating device instead... {:?}",
                            err
                        );
                        self.recreate_stream().map_err(|e| {
                            EngineError::DeviceError(format!("Failed to recreate stream: {:?}", e))
                        })?;
                    }
                    self.eq.reset();

                    if let Err(err) = self.device.play() {
                        warn!(
                            "Failed to restart playback after resume reset, recreating device and retrying... {:?}",
                            err
                        );
                        self.recreate_stream().map_err(|e| {
                            EngineError::DeviceError(format!("Failed to recreate stream: {:?}", e))
                        })?;

                        if let Err(e) = self.device.play() {
                            return Err(EngineError::DeviceError(format!(
                                "Failed to start playback after recreation on resume: {:?}",
                                e
                            )));
                        }
                    }
                }

                self.state = EngineState::Playing;
                Ok(())
            }
            EngineState::Idle => Err(EngineError::InvalidState(
                "Cannot play: no media loaded".to_string(),
            )),
        }
    }

    /// 暂停播放。
    pub fn pause(&mut self) -> Result<(), EngineError> {
        if self.state != EngineState::Playing {
            return Ok(());
        }

        if let Err(e) = self.device.pause() {
            warn!("Failed to pause device: {:?}", e);
        }

        self.state = EngineState::Paused;
        Ok(())
    }

    /// 推进设备的延后工作（目前即完成异步暂停淡出），主循环每轮一次。
    pub fn poll(&mut self) {
        if let Err(e) = self.device.poll() {
            warn!("device poll failed: {:?}", e);
        }
    }

    /// 是否有延后的设备工作待处理、因而 `poll` 必须尽快运行（目前即异步暂停淡出）。
    /// 驱动播放线程的空闲挂起。
    pub fn has_deferred_work(&self) -> bool {
        self.device.has_deferred_work()
    }

    /// 停止播放并清空全部状态。
    pub fn stop(&mut self) {
        // 把重采样器的尾巴冲刷到设备，避免最后一曲被截断
        if self.drain == DrainState::Drained && self.state == EngineState::Playing {
            self.flush_tail_to_device();
        }

        self.media.close();
        self.clear_pipeline();
        self.state = EngineState::Idle;
    }

    /// 跳转到指定时间（单位：秒）。
    pub fn seek(&mut self, time: f64) -> Result<(), SeekError> {
        let result = self.media.seek(time);
        if result.is_ok() {
            // 从 EOF 区域跳回来即恢复正常解码
            self.drain = DrainState::Inactive;
            self.rebuild_attempts = 0;

            if self.state == EngineState::Playing {
                self.flush_for_seek();
            }
        }
        result
    }

    /// 跳转定位前把一切缓冲中的东西丢掉：设备自己的队列、管线的两条环形缓冲，
    /// 以及重采样器/混音器/均衡器状态，好让跳转后的音频干净且立即播出。
    fn flush_for_seek(&mut self) {
        if self.device.has_stream() {
            if let Err(err) = self.device.reset() {
                warn!("Failed to reset device on seek: {:?}", err);
            } else if let Err(err) = self.device.play() {
                warn!("Failed to resume device after seek reset: {:?}", err);
            }
        }

        if let Some(resampler) = &mut self.resampler {
            resampler.reset();
        }
        if let Some(mixer) = &mut self.mixer {
            mixer.reset();
        }
        self.eq.reset();
        if let Some(pipeline) = &mut self.pipeline {
            pipeline.flush_buffers();
        }
    }

    /// 设置播放音量（0.0 到 1.0）。
    pub fn set_volume(&mut self, volume: f64) -> Result<(), EngineError> {
        self.device
            .set_volume(volume)
            .map_err(|e| EngineError::DeviceError(format!("Failed to set volume: {:?}", e)))
    }

    /// 设置 ReplayGain 倍率（线性）。
    pub fn set_replaygain(&mut self, gain: f64) -> Result<(), EngineError> {
        self.device
            .set_replaygain(gain)
            .map_err(|e| EngineError::DeviceError(format!("Failed to set RG: {:?}", e)))
    }

    /// 取当前播放位置（单位：毫秒）。
    pub fn position_ms(&self) -> Option<u64> {
        self.media.position_ms().ok()
    }

    /// 取当前已加载曲目的路径（若有）。
    pub fn current_path(&self) -> Option<&Path> {
        self.media.current_path()
    }

    /// 检查元数据更新，有更新就返回。
    pub fn check_metadata_update(&mut self) -> Option<CompleteMetadata> {
        self.media.check_metadata_update()
    }

    /// 把均衡器同步到当前流格式。每次（重）建流之后都要调用。
    fn sync_eq_format(&mut self) {
        let Some(format) = self.device.current_format() else {
            return;
        };
        if self.reported_sample_rate != format.sample_rate {
            self.reported_sample_rate = format.sample_rate;
            let _ = self
                .events_tx
                .send(PlaybackEvent::SampleRateChanged(format.sample_rate));
        }
        if format.sample_rate > 0 {
            self.eq.set_sample_rate(f64::from(format.sample_rate));
        }
        self.eq
            .set_channel_count(format.channels.to_layout().count().max(1));
    }

    /// 用当前声道格式重建设备流并把均衡器重新同步到它。返回原始设备错误，
    /// 好让每个调用方按自己的措辞上报。
    fn recreate_stream(&mut self) -> Result<(), DeviceError> {
        let channels = self.device.current_format().map(|f| f.channels.clone());
        self.device.create_stream(channels).map(|_| ())?;
        self.sync_eq_format();
        Ok(())
    }

    /// 设备流缺失时的自愈。`open`/`process_cycle` 的所有 reset/play/重建都以
    /// `has_stream` 为前置，缺了这条路径就永久静音（每曲报 Ok+Playing、空转
    /// 到跳歌）。退避与 WriteTimeout 共用，避免设备长期不可用时每周期重试。
    fn retry_missing_stream(&mut self) {
        let now = std::time::Instant::now();
        if let Some((next_recreate, _)) = self.device_recreate_defer
            && now < next_recreate
        {
            return;
        }
        let failures = self
            .device_recreate_defer
            .map(|(_, failures)| failures + 1)
            .unwrap_or(0);
        let defer = DEVICE_RECREATE_BACKOFF_INITIAL
            .saturating_mul(1_u32 << failures.min(4))
            .min(DEVICE_RECREATE_BACKOFF_MAX);
        self.device_recreate_defer = Some((now + defer, failures));
        warn!(
            failures,
            ?defer,
            "audio device has no stream; retrying stream creation"
        );
        match self.recreate_stream() {
            Ok(()) => match self.device.play() {
                Ok(()) => {
                    self.device_recreate_defer = None;
                    info!("audio stream recreated after missing stream");
                }
                Err(e) => error!("Failed to start playback after stream recreation: {:?}", e),
            },
            Err(e) => error!("Failed to recreate missing stream: {:?}", e),
        }
    }

    /// 更新影响播放的设置。
    pub fn update_settings(&mut self, settings: &PlaybackSettings) {
        self.eq.set_config(&settings.equalizer);
    }

    /// 实时应用新的均衡器设置。
    pub fn set_equalizer(&mut self, settings: &EqualizerSettings) {
        self.eq.set_config(settings);
    }

    /// 开启或关闭媒体流上的循环感知解码。
    pub fn set_looping(&mut self, enabled: bool) {
        self.media.set_looping(enabled);
    }

    /// 处理音频管线的一个周期。
    ///
    /// 返回的结果指示应当继续、处理 EOF，还是处理错误。
    pub fn process_cycle(&mut self) -> EngineCycleResult {
        if self.state != EngineState::Playing {
            return EngineCycleResult::NothingToDo;
        }

        if !self.device.has_stream() {
            // 初始化失败或一次重建失败后没有流：不重试就会永久静音，每首空转到
            // 跳过。退避复用 WriteTimeout 的阶梯，设备长期不可用时不至于每周期重试。
            self.retry_missing_stream();
            return EngineCycleResult::NothingToDo;
        }

        if !self.media.has_stream() {
            return EngineCycleResult::NothingToDo;
        }

        if matches!(self.drain, DrainState::Draining { .. }) {
            return self.drain_cycle();
        }

        if self.pipeline.is_none() {
            let device_format = match self.device.current_format() {
                Some(fmt) => fmt.clone(),
                None => {
                    error!("No device format available");
                    return EngineCycleResult::NothingToDo;
                }
            };

            if let Err(e) = self.setup_pipeline(&device_format, PipelineOverrides::default()) {
                error!("Failed to setup audio pipeline: {:?}", e);
                return EngineCycleResult::NothingToDo;
            }
        }

        // 设备能接收时才解码，否则我们会丢掉音频
        let frame_duration = self
            .media
            .frame_duration()
            .map(|d| d as usize)
            .unwrap_or(DEFAULT_BUFFER_FRAMES);
        let throttle = self
            .pipeline
            .as_ref()
            .is_some_and(|p| !p.can_accept_decode(frame_duration));
        if throttle {
            return self.consume_to_device();
        }

        let result = match self.process_decode_resample() {
            Ok(result) => result,
            Err(e) => {
                error!("Audio engine error: {:?}", e);
                return EngineCycleResult::NothingToDo;
            }
        };

        match result {
            DecodeStepResult::Eof => {
                info!("EOF, draining pipeline to the device");
                self.drain = DrainState::Draining { cycles: 0 };
                return self.drain_cycle();
            }
            DecodeStepResult::FatalError(msg) => {
                error!("Fatal error in audio engine: {msg}");
                return EngineCycleResult::FatalError(msg);
            }
            DecodeStepResult::Rebuild(overrides) => {
                self.rebuild_attempts += 1;
                if self.rebuild_attempts > MAX_REBUILD_ATTEMPTS {
                    error!(
                        "pipeline rebuilt {} times without progress; skipping track",
                        self.rebuild_attempts
                    );
                    return EngineCycleResult::FatalError(
                        "pipeline rebuild loop (format kept changing)".to_string(),
                    );
                }
                if let Err(e) = self.rebuild_pipeline(overrides) {
                    error!("Failed to rebuild audio pipeline: {:?}", e);
                    return EngineCycleResult::NothingToDo;
                }

                return EngineCycleResult::Continue;
            }
            DecodeStepResult::Continue => {
                self.rebuild_attempts = 0;
            }
        }

        self.consume_to_device()
    }

    /// 尽可能多地送往设备能接收的量，然后等待它排空。若这连续发生太多次
    /// （超过 MAX_DRAIN_CYCLES）我们就直接放弃。
    fn drain_cycle(&mut self) -> EngineCycleResult {
        if let DrainState::Draining { cycles } = &mut self.drain {
            *cycles += 1;
            if *cycles > MAX_DRAIN_CYCLES {
                warn!("pipeline drain did not finish within {MAX_DRAIN_CYCLES} cycles");
                warn!("reporting EOF with audio still buffered");
                self.drain = DrainState::Drained;
                return EngineCycleResult::Eof;
            }
        }

        if let Some(p) = &mut self.pipeline {
            Self::resample_and_route(
                p,
                self.resampler.as_mut(),
                &mut self.mixer,
                &mut self.eq,
                &mut self.tap,
            );
        }

        match self.consume_to_device() {
            EngineCycleResult::Continue => {}
            other => return other,
        }

        let empty = match &self.pipeline {
            Some(p) => {
                p.resampler_input.potentially_available() == 0
                    && p.device_input.potentially_available() == 0
            }
            None => true,
        };

        if empty {
            self.drain = DrainState::Drained;
            info!("EOF, track finished");
            return EngineCycleResult::Eof;
        }

        EngineCycleResult::Continue
    }

    /// 从管线取样本送往设备
    fn consume_to_device(&mut self) -> EngineCycleResult {
        let Some(pipeline) = &mut self.pipeline else {
            return EngineCycleResult::NothingToDo;
        };

        let consume_result = self.device.consume_from(&mut pipeline.device_input);

        if let Err(err) = consume_result {
            // 只在错误路径上构造 Span：每个周期都进入一次的话，只要 TRACE 日志
            // 开着就会每周期分配一个 `Span`（Arc），而这里每个播放周期都跑
            // （性能准则 §8）。下面四条诊断都保住各自的 span 父级。
            let s = trace_span!("consume_from").entered();
            // WriteTimeout 每个周期都复现的设备，靠重建永远收敛不了；用一个逐步
            // 变大的推迟窗口给重建定拍，而不是每周期翻弄这条流。上面那次 consume
            // 尝试仍每周期执行，所以它的超时继续给循环定拍，而持续到窗口之后的
            // 一次干净 consume 会清掉退避。
            let now = std::time::Instant::now();
            if let Some((next_recreate, _)) = self.device_recreate_defer
                && now < next_recreate
            {
                // 推迟窗口之内：这一轮故障已经记过日志、重建也已推迟——再把同一个
                // WriteTimeout 记一遍，就会按 consume 超时的节奏重复（~4 Hz，
                // 设备 outage 期间有时持续几分钟，性能准则 §29）。
                return EngineCycleResult::NothingToDo;
            }
            warn!(parent: &s, ?err, "Failed to consume from pipeline: {err}");

            let failures = self
                .device_recreate_defer
                .map(|(_, failures)| failures + 1)
                .unwrap_or(0);
            let defer = DEVICE_RECREATE_BACKOFF_INITIAL
                .saturating_mul(1_u32 << failures.min(4))
                .min(DEVICE_RECREATE_BACKOFF_MAX);
            self.device_recreate_defer = Some((now + defer, failures));
            warn!(parent: &s, failures, ?defer, "Recreating device and retrying...");

            if let Err(e) = self.recreate_stream() {
                error!(parent: &s, "Failed to recreate stream: {:?}", e);
                return EngineCycleResult::NothingToDo;
            }

            let Some(pipeline) = &mut self.pipeline else {
                return EngineCycleResult::NothingToDo;
            };

            let retry_result = self.device.consume_from(&mut pipeline.device_input);

            if let Err(err) = retry_result {
                error!(parent: &s, ?err, "Failed to consume after recreation: {err}");
                error!(
                    "This likely indicates a problem with the audio device or driver\n\
                    (or an underlying issue in the used DeviceProvider)\n\
                    Please check your audio setup and try again."
                );

                return EngineCycleResult::FatalError(format!(
                    "audio device unusable after recreation: {err}"
                ));
            }
        } else if let Some((next_recreate, _)) = self.device_recreate_defer {
            // 干净的 consume。不要在第一个健康周期就宽恕失败历史：每次重建后只
            // 短暂排空一下的驱动会把 `failures` 一次次清零，于是退避永远升不上去。
            // 只有设备健康地越过当前推迟窗口，才算终结这一轮故障；在此之前，再次
            // 出现的 WriteTimeout 会落在窗口之内，由上面那道推迟重建的门来定拍，
            // 而不是翻弄这条流。
            if std::time::Instant::now() >= next_recreate {
                self.device_recreate_defer = None;
            }
        }

        EngineCycleResult::Continue
    }

    /// 为新曲目搭建音频管线。`overrides` 顶替媒体流申报错了的参数（重建路径）。
    /// 否则就按媒体流申报的格式来搭建管线。
    fn setup_pipeline(
        &mut self,
        device_format: &FormatInfo,
        overrides: PipelineOverrides,
    ) -> Result<(), EngineError> {
        let source_spec = match overrides.source_spec {
            Some(spec) => spec,
            None => self
                .media
                .channels()
                .map_err(|e| EngineError::MediaError(format!("Failed to get channels: {:?}", e)))?,
        };

        let source_layout = source_spec.to_layout();
        let device_layout = device_format.channels.to_layout();

        let source_channel_count = source_layout.count().max(1);
        let device_channel_count = device_layout.count().max(1);
        let channels_match = source_layout == device_layout;

        let source_rate = self
            .media
            .sample_rate()
            .unwrap_or(device_format.sample_rate);

        // 解码环形缓冲必须装得下一个最坏情况的解码包：块大小高于默认值的流
        // （例如大块 24-bit/96 kHz FLAC）否则会每个包都丢音频，因为同线程的重采样器
        // 无法在写入中途排空，而非阻塞的包写入会丢掉放不下的一部分。普通流保持
        // 默认尺寸不变。
        let buffer_frames = self
            .media
            .frame_duration()
            .map(|d| d as usize)
            .unwrap_or(DEFAULT_BUFFER_FRAMES)
            .max(DEFAULT_BUFFER_FRAMES);

        let pipeline = AudioPipeline::new(
            source_channel_count,
            device_channel_count,
            source_rate,
            device_format.sample_rate,
            buffer_frames,
            self.pipeline_capacity_floor,
        );
        // 把下限抬到本曲实际需要的容量，之后的曲目就按迄今见过的最大尺寸分配，
        // 而不是又换一个新尺寸。
        self.pipeline_capacity_floor = self.pipeline_capacity_floor.max(output_frame_bound(
            source_rate,
            device_format.sample_rate,
            buffer_frames,
        ));

        if channels_match {
            self.mixer = None;
        } else {
            let mut mixer = ChannelMixer::new(source_layout, device_layout, MixOptions::default());

            if mixer.needs_mixing() || source_channel_count != device_channel_count {
                mixer.ensure_output_capacity(output_frame_bound(
                    source_rate,
                    device_format.sample_rate,
                    buffer_frames,
                ));
                self.mixer = Some(mixer);
            } else {
                self.mixer = None;
            }
        }

        self.pipeline = Some(pipeline);

        Ok(())
    }

    fn clear_pipeline(&mut self) {
        self.pipeline = None;
        self.resampler = None;
        self.mixer = None;
        self.eq.reset();
        self.drain = DrainState::Inactive;
        self.rebuild_attempts = 0;
    }

    /// 曲中遇到声道/采样率/声道数不匹配后重建管线。丢掉所有缓冲中的音频，
    /// 连重采样器一起。
    fn rebuild_pipeline(&mut self, overrides: PipelineOverrides) -> Result<(), EngineError> {
        let device_format =
            self.device.current_format().cloned().ok_or_else(|| {
                EngineError::DeviceError("no device format for rebuild".to_string())
            })?;

        info!(
            "Rebuilding audio pipeline (channels={:?})",
            overrides.source_spec
        );

        self.pipeline = None;
        self.mixer = None;
        if let Some(resampler) = &mut self.resampler {
            resampler.reset();
        }

        self.setup_pipeline(&device_format, overrides)
    }

    fn reset_resampler(&mut self) {
        if let Some(resampler) = &mut self.resampler {
            resampler.reset();
        }
        if let Some(mixer) = &mut self.mixer {
            mixer.reset();
        }
        if let Some(p) = &mut self.pipeline {
            p.clear_resampler_output();
        }
    }

    /// 处理解码与重采样这两步。
    fn process_decode_resample(&mut self) -> Result<DecodeStepResult, EngineError> {
        let p = self.pipeline.as_mut().ok_or(EngineError::NoPipeline)?;

        let decode_result = match self.media.decode_into(&mut p.decoder_output) {
            Ok(result) => result,
            Err(e) => {
                return Self::handle_decode_error(e);
            }
        };

        match decode_result {
            DecodeResult::Eof => {
                info!("EOF from decode_into");
                return Ok(DecodeStepResult::Eof);
            }
            DecodeResult::Decoded { rate, .. } => {
                if rate == p.target_rate {
                    if let Some(mut old) = self.resampler.take() {
                        info!("Source rate now matches device; dropping resampler");
                        Self::flush_old_resampler(
                            &mut old,
                            p,
                            &mut self.mixer,
                            &mut self.eq,
                            &mut self.tap,
                        );
                    }
                } else {
                    let duration = self.media.frame_duration().unwrap_or(1024);
                    // 刻意不以 `duration` 为键：即便采样率/声道数完全相同，解码器
                    // 块大小也逐文件不同，以它为键几乎每次换曲都会重建整个重采样器
                    // （FFT plan + 声道缓冲）。当初构建这个重采样器用的固定块大小
                    // 一直有效；下面那些交接缓冲按每曲动态定尺寸。
                    let needs_new_resampler = match &self.resampler {
                        Some(resampler) => {
                            !resampler.matches_params(rate, p.target_rate, p.source_channel_count)
                        }
                        None => true,
                    };

                    if needs_new_resampler {
                        if let Some(mut old) = self.resampler.take() {
                            info!(
                                "Stream parameters changed (rate {} -> {}, \
                                 duration {}); flushing and rebuilding resampler",
                                p.source_rate, rate, duration
                            );
                            Self::flush_old_resampler(
                                &mut old,
                                p,
                                &mut self.mixer,
                                &mut self.eq,
                                &mut self.tap,
                            );
                        }
                        let resampler = match Resampler::new(
                            rate,
                            p.target_rate,
                            duration,
                            p.source_channel_count as u16,
                        ) {
                            Ok(resampler) => resampler,
                            Err(e) => {
                                error!(
                                    "Failed to build resampler (rate {rate} -> {}, \
                                     duration {duration}, channels {}): {e}",
                                    p.target_rate, p.source_channel_count
                                );
                                return Ok(DecodeStepResult::FatalError(e));
                            }
                        };
                        // 一个周期可能推过重采样器好几个块，所以要确保交接缓冲能
                        // 吸收最坏情况，免日后再重新分配
                        let blocks = p.decoder_buffer_frames.div_ceil(duration.max(1) as usize);
                        p.ensure_resampler_output_capacity(blocks * resampler.output_frames_max());
                        self.resampler = Some(resampler);
                    }
                }

                p.source_rate = rate;
            }
        }

        // 每个周期最多读满一整圈解码缓冲：大包是整体写入的，就必须整体排空，
        // 否则残留会蚕食下一个包写入所需的空闲空间。对普通流而言这等于旧的默认值。
        Self::resample_and_route(
            p,
            self.resampler.as_mut(),
            &mut self.mixer,
            &mut self.eq,
            &mut self.tap,
        );

        Ok(DecodeStepResult::Continue)
    }

    /// 把解码缓冲里的帧推过重采样器（未启用重采样时原样直通），
    /// 并把结果交给设备级。
    fn resample_and_route(
        p: &mut AudioPipeline,
        resampler: Option<&mut Resampler>,
        mixer: &mut Option<ChannelMixer>,
        eq: &mut EqualizerProcessor,
        tap: &mut SpectrumTap,
    ) {
        match resampler {
            Some(resampler) => {
                resampler.process_into(
                    &mut p.resampler_input,
                    &mut p.resampler_output,
                    p.decoder_buffer_frames,
                );
            }
            None => {
                Resampler::passthrough_direct(
                    &mut p.resampler_input,
                    &mut p.resampler_output,
                    p.decoder_buffer_frames,
                );
            }
        }

        Self::route_resampler_output(p, mixer, eq, tap);
    }

    fn route_resampler_output(
        p: &mut AudioPipeline,
        mixer: &mut Option<ChannelMixer>,
        eq: &mut EqualizerProcessor,
        tap: &mut SpectrumTap,
    ) {
        let result = if let Some(mixer) = mixer {
            let frames = mixer.mix(&p.resampler_output);
            Self::eq_with_tap(eq, tap, mixer.output_planes_mut(), frames);
            Self::passthrough_to_device(
                mixer.output_planes(),
                frames,
                &mut p.device_input_producers,
            )
        } else if p.source_channel_count == p.device_channel_count {
            let frames = p.resampler_output.iter().map(Vec::len).min().unwrap_or(0);
            Self::eq_with_tap(eq, tap, &mut p.resampler_output, frames);
            Self::passthrough_to_device(&p.resampler_output, frames, &mut p.device_input_producers)
        } else {
            warn!(
                "No mixer for {} -> {} channel mismatch; dropping frames",
                p.source_channel_count, p.device_channel_count
            );
            Ok(())
        };

        // 设备环形缓冲按一个周期的最坏情况定尺寸、并在这同一线程上排空，所以写入
        // 失败意味着引擎有 bug——要大张旗鼓地报，而不是静默丢音频
        if let Err(e) = result {
            error!("failed to hand resampler output to the device stage: {e:?}");
        }

        p.clear_resampler_output();
    }

    /// 在均衡级两端抽频谱：前置与后置环形缓冲看到的永远是同一批帧。
    fn eq_with_tap(
        eq: &mut EqualizerProcessor,
        tap: &mut SpectrumTap,
        planes: &mut [Vec<f64>],
        frames: usize,
    ) {
        let tapped = tap.push_pre(planes, frames);
        eq.process(planes, frames);
        tap.push_post(planes, tapped, eq.audible());
    }

    /// 冲刷旧重采样器并清空它的输出。
    fn flush_old_resampler(
        old: &mut Resampler,
        p: &mut AudioPipeline,
        mixer: &mut Option<ChannelMixer>,
        eq: &mut EqualizerProcessor,
        tap: &mut SpectrumTap,
    ) {
        if old.channels() != p.source_channel_count {
            warn!(
                "dropping resampler tail: channel count changed ({} -> {})",
                old.channels(),
                p.source_channel_count
            );
            return;
        }

        let flushed = old.flush_into(&mut p.resampler_output);
        if flushed > 0 {
            info!("flushed {flushed} tail frames from the previous resampler");
            Self::route_resampler_output(p, mixer, eq, tap);
        }
    }

    fn flush_tail_to_device(&mut self) {
        let Some(resampler) = &mut self.resampler else {
            return;
        };
        let Some(p) = &mut self.pipeline else {
            return;
        };

        Self::flush_old_resampler(resampler, p, &mut self.mixer, &mut self.eq, &mut self.tap);

        // 重复几轮以确保缓冲中的帧全部冲刷干净（单趟未必能消费完整块）
        for _ in 0..8 {
            if p.device_input.potentially_available() == 0 {
                break;
            }
            if let Err(err) = self.device.consume_from(&mut p.device_input) {
                warn!("failed to hand the flushed tail to the device: {err}");
                break;
            }
        }
    }

    fn passthrough_to_device(
        input: &[Vec<f64>],
        frames: usize,
        output: &mut crate::media::pipeline::ChannelProducers,
    ) -> Result<(), crate::media::pipeline::WriteError> {
        if frames == 0 {
            return Ok(());
        }
        let slices: smallvec::SmallVec<[&[f64]; 8]> = input.iter().map(|v| &v[..frames]).collect();

        output.write_slices(&slices)
    }

    /// 统一处理解码错误
    fn handle_decode_error(
        e: crate::media::errors::PlaybackReadError,
    ) -> Result<DecodeStepResult, EngineError> {
        use crate::media::errors::PlaybackReadError;

        match e {
            PlaybackReadError::InvalidState => {
                error!("Thread state is invalid: decoder state is invalid");
                Err(EngineError::DecodeError(
                    "Decoder in invalid state".to_string(),
                ))
            }
            PlaybackReadError::NeverStarted => {
                error!("Thread state is invalid: playback never started");
                Err(EngineError::DecodeError(
                    "Playback never started".to_string(),
                ))
            }
            PlaybackReadError::Eof => {
                info!("EOF during decode");
                Ok(DecodeStepResult::Eof)
            }
            PlaybackReadError::ChannelCountChanged(count) => {
                warn!("decoded channel count changed to {count}; rebuilding pipeline");
                Ok(DecodeStepResult::Rebuild(PipelineOverrides {
                    source_spec: Some(ChannelSpec::Count(count.min(usize::from(u16::MAX)) as u16)),
                }))
            }
            PlaybackReadError::Unknown(s) => {
                error!("Unknown decode error: {}", s);
                warn!("Samples may be skipped");
                Ok(DecodeStepResult::Continue)
            }
            PlaybackReadError::DecodeFatal(s) => {
                error!("Fatal decoding error: {}", s);
                Ok(DecodeStepResult::FatalError(s))
            }
        }
    }
}

/// 解码/重采样这一步的内部结果类型。
enum DecodeStepResult {
    Continue,
    Eof,
    FatalError(String),
    Rebuild(PipelineOverrides),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::channels::{ChannelLayout, ChannelPosition};
    use crate::playback::thread::spectrum_tap;
    use crate::settings::equalizer::{EqBandKind, EqBandSettings, EqualizerSettings};

    fn sine(frequency: f64, frames: usize, sample_rate: f64) -> Vec<f64> {
        (0..frames)
            .map(|i| (2.0 * std::f64::consts::PI * frequency * i as f64 / sample_rate).sin())
            .collect()
    }

    fn config(kind: EqBandKind, frequency: f64, gain_db: f64, enabled: bool) -> EqualizerSettings {
        EqualizerSettings {
            enabled,
            bands: vec![EqBandSettings {
                kind,
                frequency,
                gain_db,
                q: 1.0,
                enabled: true,
            }],
            ..Default::default()
        }
    }

    fn peak(planes: &[Vec<f64>]) -> f64 {
        planes
            .iter()
            .flat_map(|p| p.iter())
            .fold(0.0_f64, |peak, &s| peak.max(s.abs()))
    }

    #[test]
    fn route_passthrough_applies_eq() {
        let mut p = AudioPipeline::new(2, 2, 48_000, 48_000, 64, 0);
        let mut eq = EqualizerProcessor::new(48_000.0, 2);
        eq.set_config(&config(EqBandKind::Bell, 1_000.0, 24.0, true));

        let dry = sine(1_000.0, 64 * 64, 48_000.0);
        let in_peak = peak(std::slice::from_ref(&dry));
        let (mut tap, _consumer) = spectrum_tap();
        let mut out_peak = 0.0;
        // 增益渐变得几个块才能收敛
        for block in 0..64 {
            let chunk: Vec<f64> = dry[block * 64..(block + 1) * 64].to_vec();
            p.resampler_output = vec![chunk.clone(), chunk];
            AudioEngine::route_resampler_output(&mut p, &mut None, &mut eq, &mut tap);
            assert_eq!(p.device_input.try_read_to_staging(64), 64);
            out_peak = peak(p.device_input.staging());
        }

        assert!(out_peak > in_peak * 10.0);
        assert!(out_peak < in_peak * 25.0);
    }

    #[test]
    fn route_passthrough_bypassed_eq_is_bit_exact() {
        let mut p = AudioPipeline::new(2, 2, 48_000, 48_000, 64, 0);
        let mut eq = EqualizerProcessor::new(48_000.0, 2);
        eq.set_config(&config(EqBandKind::Bell, 1_000.0, 24.0, false));

        let dry = sine(1_000.0, 64, 48_000.0);
        let (mut tap, _consumer) = spectrum_tap();
        p.resampler_output = vec![dry.clone(), dry.clone()];
        AudioEngine::route_resampler_output(&mut p, &mut None, &mut eq, &mut tap);

        assert_eq!(p.device_input.try_read_to_staging(64), 64);
        assert_eq!(p.device_input.staging()[0], dry);
    }

    #[test]
    fn route_mixer_path_applies_eq_after_mixing() {
        let mut p = AudioPipeline::new(1, 2, 48_000, 48_000, 64, 0);
        let mut mixer = Some(ChannelMixer::new(
            ChannelLayout::Positioned(ChannelPosition::FRONT_CENTER),
            ChannelLayout::Positioned(ChannelPosition::FRONT_LEFT | ChannelPosition::FRONT_RIGHT),
            MixOptions::default(),
        ));
        let mut eq = EqualizerProcessor::new(48_000.0, 2);
        eq.set_config(&config(EqBandKind::Notch, 1_000.0, 0.0, true));

        let dry = sine(1_000.0, 64 * 64, 48_000.0);
        let (mut tap, _consumer) = spectrum_tap();
        let mut out_peak = 1.0;
        for block in 0..64 {
            p.resampler_output = vec![dry[block * 64..(block + 1) * 64].to_vec()];
            AudioEngine::route_resampler_output(&mut p, &mut mixer, &mut eq, &mut tap);
            assert_eq!(p.device_input.try_read_to_staging(64), 64);
            assert_eq!(p.device_input.staging().len(), 2);
            out_peak = peak(p.device_input.staging());
        }

        assert!(out_peak < 0.01);
    }
}
