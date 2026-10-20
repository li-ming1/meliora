pub(crate) mod audio_engine;
mod device_controller;
mod media_controller;
mod queue_manager;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    thread::sleep,
};

use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    watch,
};
use tracing::{debug, error, info, warn};

use crate::{
    media::errors::PlaybackStartError,
    playback::{
        dsp::spectrum::spectrum_tap, events::RepeatState, session_storage::PlaybackSessionData,
    },
    settings::{
        equalizer::EqualizerSettings,
        playback::PlaybackSettings,
        replaygain::{ReplayGainAutoHint, ReplayGainMode, calculate_gain},
    },
};

use super::{
    events::{PlaybackCommand, PlaybackEvent},
    interface::PlaybackInterface,
    queue::QueueItemData,
};

use audio_engine::{AudioEngine, EngineCycleResult, EngineState, OnlineRefresh};
use queue_manager::{
    DequeueManyResult, DequeueResult, InsertResult, JumpResult, MoveResult, QueueManager,
    QueueNavigationResult, ReplaceResult, Reshuffled, ShuffleResult, UndoResult,
};

// 对位置广播节流，避免占用过多 CPU，应用未获得焦点时尤其如此
const ACTIVE_POSITION_BROADCAST_INTERVAL_MS: u64 = 33;
const BACKGROUND_POSITION_BROADCAST_INTERVAL_MS: u64 = 250;

/// 播放中连续多少个无进展周期之后跳过当前曲目。
const MAX_NO_PROGRESS_CYCLES: u32 = 50;

/// 播放循环内在线 URL 刷新的上限：提供者客户端没有逐请求超时，而这个重试在播放
/// 线程上同步执行，所以无响应的 API 绝不能拖住音频循环。
/// 只由 `build_refresh_hook` 消费（online_sources 门控）。
#[cfg(feature = "online_sources")]
const STREAM_REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

/// 无进展周期之后的休眠：从 2 ms 指数增长到 50 ms，让持续错误不至于占死一个核。
fn no_progress_backoff(cycles: u32) -> std::time::Duration {
    let shift = cycles.saturating_sub(1).min(5);
    let ms = (2_u64 << shift).min(50);
    std::time::Duration::from_millis(ms)
}

/// 当前曲目结束前多早就在后台预打开下一曲的媒体流（无缝切歌前进）。这段提前量
/// 覆盖最慢的在线打开（签名 URL 刷新 + 连接 + 探测）。
const PREPARE_LEAD_MS: u64 = 15_000;

/// 队列条目的路径是否指向可播放的东西：仍存在的本地文件，或（启用酷狗 feature 时）
/// 一个 HTTP(S) 流 URL。
fn path_playable(path: &Path) -> bool {
    #[cfg(feature = "online_sources")]
    if crate::media::is_http_path(path) {
        return true;
    }

    path.exists()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Stopped,
    Playing,
    Paused,
}

impl From<EngineState> for PlaybackState {
    fn from(state: EngineState) -> Self {
        match state {
            EngineState::Idle => PlaybackState::Stopped,
            EngineState::Playing => PlaybackState::Playing,
            EngineState::Paused => PlaybackState::Paused,
        }
    }
}

/// 播放线程通过协调音频引擎与队列管理器来编排音频播放。
pub struct PlaybackThread {
    /// 播放设置，线程启动时接收。
    playback_settings: PlaybackSettings,
    commands_rx: UnboundedReceiver<PlaybackCommand>,
    events_tx: UnboundedSender<PlaybackEvent>,
    /// 当前曲目最近一次的时间戳（毫秒），用于判断位置自上次更新后是否变化。
    last_timestamp: u64,
    /// 已经发给 UI 与元数据广播服务的最近时间戳。
    last_broadcast_timestamp: u64,
    /// 位置更新是否应当按满频率发出。
    position_broadcast_active: bool,
    engine: AudioEngine,
    queue: QueueManager,
    /// 启动时要应用的音量（从持久化设置恢复）。
    initial_volume: f64,
    /// ReplayGain 自动模式当前的取向提示。
    rg_auto_hint: ReplayGainAutoHint,
    /// 上次元数据更新缓存下来的曲目增益。
    last_track_gain: Option<f64>,
    /// 上次元数据更新缓存下来的专辑增益。
    last_album_gain: Option<f64>,
    /// 缓存的曲目/专辑样本峰值，用于防削波的增益封顶。
    last_track_peak: Option<f64>,
    last_album_peak: Option<f64>,
    /// 已知时当前曲目的时长（毫秒），驱动无缝切歌预备。
    duration_ms: Option<u64>,
    stop_after_current: bool,
    /// 播放中连续的无进展周期数；驱动退避与跳过。
    no_progress_cycles: u32,
    /// 空闲等待挂起期间从通道取出的命令，交给下一次 `command_intake`，
    /// 因此唤醒它的那条命令绝不会丢。
    pending_command: Option<PlaybackCommand>,
    /// 启动后的首个在线打开是否要先刷新 URL：线程构造时置位（恢复出来的会话
    /// 里当前曲目是 HTTP 直链——签名 URL 跨进程必然过期）。见
    /// [`Self::open_with_resampler`] 的先行刷新分支。
    #[cfg(feature = "online_sources")]
    initial_online_open: bool,
}

impl PlaybackThread {
    /// 创建新的播放接口并启动播放线程。
    pub fn start(
        queue: Arc<RwLock<Arc<Vec<QueueItemData>>>>,
        playback_settings: PlaybackSettings,
        last_volume: f64,
        session: PlaybackSessionData,
        storage_tx: watch::Sender<PlaybackSessionData>,
    ) -> PlaybackInterface {
        let (commands_tx, commands_rx) = unbounded_channel();
        let (events_tx, events_rx) = unbounded_channel();
        let engine_events_tx = events_tx.clone();
        let (tap, tap_consumer) = spectrum_tap();

        std::thread::Builder::new()
            .name("playback".to_string())
            .spawn(move || {
                // 解码泵的 MMCSS 登记（"Playback" 任务类）：CPU 竞争场景
                // 下保住环形缓冲的生产者——2026-10-04 的 696 次 underrun
                // 就是泵线程被后台构建饿穿（见 devices::mmcss 模块文档）。
                crate::devices::mmcss::register_playback_thread();
                // 恢复出来的会话里当前曲目若是在线直链，其签名 URL 跨进程
                // 必然过期：首个打开前先刷新，省掉一轮注定 403 的陈旧打开
                // （见 open_with_resampler 的先行刷新分支）。
                #[cfg(feature = "online_sources")]
                let initial_online_open = {
                    let position = session.queue_position.unwrap_or(usize::MAX);
                    session
                        .queue
                        .get(position)
                        .is_some_and(|item| crate::media::is_http_path(item.get_path()))
                };
                let queue_manager =
                    QueueManager::new(queue, playback_settings.clone(), session, storage_tx);

                let mut thread = PlaybackThread {
                    playback_settings,
                    commands_rx,
                    events_tx,
                    last_timestamp: u64::MAX,
                    last_broadcast_timestamp: u64::MAX,
                    position_broadcast_active: true,
                    engine: AudioEngine::new(engine_events_tx, tap),
                    queue: queue_manager,
                    initial_volume: last_volume,
                    rg_auto_hint: ReplayGainAutoHint::PreferTrack,
                    last_track_gain: None,
                    last_album_gain: None,
                    last_track_peak: None,
                    last_album_peak: None,
                    duration_ms: None,
                    stop_after_current: false,
                    no_progress_cycles: 0,
                    pending_command: None,
                    #[cfg(feature = "online_sources")]
                    initial_online_open,
                };

                thread.run();
            })
            .expect("unable to spawn thread");

        PlaybackInterface::new(commands_tx, events_rx, tap_consumer)
    }

    /// 初始化引擎并运行主循环。
    pub fn run(&mut self) {
        // 初始化音频引擎（媒体提供者、设备提供者、初始流）
        if let Err(e) = self.engine.initialize() {
            error!("Failed to initialize audio engine: {:?}", e);
        }

        self.engine.set_equalizer(&self.playback_settings.equalizer);

        self.set_volume(self.initial_volume);
        self.send_event(PlaybackEvent::RepeatChanged(self.queue.repeat_state()));
        self.send_event(PlaybackEvent::ShuffleToggled(
            self.queue.is_shuffle_enabled(),
        ));

        loop {
            self.main_loop();
        }
    }

    /// 收取命令并运行音频播放循环。
    pub fn main_loop(&mut self) {
        self.command_intake();

        // 先把设备的延后工作收尾（例如异步暂停淡出），但不因此阻塞命令收取。
        self.engine.poll();

        // 收集已完成的无缝切歌预备（若有）。预备成功但落地路径与所请求的不同，
        // 说明预备线程把过期的在线 URL 救了回来：把新 URL 写回当初请求预备的那个
        // 队列条目，这样 EOF 前进时打开的是就绪的流而不是过期 URL。
        // 若解析出的下一条目已与请求不符，`replace_next_path` 会静默丢弃这次写入。
        if let Some(report) = self.engine.poll_prepared()
            && report.succeeded
            && report.path != report.requested
        {
            self.queue.replace_next_path(&report.requested, report.path);
        }

        if self.engine.state() == EngineState::Playing {
            if self.play_audio() {
                self.no_progress_cycles = 0;
                self.maybe_prepare_next();
            } else {
                self.no_progress_cycles = self.no_progress_cycles.saturating_add(1);
                if self.no_progress_cycles >= MAX_NO_PROGRESS_CYCLES {
                    warn!(
                        "engine made no progress for {} cycles; skipping track",
                        self.no_progress_cycles
                    );
                    self.no_progress_cycles = 0;
                    self.next(false, false);
                } else {
                    // 我们没有阻塞等待设备，所以必须在这里休眠
                    sleep(no_progress_backoff(self.no_progress_cycles));
                }
            }
        } else {
            self.no_progress_cycles = 0;
        }

        // 冲刷防抖的队列会话快照：批量变更合并成每个防抖窗口一次全队列快照。
        self.queue.flush_pending_session();

        self.process_metadata_update();

        // 没有播放时让线程挂起；放在会话冲刷之后，脏快照才不会被搁在挂起后面。
        // 旧的 10 ms 节奏只在 idle_wait 内、设备延后工作（暂停淡出截止）仍未完成时保留。
        if self.engine.state() != EngineState::Playing {
            self.idle_wait();
        }
    }

    /// 停止或暂停时让线程挂起，直到真的有活可干。阻塞在命令通道上，空闲的播放器
    /// 因此零次唤醒，而不是 100 Hz 空转；延后的暂停淡出（其 `poll` 截止尚未到）
    /// 仍沿用旧的 10 ms 节奏。唤醒它的命令会暂存给下一次 `command_intake`——
    /// 挂起发生在一轮 pass 的末尾，所以该命令在下一轮就被处理，零额外延迟。
    fn idle_wait(&mut self) {
        if self.engine.has_deferred_work() {
            sleep(std::time::Duration::from_millis(10));
            return;
        }
        match self.commands_rx.blocking_recv() {
            Some(command) => self.pending_command = Some(command),
            // 发送端已全部丢弃（应用正在退出）：保持旧节奏直到进程退出，
            // 而不是在断开的通道上空转。
            None => sleep(std::time::Duration::from_millis(10)),
        }
    }

    /// 从命令通道读取进来的命令并处理。连续的 Next/Seek 串会先做折叠（见下文）。
    pub fn command_intake(&mut self) {
        let mut commands: Vec<PlaybackCommand> = Vec::new();
        // 唤醒挂起 idle_wait 的那条命令排在批次最前，这样跨越唤醒的
        // Next/Seek 串照常折叠。
        if let Some(command) = self.pending_command.take() {
            commands.push(command);
        }
        while let Ok(command) = self.commands_rx.try_recv() {
            commands.push(command);
        }
        // 把连续的 Next（快速连点）与 Seek（进度条拖动约 30 Hz 发出）各自折叠成
        // 该串的最后一个：每执行一次 Seek 都会重建音频流并重新锚定在线源，
        // 中间位置纯属浪费。
        let mut collapsed: Vec<PlaybackCommand> = Vec::with_capacity(commands.len());
        for command in commands {
            let repeats_last = matches!(
                (&command, collapsed.last()),
                (PlaybackCommand::Next, Some(PlaybackCommand::Next))
                    | (PlaybackCommand::Seek(_), Some(PlaybackCommand::Seek(_)))
            );
            if repeats_last {
                *collapsed.last_mut().expect("matched collapsed.last()") = command;
            } else {
                collapsed.push(command);
            }
        }

        for command in collapsed {
            match command {
                PlaybackCommand::Play => self.play(),
                PlaybackCommand::Pause => self.pause(),
                #[cfg(not(target_os = "windows"))]
                PlaybackCommand::TogglePlayPause => self.toggle_play_pause(),
                PlaybackCommand::Queue(v) => self.queue_item(&v),
                PlaybackCommand::QueueList(v) => self.queue_list(v),
                PlaybackCommand::InsertAt { item, position } => self.insert_at(&item, position),
                PlaybackCommand::InsertListAt { items, position } => {
                    self.insert_list_at(items, position)
                }
                PlaybackCommand::Next => self.next(true, false),
                PlaybackCommand::Previous => self.previous(),
                PlaybackCommand::ClearQueue => self.clear_queue(),
                PlaybackCommand::Jump(v) => self.jump(v),
                PlaybackCommand::Seek(v) => self.seek(v),
                PlaybackCommand::SetVolume(v) => self.set_volume(v),
                PlaybackCommand::ReplaceQueue(v) => self.replace_queue(v),
                #[cfg(any(target_os = "linux", target_os = "windows"))]
                PlaybackCommand::Stop => self.stop(),
                PlaybackCommand::ToggleShuffle => self.toggle_shuffle(),
                PlaybackCommand::SetShuffle(v) => self.set_shuffle(v),
                PlaybackCommand::SetRepeat(v) => self.set_repeat(v),
                PlaybackCommand::RemoveItem(idx) => self.remove(idx),
                PlaybackCommand::RemoveItems(indices) => self.remove_many(indices),
                PlaybackCommand::MoveItem { from, to } => self.move_item(from, to),
                PlaybackCommand::MoveItems { indices, to } => self.move_items(indices, to),
                PlaybackCommand::Undo => self.undo(),
                PlaybackCommand::SettingsChanged(settings) => self.settings_changed(settings),
                PlaybackCommand::SetEqualizer(settings) => self.set_equalizer(settings),
                PlaybackCommand::SetPositionBroadcastActive(active) => {
                    self.set_position_broadcast_active(active)
                }
                PlaybackCommand::ReplaceQueueWithIndex(v, idx) => {
                    self.replace_queue_with_index(v, idx)
                }
                PlaybackCommand::StopAfterCurrent => self.toggle_stop_after_current(),
            }
        }
    }
    /// 取当前播放状态。
    fn state(&self) -> PlaybackState {
        self.engine.state().into()
    }

    /// 暂停播放。
    pub fn pause(&mut self) {
        if self.state() == PlaybackState::Paused {
            return;
        }

        if self.state() == PlaybackState::Playing {
            if let Err(e) = self.engine.pause() {
                warn!("Failed to pause: {:?}", e);
            }

            self.send_event(PlaybackEvent::StateChanged(PlaybackState::Paused));
        }
    }

    /// 恢复播放。若上次播放的曲目是队列末尾，则从头重启队列。
    pub fn play(&mut self) {
        let current_state = self.state();

        if current_state == PlaybackState::Playing {
            return;
        }

        if current_state == PlaybackState::Paused {
            if let Err(e) = self.engine.play() {
                error!("Failed to resume playback: {:?}", e);
                return;
            }

            self.send_event(PlaybackEvent::StateChanged(PlaybackState::Playing));
            return;
        }

        // 处于停止且队列非空时，从头开始播放
        if current_state == PlaybackState::Stopped
            && let Some((path, index)) = self.queue.first_playable_path()
        {
            self.open_or_log(&path);
            self.queue.set_position(index);
            self.send_event(PlaybackEvent::QueuePositionChanged(index));
        }
    }

    /// 打开媒体文件并准备好播放。
    fn open(&mut self, path: &Path) -> Result<(), PlaybackStartError> {
        self.open_with_resampler(path, false)
    }

    /// 打开曲目用于播放；失败只记日志而不向上抛：即便这首打不开，
    /// 正在进行的那次队列操作也必须走完。
    fn open_or_log(&mut self, path: &Path) {
        if let Err(err) = self.open(path) {
            error!(path = %path.display(), ?err, "Unable to open file: {err}");
        }
    }

    /// 跨越一次可能复用重采样器的切换时调用 [`Self::open_or_log`]
    /// （无缝切歌前进）。
    fn open_or_log_resampled(&mut self, path: &Path, preserve_resampler: bool) {
        if let Err(err) = self.open_with_resampler(path, preserve_resampler) {
            error!(path = %path.display(), ?err, "Unable to open file: {err}");
        }
    }

    /// 过期在线流 URL 的一次性重试：持久化的酷狗直链迟早会 403（签名 URL），
    /// 所以在跳过本曲之前，先用当前条目的提供者身份重新取一个全新 URL、
    /// 更新队列条目，并返回新路径。
    #[cfg(feature = "online_sources")]
    fn refresh_expired_online_url(&mut self) -> Option<PathBuf> {
        let identity = self.queue.current_online_identity().or_else(|| {
            // 歌单条目只持久化裸的流 URL，没有提供者身份。提供者的流注册表记得
            // 这些 URL 背后的曲目（酷狗在 URL 里嵌了 `mx{mixsongid}` 标记），
            // 于是编译期注册表能为重新取链还原出完整身份。
            let path = self.queue.current_path()?;
            crate::online_sources::identify_path(&path).map(|m| m.identity)
        })?;
        let display = self
            .queue
            .current_display()
            .unwrap_or((None, None, None, None));
        let refresh = self.build_refresh_hook(identity, display);
        let path = refresh()?;
        self.queue.replace_current_path(path.clone());
        Some(path)
    }

    #[cfg(not(feature = "online_sources"))]
    fn refresh_expired_online_url(&mut self) -> Option<PathBuf> {
        None
    }

    /// 待打开曲目的流中刷新钩子：只在 HTTP 路径且提供者身份可解析时存在
    /// （队列条目持久化的身份，或流注册表兜底）。在打开时就装到 source 上，
    /// 过期的签名 URL 于是在填充线程自愈，而不是拖到触发致命跳过。
    #[cfg(feature = "online_sources")]
    fn build_current_refresh_hook(&self) -> Option<crate::media::http_source::MediaUrlRefresh> {
        let path = self.queue.current_path()?;
        if !crate::media::is_http_path(&path) {
            return None;
        }
        let identity = self
            .queue
            .current_online_identity()
            .or_else(|| crate::online_sources::identify_path(&path).map(|m| m.identity))?;
        let display = self
            .queue
            .current_display()
            .unwrap_or((None, None, None, None));
        Some(self.build_refresh_hook(identity, display))
    }

    #[cfg(not(feature = "online_sources"))]
    fn build_current_refresh_hook(&self) -> Option<crate::media::http_source::MediaUrlRefresh> {
        None
    }

    /// 所有过期 URL 刷新点共用的内核——切换时的同步重试
    /// （[`Self::refresh_expired_online_url`]）、后台预备的 on-error 钩子，
    /// 以及装到每条已打开流上的流中刷新钩子（[`Self::build_current_refresh_hook`]、
    /// [`AudioEngine::prepare_next`]）——提供者调用、它的超时与音质设置因此
    /// 只存在于这一处。返回可共享的钩子：每次调用重新取一个新路径，
    /// 提供者再也给不出时返回 `None`。
    ///
    /// 钩子会阻塞在提供者刷新上，上界是 [`STREAM_REFRESH_TIMEOUT`]，只能跑在
    /// 允许阻塞的地方：播放线程（切换间隙本就听得见）或专用的 media-prepare /
    /// fill std 线程。它们都是普通线程，所以 `block_on` 只挂住该线程本身——
    /// 绝不可能占住 tokio worker 而饿到 runtime。
    #[cfg(feature = "online_sources")]
    fn build_refresh_hook(
        &self,
        identity: crate::online_sources::OnlineIdentity,
        display: crate::online_sources::OnlineDisplay,
    ) -> crate::media::http_source::MediaUrlRefresh {
        // `as_str` 返回 `&'static str`，因此音质快照能直接随闭包带走，不必借用 `self`。
        #[cfg(feature = "kugou")]
        let kugou_quality = self.playback_settings.online_quality.as_str();
        Arc::new(move || {
            let ctx = crate::online_sources::RefreshContext {
                #[cfg(feature = "kugou")]
                kugou_quality,
                display: display.clone(),
            };
            let fresh = crate::RUNTIME
                .block_on(async {
                    tokio::time::timeout(
                        STREAM_REFRESH_TIMEOUT,
                        crate::online_sources::refresh_online_url(&identity, &ctx),
                    )
                    .await
                    .ok()
                    .flatten()
                })
                .map(PathBuf::from);
            if fresh.is_none() {
                warn!(
                    identity = ?identity,
                    title = ctx.display.0.as_deref().unwrap_or("?"),
                    "failed to refresh expired stream URL"
                );
            }
            fresh
        })
    }

    /// 用 `next_path` 的 on-error 钩子武装后台预备：只对能解析出提供者身份的
    /// HTTP 路径存在（队列条目持久化的身份，或流注册表兜底）。钩子所需的一切
    /// 在预备任务 spawn 之前捕获完毕；它跑在专用的 media-prepare std 线程上，
    /// 在那里阻塞等待提供者刷新不会拖住播放循环。
    #[cfg(feature = "online_sources")]
    fn build_prepare_refresh(&self, next_path: &Path) -> Option<OnlineRefresh> {
        if !crate::media::is_http_path(next_path) {
            return None;
        }
        let identity = self.queue.peek_next_online_identity().or_else(|| {
            // 与切换重试同一兜底：歌单条目只持久化裸流 URL，而各提供者的流
            // 注册表记得这些 URL 背后的曲目。
            crate::online_sources::identify_path(next_path).map(|m| m.identity)
        })?;
        let display = self
            .queue
            .peek_next_display()
            .unwrap_or((None, None, None, None));
        Some(OnlineRefresh {
            retry: self.build_refresh_hook(identity, display),
        })
    }

    /// 本地文件从不需要 URL 刷新；预备的行为与该钩子存在之前完全一致。
    #[cfg(not(feature = "online_sources"))]
    fn build_prepare_refresh(&self, _next_path: &Path) -> Option<OnlineRefresh> {
        None
    }

    fn open_with_resampler(
        &mut self,
        path: &Path,
        preserve_resampler: bool,
    ) -> Result<(), PlaybackStartError> {
        // 启动恢复出来的在线曲目先行刷新：队列里的 HTTP 直链是跨进程持久化的
        // 签名 URL，必然已过期，直接打开只会吃一轮 403 再走下面的重试——
        // 实测那一轮白等 ~575ms，刷新 ~490ms，起播从 ~1.4s 才能开始。先刷新
        // 再打开，起播提前到 ~0.7s。只对启动后的首个在线打开生效；会话中途
        // 新取的 URL 不受影响。刷新失败就原样走下面的打开 + 重试逻辑，
        // 行为与没有这个分支时完全一致。
        #[cfg(feature = "online_sources")]
        let pre_refreshed = if self.initial_online_open && crate::media::is_http_path(path) {
            self.initial_online_open = false;
            let refreshed = self.refresh_expired_online_url();
            if refreshed.is_some() {
                info!("restored online track: pre-refreshed expired stream URL before first open");
            }
            refreshed
        } else {
            None
        };
        #[cfg(not(feature = "online_sources"))]
        let pre_refreshed: Option<std::path::PathBuf> = None;
        let path = pre_refreshed.as_deref().unwrap_or(path);

        info!("Opening track '{}'", path.display());

        self.last_track_gain = None;
        self.last_album_gain = None;
        self.last_track_peak = None;
        self.last_album_peak = None;

        let (duration, open_path) = {
            // 为已打开的流装上流中 URL 刷新（过期的签名 URL 会在 fill 线程上自愈）；
            // 本地文件为 None。
            let refresh = self.build_current_refresh_hook();
            match self.engine.open(path, preserve_resampler, refresh) {
                Ok(duration) => (duration, path.to_path_buf()),
                Err(first_error) => {
                    // 持久化的在线直链带签名、会过期（403）。跳过之前先用该条目的
                    // 提供者身份重新取一个新 URL，并重试打开一次。
                    let fresh_path = if crate::media::is_http_path(path) {
                        self.refresh_expired_online_url()
                    } else {
                        None
                    };
                    let Some(fresh_path) = fresh_path else {
                        return Err(first_error);
                    };
                    info!("persisted stream URL expired; retrying with refreshed URL");
                    let refresh = self.build_current_refresh_hook();
                    let duration = self.engine.open(&fresh_path, preserve_resampler, refresh)?;
                    (duration, fresh_path)
                }
            }
        };

        // [mem] 探针：每次换曲都采样，让长会话的日志能看出已提交的 private 字节是
        // 逐曲持续爬升还是回落到稳定基线（真实的存活增长，而非工作集噪声）。
        let (private, working) = crate::process_memory_mb();
        tracing::info!(path = %open_path.display(), private_mb = private, working_mb = working, "[mem] track open");

        // 单曲循环开启时启用循环点感知解码
        self.engine
            .set_looping(self.queue.repeat_state() == RepeatState::RepeatingOne);

        self.send_event(PlaybackEvent::SongChanged(open_path));

        self.duration_ms = duration;
        self.send_event(PlaybackEvent::DurationChanged(duration.unwrap_or(0)));

        self.process_metadata_update();

        self.update_ts(true);

        self.send_event(PlaybackEvent::StateChanged(PlaybackState::Playing));

        Ok(())
    }

    /// 当前曲目即将结束时预打开下一曲的媒体流，让 EOF 切换挂上的是就绪的流，而不是
    /// 阻塞在连接+探测上（在线源听感上的切歌间隙就出在这里）。队列变更会使已预备的
    /// 流失效。若下一条目是 HTTP 路径，预备会带上 URL 刷新钩子，于是过期的签名 URL
    /// 在这个提前窗口内由预备线程自行救回（刷新 + 重开），而不是阻塞切换。
    fn maybe_prepare_next(&mut self) {
        if self.stop_after_current || self.playback_settings.consume {
            return;
        }
        let Some(duration) = self.duration_ms else {
            return;
        };
        if duration.saturating_sub(self.last_timestamp) > PREPARE_LEAD_MS {
            return;
        }
        // 在取队列锁并克隆 PathBuf 之前的廉价守卫：15s 提前窗口内每个解码周期都会
        // 走到这里，而下一流一旦已预备/已在途，之后的每次调用都是纯开销。
        if self.engine.is_next_prepared() {
            return;
        }
        let Some(next_path) = self.queue.peek_next_path() else {
            return;
        };
        // 失败冷却长达 30 s，而这里每个解码周期都跑：当 prepare_next 只会拒绝这次
        // 尝试时，先跳过注册表扫描与闭包分配。
        if self.engine.prepare_rejected(&next_path) {
            return;
        }
        let on_open_error = self.build_prepare_refresh(&next_path);
        self.engine.prepare_next(&next_path, on_open_error);
    }

    fn process_metadata_update(&mut self) {
        if let Some(metadata) = self.engine.check_metadata_update() {
            self.last_track_gain = metadata.metadata.replaygain_track_gain;
            self.last_album_gain = metadata.metadata.replaygain_album_gain;
            self.last_track_peak = metadata.metadata.replaygain_track_peak;
            self.last_album_peak = metadata.metadata.replaygain_album_peak;

            self.reapply_replaygain();

            self.send_event(PlaybackEvent::MetadataUpdate(metadata.metadata));
            self.send_event(PlaybackEvent::AlbumArtUpdate(metadata.album_art));
        }
    }

    fn reapply_replaygain(&mut self) {
        let gain = calculate_gain(
            &self.playback_settings.replaygain,
            self.rg_auto_hint,
            self.last_track_gain,
            self.last_album_gain,
            self.last_track_peak,
            self.last_album_peak,
        );
        if let Err(e) = self.engine.set_replaygain(gain) {
            warn!("Failed to set ReplayGain: {:?}", e);
        }
    }

    fn recompute_rg_auto_hint(&mut self) -> bool {
        // hint 只被 Auto 档的 calculate_gain 消费，而默认档是 Off：非 Auto
        // 时不做整队 `all_items_same_album` 扫描，也就不必为每次离散队列
        // 操作去抢那把与 UI/歌词面板共享的队列读锁。Off→Auto 的切换在
        // settings_changed 里补算，hint 不会停在过期值上。
        if self.playback_settings.replaygain.mode != ReplayGainMode::Auto {
            return false;
        }

        let next_hint = if !self.queue.is_shuffle_enabled() && self.queue.all_items_same_album() {
            ReplayGainAutoHint::PreferAlbum
        } else {
            ReplayGainAutoHint::PreferTrack
        };

        let changed = self.rg_auto_hint != next_hint;
        self.rg_auto_hint = next_hint;
        changed
    }

    fn refresh_rg_auto_hint(&mut self) {
        // 走到这里说明处于 Auto 档（非 Auto 时 recompute 恒返回 false）。
        if self.recompute_rg_auto_hint() {
            self.reapply_replaygain();
        }
    }

    /// 跳到队列中的下一曲。
    fn next(&mut self, user_initiated: bool, preserve_resampler: bool) {
        if user_initiated {
            self.set_stop_after_current(false);
        }

        if self.playback_settings.consume
            && !user_initiated
            && let Some(current_idx) = self.queue.current_position()
        {
            self.remove_with_resampler(current_idx, preserve_resampler);
            return;
        }

        match self.queue.next(user_initiated) {
            QueueNavigationResult::Changed {
                index,
                path,
                reshuffled,
            } => {
                info!("Opening next file in queue at index {}", index);

                if reshuffled == Reshuffled::Reshuffled {
                    self.send_event(PlaybackEvent::QueueUpdated);
                }

                self.open_or_log_resampled(
                    &path,
                    preserve_resampler && reshuffled == Reshuffled::NotReshuffled,
                );

                self.send_event(PlaybackEvent::QueuePositionChanged(index));
            }
            QueueNavigationResult::Unchanged { path } => {
                info!("Repeating current track");
                self.open_or_log_resampled(&path, preserve_resampler);
            }
            QueueNavigationResult::EndOfQueue => {
                info!("Playback queue ended, stopping playback");
                self.stop();
            }
        }
    }

    /// 跳到队列中的上一曲。
    fn previous(&mut self) {
        self.set_stop_after_current(false);

        // 已超过 5 秒时改为跳转到开头，而不是退回上一曲
        if self.state() == PlaybackState::Playing
            && self.playback_settings.prev_track_jump_first
            && self.last_timestamp > 5_000
        {
            self.seek(0.0);
            return;
        }

        // 处理停止态——从最后一曲开始播。可播槽位未必是 `len() - 1`：末尾若
        // 有无用可播的条目，游标会停在真正打开的那曲之后，下一次 `next()`
        // 就会撞上 EndOfQueue 而不是继续播。
        if self.state() == PlaybackState::Stopped {
            if let Some((path, index)) = self.queue.last_playable_path() {
                self.open_or_log(&path);
                self.queue.set_position(index);
                self.send_event(PlaybackEvent::QueuePositionChanged(index));
            }
            return;
        }

        match self.queue.previous() {
            QueueNavigationResult::Changed {
                index,
                path,
                reshuffled: _,
            } => {
                info!("Opening previous file in queue at index {}", index);
                self.open_or_log(&path);
                self.send_event(PlaybackEvent::QueuePositionChanged(index));
            }
            QueueNavigationResult::Unchanged { path } => {
                info!("At beginning of queue, replaying current track");
                self.open_or_log(&path);
            }
            QueueNavigationResult::EndOfQueue => {
                // 已在队列开头，什么都不做
            }
        }
    }

    /// 若当前没在播任何东西，就打开并开始播放刚插入到 `first_index` 的那个条目。
    /// 不可播的路径会被跳过；无论如何调用方随后那条 QueueUpdated 仍会播报队列变更。
    fn start_playing_inserted_if_stopped(&mut self, item: &QueueItemData, first_index: usize) {
        if self.state() != PlaybackState::Stopped || !path_playable(item.get_path()) {
            return;
        }
        let path = item.get_path();
        self.open_or_log(path);
        self.queue.set_position(first_index);
        self.send_event(PlaybackEvent::QueuePositionChanged(first_index));
    }

    /// 若当前没在播任何东西，就打开并开始播放刚插入的这批里第一个可播条目：
    /// `first` 是 `(批次内下标, 条目)`，`first_index` 是这批落在队列里的起始槽位。
    fn start_playing_first_inserted_if_stopped(
        &mut self,
        first: Option<(usize, QueueItemData)>,
        first_index: usize,
    ) {
        if self.state() != PlaybackState::Stopped {
            return;
        }
        let Some((relative_idx, first)) = first else {
            return;
        };
        let path = first.get_path();
        self.open_or_log(path);
        let position = first_index + relative_idx;
        self.queue.set_position(position);
        self.send_event(PlaybackEvent::QueuePositionChanged(position));
    }

    /// 往队列新增一个 [`QueueItemData`]。若当前没在播任何东西，就开始播放它。
    fn queue_item(&mut self, item: &QueueItemData) {
        info!("Adding file to queue: {}", item);

        let index = self.queue.queue_item(item.clone());
        self.refresh_rg_auto_hint();
        self.start_playing_inserted_if_stopped(item, index);

        self.send_event(PlaybackEvent::QueueUpdated);
    }

    /// 往队列新增一组 [`QueueItemData`]。若当前没在播任何东西，就开始播放第一曲。
    fn queue_list(&mut self, items: Vec<QueueItemData>) {
        if items.is_empty() {
            return;
        }

        info!("Adding {} files to queue", items.len());

        // 逐曲 exists() 是同步系统调用，只有 Stopped 分支消费其结果：先判
        // 状态再探测，播放中批量入队不再阻塞 playback 线程做 N 次 stat。
        // state 在命令单线程处理内不被 queue_items 改变，判定前移语义等价。
        let first = (self.state() == PlaybackState::Stopped)
            .then(|| {
                items
                    .iter()
                    .enumerate()
                    .find(|(_, item)| path_playable(item.get_path()))
                    .map(|(idx, item)| (idx, item.clone()))
            })
            .flatten();
        let first_index = self.queue.queue_items(items);
        self.refresh_rg_auto_hint();

        // `first` 已内含 Stopped 判定
        if let Some((relative_idx, first)) = first {
            let path = first.get_path();
            self.open_or_log(path);
            let position = first_index + relative_idx;
            // 随机播放开启时 `queue_items` 会把追加的整批打乱，被打开的那个条目
            // 便不再位于 `first_index + relative_idx`；经原始顺序解析出它的真实
            // 槽位，让游标跟着已打开的路径走（jump 自己会持久化状态）。
            let position = match self.queue.jump_unshuffled(position) {
                JumpResult::Jumped { .. } => self.queue.current_position().unwrap_or(position),
                JumpResult::OutOfBounds => {
                    self.queue.set_position(position);
                    position
                }
            };
            self.send_event(PlaybackEvent::QueuePositionChanged(position));
        }

        self.send_event(PlaybackEvent::QueueUpdated);
    }

    /// 把队列里的一个条目从一个位置挪到另一个位置。
    fn move_item(&mut self, from: usize, to: usize) {
        match self.queue.move_item(from, to, true) {
            MoveResult::Moved => {
                self.send_event(PlaybackEvent::QueueUpdated);
            }
            MoveResult::MovedCurrent { new_position } => {
                self.send_event(PlaybackEvent::QueuePositionChanged(new_position));
                self.send_event(PlaybackEvent::QueueUpdated);
            }
            MoveResult::Unchanged => {}
        }
    }

    fn move_items(&mut self, indices: Vec<usize>, to: usize) {
        use crate::playback::thread::queue_manager::MoveItemsResult;
        match self.queue.move_items(indices, to) {
            MoveItemsResult::Moved => {
                self.send_event(PlaybackEvent::QueueUpdated);
            }
            MoveItemsResult::MovedCurrent { new_position } => {
                self.send_event(PlaybackEvent::QueuePositionChanged(new_position));
                self.send_event(PlaybackEvent::QueueUpdated);
            }
            MoveItemsResult::Unchanged => {}
        }
    }

    /// 撤销最近一次队列变更。
    fn undo(&mut self) {
        let previous_state = self.state();
        let previous_shuffle = self.queue.is_shuffle_enabled();
        let previous_position = self.queue.current_position();

        match self.queue.undo_last_action() {
            UndoResult::Ok {
                current_idx,
                current_path,
                shuffle,
            } => {
                self.refresh_rg_auto_hint();

                if previous_state != PlaybackState::Stopped {
                    let should_reopen = self.engine.current_path() != Some(current_path.as_path());

                    if should_reopen {
                        self.open_or_log(&current_path);

                        if previous_state == PlaybackState::Paused {
                            self.pause();
                        }
                    }
                }

                if previous_shuffle != shuffle {
                    self.send_event(PlaybackEvent::ShuffleToggled(shuffle));
                }

                self.send_event(PlaybackEvent::QueueUpdated);

                if previous_position != Some(current_idx) {
                    self.send_event(PlaybackEvent::QueuePositionChanged(current_idx));
                }
            }
            UndoResult::OkNoCurrent { shuffle } => {
                self.refresh_rg_auto_hint();

                if previous_state != PlaybackState::Stopped {
                    self.stop();
                }

                if previous_shuffle != shuffle {
                    self.send_event(PlaybackEvent::ShuffleToggled(shuffle));
                }

                self.send_event(PlaybackEvent::QueueUpdated);

                if previous_position.is_some() {
                    self.send_event(PlaybackEvent::QueuePositionChanged(0));
                }
            }
            UndoResult::None => {}
        }
    }

    /// 移除当前播放曲目之后：打开顶替它槽位的那曲（按要求在这次切换中保留
    /// 重采样器），或者队列已无可播内容时停止。
    fn play_after_removal(&mut self, new_path: Option<PathBuf>, preserve_resampler: bool) {
        // 还有下一曲就播它
        let Some(path) = new_path else {
            self.stop();
            return;
        };
        self.open_or_log_resampled(&path, preserve_resampler);
        if let Some(pos) = self.queue.current_position() {
            self.send_event(PlaybackEvent::QueuePositionChanged(pos));
        }
    }

    /// 从队列移除一个条目。
    fn remove(&mut self, idx: usize) {
        self.remove_with_resampler(idx, false);
    }

    fn remove_with_resampler(&mut self, idx: usize, preserve_resampler: bool) {
        match self.queue.dequeue(idx) {
            DequeueResult::Removed { new_position } => {
                self.refresh_rg_auto_hint();
                self.send_event(PlaybackEvent::QueueUpdated);
                self.send_event(PlaybackEvent::QueuePositionChanged(new_position));
            }
            DequeueResult::RemovedCurrent { new_path } => {
                self.set_stop_after_current(false);
                self.refresh_rg_auto_hint();
                self.send_event(PlaybackEvent::QueueUpdated);
                self.play_after_removal(new_path, preserve_resampler);
            }
            DequeueResult::Unchanged => {}
        }
    }

    fn remove_many(&mut self, indices: Vec<usize>) {
        match self.queue.dequeue_many(indices) {
            DequeueManyResult::Removed { new_position } => {
                self.refresh_rg_auto_hint();
                self.send_event(PlaybackEvent::QueueUpdated);
                self.send_event(PlaybackEvent::QueuePositionChanged(new_position));
            }
            DequeueManyResult::RemovedCurrent { new_path } => {
                self.set_stop_after_current(false);
                self.refresh_rg_auto_hint();
                self.send_event(PlaybackEvent::QueueUpdated);
                self.play_after_removal(new_path, false);
            }
            DequeueManyResult::Unchanged => {}
        }
    }

    /// 在队列的指定位置插入一个 [`QueueItemData`]。若当前没在播任何东西，就开始播放它。
    fn insert_at(&mut self, item: &QueueItemData, position: usize) {
        info!("Inserting file to queue at position {}: {}", position, item);

        match self.queue.insert_item(position, item.clone()) {
            InsertResult::Inserted { first_index } => {
                self.refresh_rg_auto_hint();
                // 停止态时开始播放刚插入的条目
                self.start_playing_inserted_if_stopped(item, first_index);
            }
            InsertResult::InsertedMovedCurrent {
                first_index,
                new_position,
            } => {
                self.refresh_rg_auto_hint();
                self.send_event(PlaybackEvent::QueuePositionChanged(new_position));

                // 停止态时开始播放刚插入的条目
                self.start_playing_inserted_if_stopped(item, first_index);
            }
            InsertResult::Unchanged => {}
        }

        self.send_event(PlaybackEvent::QueueUpdated);
    }

    /// 在队列的指定位置插入一组 [`QueueItemData`]。若当前没在播任何东西，就开始播放第一曲。
    fn insert_list_at(&mut self, items: Vec<QueueItemData>, position: usize) {
        if items.is_empty() {
            return;
        }

        info!(
            "Inserting {} files to queue at position {}",
            items.len(),
            position
        );

        // 同 queue_list：exists() 探测前移到 Stopped 判定之后，只在确实
        // 需要起播时才做逐曲 stat；消费点内部的 Stopped 短路保持不变。
        let first = (self.state() == PlaybackState::Stopped)
            .then(|| {
                items
                    .iter()
                    .enumerate()
                    .find(|(_, item)| path_playable(item.get_path()))
                    .map(|(idx, item)| (idx, item.clone()))
            })
            .flatten();

        match self.queue.insert_items(position, items) {
            InsertResult::Inserted { first_index } => {
                self.refresh_rg_auto_hint();
                // 停止态时开始播放这批里第一个插入的条目
                self.start_playing_first_inserted_if_stopped(first, first_index);
            }
            InsertResult::InsertedMovedCurrent {
                first_index,
                new_position,
            } => {
                self.refresh_rg_auto_hint();
                self.send_event(PlaybackEvent::QueuePositionChanged(new_position));

                // 停止态时开始播放这批里第一个插入的条目
                self.start_playing_first_inserted_if_stopped(first, first_index);
            }
            InsertResult::Unchanged => {}
        }

        self.send_event(PlaybackEvent::QueueUpdated);
    }

    /// 时间戳有变化时发出一条 [`PositionChanged`] 事件。
    fn update_ts(&mut self, force: bool) {
        if let Some(timestamp) = self.engine.position_ms() {
            self.last_timestamp = timestamp;

            if timestamp == self.last_broadcast_timestamp {
                return;
            }

            if !force {
                let min_interval = if self.position_broadcast_active {
                    ACTIVE_POSITION_BROADCAST_INTERVAL_MS
                } else {
                    BACKGROUND_POSITION_BROADCAST_INTERVAL_MS
                };

                if timestamp > self.last_broadcast_timestamp
                    && self.last_broadcast_timestamp.saturating_add(min_interval) > timestamp
                {
                    return;
                }
            }

            self.send_event(PlaybackEvent::PositionChanged(timestamp));
            self.last_broadcast_timestamp = timestamp;
        }
    }

    /// 跳转到指定时间戳（单位：秒）。
    fn seek(&mut self, timestamp: f64) {
        if let Err(e) = self.engine.seek(timestamp) {
            warn!("Failed to seek: {:?}", e);
        } else {
            self.update_ts(true);
        }
    }

    /// 跳转到队列中的指定下标。
    fn jump(&mut self, index: usize) {
        match self.queue.jump(index) {
            JumpResult::Jumped { path } => {
                self.set_stop_after_current(false);
                self.open_or_log(&path);
                self.send_event(PlaybackEvent::QueuePositionChanged(index));
            }
            JumpResult::OutOfBounds => {
                warn!("Jump index {} out of bounds", index);
            }
        }
    }

    /// 忽略随机播放，跳转到队列中的指定下标：播放的是原始队列里该下标上的条目，
    /// 而不是洗牌后的条目。返回是否打开了曲目。
    fn jump_unshuffled(&mut self, index: usize) -> bool {
        match self.queue.jump_unshuffled(index) {
            JumpResult::Jumped { path } => {
                self.set_stop_after_current(false);
                self.open_or_log(&path);
                // 取在（可能已洗牌的）队列里的真实位置
                if let Some(pos) = self.queue.current_position() {
                    self.send_event(PlaybackEvent::QueuePositionChanged(pos));
                }
                true
            }
            JumpResult::OutOfBounds => {
                warn!("Jump unshuffled index {} out of bounds", index);
                false
            }
        }
    }

    /// 用给定的一组路径替换当前队列。
    fn replace_queue(&mut self, paths: Vec<QueueItemData>) {
        debug!(
            count = paths.len(),
            first = %paths
                .first()
                .map(|item| item.get_path().display().to_string())
                .unwrap_or_default(),
            "Replacing queue"
        );
        self.set_stop_after_current(false);

        match self.queue.replace_queue(paths) {
            ReplaceResult::Replaced { first_item } => {
                self.refresh_rg_auto_hint();
                if first_item.is_some()
                    && let Some(first_index) = self.queue.first_playable_index()
                {
                    self.jump(first_index);
                }
            }
            ReplaceResult::Empty => {
                self.refresh_rg_auto_hint();
                self.stop();
            }
        }

        self.send_event(PlaybackEvent::QueueUpdated);
    }

    fn replace_queue_with_index(&mut self, paths: Vec<QueueItemData>, idx: usize) {
        self.set_stop_after_current(false);

        match self.queue.replace_queue(paths) {
            ReplaceResult::Replaced { .. } => {
                self.refresh_rg_auto_hint();
                // 所请求的槽位可能已经没了（恢复的歌单变短，或洗牌映射不再包含它）。
                // 游标留空在过去会让正在播的东西继续播、而 UI 丢了位置；这里像
                // `replace_queue` 那样退回到第一个可播槽位。
                // `first_playable_index` 是（可能已洗牌的）播放队列的下标，
                // 所以此处用普通的 jump。
                if !self.jump_unshuffled(idx)
                    && let Some(first) = self.queue.first_playable_index()
                {
                    self.jump(first);
                }
            }
            ReplaceResult::Empty => {
                self.refresh_rg_auto_hint();
                self.stop();
            }
        }

        self.send_event(PlaybackEvent::QueueUpdated);
    }

    /// 清空当前队列。
    fn clear_queue(&mut self) {
        self.set_stop_after_current(false);

        let keep_current = self.playback_settings.keep_current_on_queue_clear
            && self.state() != PlaybackState::Stopped;
        self.queue.clear(keep_current);
        self.refresh_rg_auto_hint();

        if !keep_current {
            self.stop();
        }

        self.send_event(PlaybackEvent::QueuePositionChanged(
            self.queue.current_position().unwrap_or(0),
        ));
        self.send_event(PlaybackEvent::QueueUpdated);
    }

    /// 停止当前播放。
    fn stop(&mut self) {
        self.set_stop_after_current(false);
        self.engine.stop();
        self.engine.drop_prepared();
        self.last_track_gain = None;
        self.last_album_gain = None;
        self.last_track_peak = None;
        self.last_album_peak = None;
        self.duration_ms = None;

        self.send_event(PlaybackEvent::StateChanged(PlaybackState::Stopped));
    }

    fn consume_current_track(&mut self) {
        if self.playback_settings.consume
            && let Some(current_idx) = self.queue.current_position()
            && let DequeueResult::RemovedCurrent { .. } = self.queue.dequeue(current_idx)
        {
            self.refresh_rg_auto_hint();
            self.send_event(PlaybackEvent::QueueUpdated);
        }
    }

    fn toggle_stop_after_current(&mut self) {
        if self.state() != PlaybackState::Stopped {
            self.set_stop_after_current(!self.stop_after_current);
        }
    }

    fn set_stop_after_current(&mut self, stop_after_current: bool) {
        if self.stop_after_current == stop_after_current {
            return;
        }

        self.stop_after_current = stop_after_current;
        self.send_event(PlaybackEvent::StopAfterCurrentChanged(stop_after_current));
    }

    /// 切换随机播放模式。这会导致队列被复制一份并洗牌。
    fn toggle_shuffle(&mut self) {
        let (shuffled, new_position) = match self.queue.toggle_shuffle() {
            ShuffleResult::Shuffled => (true, None),
            ShuffleResult::Unshuffled { new_position } => (false, Some(new_position)),
        };

        self.refresh_rg_auto_hint();
        self.send_event(PlaybackEvent::ShuffleToggled(shuffled));
        self.send_event(PlaybackEvent::QueueUpdated);

        if let Some(new_position) = new_position {
            self.send_event(PlaybackEvent::QueuePositionChanged(new_position));
        }
    }

    /// 把随机播放强制设为指定值。SMTC / MPRIS 递来的是明确的开/关请求，
    /// 在那里做切换会朝反方向翻。
    fn set_shuffle(&mut self, shuffle: bool) {
        if self.queue.shuffled() == shuffle {
            return;
        }
        self.toggle_shuffle();
    }

    /// 设置播放流的音量。
    fn set_volume(&mut self, volume: f64) {
        if let Err(e) = self.engine.set_volume(volume) {
            warn!("Failed to set volume: {:?}", e);
        }

        self.send_event(PlaybackEvent::VolumeChanged(volume));
    }

    /// 设置循环模式。
    fn set_repeat(&mut self, state: RepeatState) {
        self.queue.set_repeat(state);
        self.engine.set_looping(state == RepeatState::RepeatingOne);

        self.send_event(PlaybackEvent::RepeatChanged(self.queue.repeat_state()));
    }

    /// 在播放/暂停之间切换。
    #[cfg(not(target_os = "windows"))]
    fn toggle_play_pause(&mut self) {
        match self.state() {
            PlaybackState::Playing => self.pause(),
            PlaybackState::Paused => self.play(),
            _ => {}
        }
    }

    /// 处理播放设置发生变更。
    fn settings_changed(&mut self, settings: PlaybackSettings) {
        self.engine.update_settings(&settings);
        self.queue.update_settings(settings.clone());
        self.playback_settings = settings;
        self.send_event(PlaybackEvent::RepeatChanged(self.queue.repeat_state()));
        // 补算 hint：非 Auto 档它长期短路在 Off 上，切进 Auto 的瞬间必须
        // 有一次真实值，否则 reapply 会按上一次离开 Auto 时的陈旧 hint 取增益。
        self.recompute_rg_auto_hint();
        self.reapply_replaygain();
    }

    /// 实时应用新的均衡器设置。持久化由 save_settings 另行负责。
    fn set_equalizer(&mut self, settings: EqualizerSettings) {
        self.engine.set_equalizer(&settings);
        self.playback_settings.equalizer = settings;
    }

    fn set_position_broadcast_active(&mut self, active: bool) {
        self.position_broadcast_active = active;
        self.update_ts(true);
    }

    /// 让引擎处理音频样本并送往设备。返回引擎在这个周期是否有前进。
    fn play_audio(&mut self) -> bool {
        match self.engine.process_cycle() {
            EngineCycleResult::Continue => {
                self.update_ts(false);
                true
            }
            EngineCycleResult::Eof => {
                if self.stop_after_current {
                    info!("EOF, stopping after current track");
                    self.consume_current_track();
                    self.stop();
                } else {
                    info!("EOF, moving to next song");
                    self.next(false, true);
                }
                true
            }
            EngineCycleResult::FatalError(msg) => {
                if self.stop_after_current {
                    error!("Fatal error in audio engine: {}, stopping playback", msg);
                    self.consume_current_track();
                    self.stop();
                } else {
                    error!("Fatal error in audio engine: {}, moving to next song", msg);
                    self.next(false, false);
                }
                true
            }
            EngineCycleResult::NothingToDo => false,
        }
    }

    fn send_event(&mut self, event: PlaybackEvent) {
        // 队列变更可能改变接下来要播哪一曲：从此刻起无缝衔接的预打开（若有）就
        // 已过期——但仅限"下一条条目确实变了"的情况。别处的编辑（元数据刷新、
        // 不触及 next 的重排）留下的预打开仍然有效，不必在预备冷却窗口里重付
        // 一次完整打开。
        if matches!(event, PlaybackEvent::QueueUpdated) {
            let next = self.queue.peek_next_path();
            self.engine.drop_prepared_unless_next(next.as_deref());
        }
        // Err 只代表 UI（连同它的接收端）已经没了
        let _ = self.events_tx.send(event);
    }
}
