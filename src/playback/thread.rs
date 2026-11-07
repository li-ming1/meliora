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

use audio_engine::{AudioEngine, EngineCycleResult, EngineState, OnlineRefresh, PrepareReport};
use queue_manager::{
    DequeueManyResult, DequeueResult, InsertResult, JumpResult, MoveResult, QueueManager,
    QueueNavigationResult, ReplaceResult, Reshuffled, ShuffleResult, UndoResult,
};

// 对位置广播节流，避免占用过多 CPU，应用未获得焦点时尤其如此
const ACTIVE_POSITION_BROADCAST_INTERVAL_MS: u64 = 33;
const BACKGROUND_POSITION_BROADCAST_INTERVAL_MS: u64 = 250;

/// 播放中连续多少个无进展周期之后跳过当前曲目。
const MAX_NO_PROGRESS_CYCLES: u32 = 50;

/// 设备重建退避窗内的轮询间隔：窗口 2-15 s，100 ms 轮询 = 每窗 20-150 次
/// 重试探询，设备恢复后额外延迟 ≤100 ms（音频已死秒级，无感）。
const DEVICE_BACKOFF_POLL_INTERVAL_MS: u64 = 100;

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

/// 开播稳定延时：当前曲目稳定播放超过此阈值后，提前为下一曲布防预取（秒播引擎），
/// 兼顾防抖（快速连跳时不触发无谓预取）与任意时刻切歌的瞬时响应。
const STABLE_PREPARE_DELAY_MS: u64 = 3_000;

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
    /// 命令批次的复用缓冲：稳态与命令密集期都不再每周期新建两个 Vec。
    command_batch: Vec<PlaybackCommand>,
    /// 启动后的首个在线打开是否要先刷新 URL：线程构造时置位（恢复出来的会话
    /// 里当前曲目是 HTTP 直链——签名 URL 跨进程必然过期）。见
    /// [`Self::open_with_resampler`] 的先行刷新分支。
    #[cfg(feature = "online_sources")]
    initial_online_open: bool,
}

/// 就地折叠连续 Next / Seek，只保留每串最后一个。用 swap 搬运，
/// 不分配、不需要 Clone；容量由调用方的复用缓冲提供。
fn collapse_commands(batch: &mut Vec<PlaybackCommand>) {
    let mut write = 0usize;
    for read in 0..batch.len() {
        let repeats_last = write > 0
            && matches!(
                (&batch[read], &batch[write - 1]),
                (PlaybackCommand::Next, PlaybackCommand::Next)
                    | (PlaybackCommand::Seek(_), PlaybackCommand::Seek(_))
            );
        if repeats_last {
            batch.swap(write - 1, read);
        } else {
            if write != read {
                batch.swap(write, read);
            }
            write += 1;
        }
    }
    batch.truncate(write);
}

#[cfg(test)]
mod collapse_bench {
    use super::*;
    use crate::test_support::alloc_guard::count_allocations;

    fn template() -> Vec<PlaybackCommand> {
        let mut commands = Vec::new();
        for i in 0..8 {
            commands.push(PlaybackCommand::Play);
            commands.push(PlaybackCommand::Next);
            commands.push(PlaybackCommand::Next);
            commands.push(PlaybackCommand::Seek(i as f64));
            commands.push(PlaybackCommand::Seek(i as f64 + 0.5));
            commands.push(PlaybackCommand::Pause);
        }
        commands
    }

    /// 优化前的形状：每周期新建两个 Vec。
    fn legacy(commands: Vec<PlaybackCommand>) -> Vec<PlaybackCommand> {
        let mut collapsed = Vec::with_capacity(commands.len());
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
        collapsed
    }

    #[test]
    fn in_place_folding_is_allocation_free_and_equivalent() {
        let source = template();
        let expected = legacy(source.clone());

        let mut batch = Vec::with_capacity(256);
        let ((), reuse_allocations) = count_allocations(|| {
            for _ in 0..2 {
                batch.clear();
                batch.extend_from_slice(&source);
                collapse_commands(&mut batch);
                std::hint::black_box(&batch);
            }
        });
        let (_, legacy_allocations) = count_allocations(|| {
            for _ in 0..2 {
                std::hint::black_box(legacy(source.clone()));
            }
        });

        eprintln!(
            "command folding allocations/2 batches: reuse={reuse_allocations} legacy={legacy_allocations}"
        );
        assert_eq!(reuse_allocations, 0, "复用缓冲的折叠必须零分配");
        assert!(legacy_allocations >= 4, "旧形状每批至少两个 Vec");
        assert_eq!(batch, expected, "折叠语义必须与旧实现逐项一致");
    }
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
                // 守卫绑定线程闭包：泵线程与进程同寿，Drop 实际不会发生。
                let _mmcss_registration = crate::devices::mmcss::register_playback_thread();
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
                    command_batch: Vec::new(),
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

        // 收集已完成的无缝切歌预备（若有）：先回写刷新救回的路径，再校验预备
        // 落地时 next 是否仍指向它——预备在途期间 next 可能被导航/循环模式改写
        // （后者不发任何钩子事件），陈旧预备落地即弃（L-prepared 爪2）。
        if let Some(report) = self.engine.poll_prepared()
            && report.succeeded
        {
            self.handle_prepared_report(report);
        }

        if self.engine.state() == EngineState::Playing {
            if self.play_audio() {
                self.no_progress_cycles = 0;
                self.maybe_prepare_next();
            } else if self.engine.in_device_backoff() {
                // 设备重建退避窗内的无产出不是曲料的错：清零计数（抖动型故障
                // 不得向 50 周期跳曲累积——干净 consume 本就经上面的分支清零，
                // 二者一致），按退避节奏轮询而不是 2 ms 空转。真无料（媒体缺失/
                // 死流/EOF）不走此分支，跳曲语义不变。
                self.no_progress_cycles = 0;
                sleep(std::time::Duration::from_millis(
                    DEVICE_BACKOFF_POLL_INTERVAL_MS,
                ));
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
        let mut batch = std::mem::take(&mut self.command_batch);
        batch.clear();
        // 唤醒挂起 idle_wait 的那条命令排在批次最前，这样跨越唤醒的
        // Next/Seek 串照常折叠。
        if let Some(command) = self.pending_command.take() {
            batch.push(command);
        }
        while let Ok(command) = self.commands_rx.try_recv() {
            batch.push(command);
        }
        // 连续的 Next（快速连点）与 Seek（进度条拖动约 30 Hz 发出）只保留
        // 每串最后一个：每执行一次 Seek 都会重建音频流并重新锚定在线源。
        collapse_commands(&mut batch);

        for command in batch.drain(..) {
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
        // drain 后容量仍在，归还给下一个周期继续复用。
        self.command_batch = batch;
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
            self.queue.set_session_playing(false);
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
            self.queue.set_session_playing(true);
            return;
        }

        // 处于停止且队列非空时，从头开始播放
        if current_state == PlaybackState::Stopped
            && let Some((path, index)) = self.queue.first_playable_path()
        {
            // 光标先于 open 归位（F2）：open 期间的流刷新钩子（identity/display/
            // URL 回写）经 queue.current_* 解析，须指向被打开的条目而不是旧光标
            // ——次序与 jump/next 一致。
            self.queue.set_position(index);
            self.drop_stale_prepared_after_navigation();
            self.open_or_log(&path);
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
            error!(
                path = %crate::media::redact_media_path(&path.to_string_lossy()),
                ?err,
                "Unable to open file: {err}"
            );
        }
    }

    /// 跨越一次可能复用重采样器的切换时调用 [`Self::open_or_log`]
    /// （无缝切歌前进）。
    fn open_or_log_resampled(&mut self, path: &Path, preserve_resampler: bool) {
        if let Err(err) = self.open_with_resampler(path, preserve_resampler) {
            error!(
                path = %crate::media::redact_media_path(&path.to_string_lossy()),
                ?err,
                "Unable to open file: {err}"
            );
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
        let pre_refreshed = if std::mem::take(&mut self.initial_online_open)
            && crate::media::is_http_path(path)
        {
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

        info!(
            "Opening track '{}'",
            crate::media::redact_media_path(&path.to_string_lossy())
        );

        self.last_track_gain = None;
        self.last_album_gain = None;
        self.last_track_peak = None;
        self.last_album_peak = None;
        // 同步清掉旧曲时长：失败早退时若残留，maybe_prepare_next 会按旧曲的
        // 时间窗做无谓预备。成功路径在下方照旧赋新值。
        self.duration_ms = None;

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
        tracing::info!(path = %crate::media::redact_media_path(&open_path.to_string_lossy()), private_mb = private, working_mb = working, "[mem] track open");

        // 单曲循环开启时启用循环点感知解码
        self.engine
            .set_looping(self.queue.repeat_state() == RepeatState::RepeatingOne);

        self.send_event(PlaybackEvent::SongChanged(open_path));

        self.duration_ms = duration;
        self.send_event(PlaybackEvent::DurationChanged(duration.unwrap_or(0)));

        self.process_metadata_update();

        self.update_ts(true);

        self.send_event(PlaybackEvent::StateChanged(PlaybackState::Playing));
        self.queue.set_session_playing(true);

        Ok(())
    }

    /// 预打开下一曲的媒体流，让 EOF 切换与中途主动切歌都能挂上就绪的流，而不是
    /// 阻塞在网络连接+格式探测上（在线源听感上的切歌间隙就出在这里）。
    /// 双门控唤醒：
    /// 1. 当前曲稳定起播（>= 3s）：提前在后台预连并缓冲下一曲，实现任意时刻切歌秒播；
    /// 2. 临近曲尾（<= 15s）：原有无缝 EOF 兜底门。
    ///
    /// 队列变更会使已预备的流失效。若下一条目是 HTTP 路径，预备会带上 URL 刷新钩子，
    /// 过期的签名 URL 在这个提前窗口内由预备线程自行救回（刷新 + 重开），不阻塞切换。
    fn maybe_prepare_next(&mut self) {
        if self.stop_after_current || self.playback_settings.consume {
            return;
        }
        let Some(duration) = self.duration_ms else {
            return;
        };
        let near_eof = duration.saturating_sub(self.last_timestamp) <= PREPARE_LEAD_MS;
        let stable_playback = self.last_timestamp >= STABLE_PREPARE_DELAY_MS;
        if !near_eof && !stable_playback {
            return;
        }
        // 在取队列锁并克隆 PathBuf 之前的廉价守卫：预备窗口内每个解码周期都会
        // 走到这里，而下一流一旦已预备/已在途，之后的每次调用都是纯开销。
        if self.engine.is_next_prepared() {
            return;
        }
        // 失败冷却门（路径无关）：30s 失败冷却窗内每次尝试都注定被
        // prepare_rejected 拒掉，先 peek 只是白付——纯缺失尾部的逐周期
        // O(tail) stat 扫描还要持队列 RwLock 读守卫（UI 写者全程等待）。
        // 冷却键是引擎私有状态，拿不到键值比对，布尔门是唯一形态。
        if self.engine.prepare_failure_cooldown_active() {
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
                // 与「即将打开的这首」比对而非 peek_next（那已是再下一首）：
                // 用户主动跳过单曲循环目标时旧预备不匹配→清；自然 EOF 前进时
                // 预备的恰好就是 path（gapless 的全部意义所在）→保留直挂。
                // 无条件清会在每次自然切歌误杀无缝衔接流（2026-10-25 终审
                // 抓到的回归）。Unchanged 臂不比——repeat-one 下预备的就是
                // 当前曲本身，对下一次前进仍然有效。
                self.engine.drop_prepared_unless_next(Some(&path));

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
                // 光标先于 open 归位（F2）：流刷新钩子按 queue.current_* 解析，
                // 须指向被打开的条目（与 jump 次序一致）。
                self.queue.set_position(index);
                self.drop_stale_prepared_after_navigation();
                self.open_or_log(&path);
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
                // queue.previous 已把光标移到新条目：open 之前丢弃为旧"下一曲"
                // 预备/在途的预备（L-prepared 爪1）。Unchanged 臂光标未动、
                // next 未变，不加。
                self.drop_stale_prepared_after_navigation();
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
        // 光标先于 open 归位（F2）：流刷新钩子按 queue.current_* 解析，须指向
        // 被打开的条目。停止态通常无预备（stop() 已 drop），此处调用属一致性
        // 防御，早退门保证零成本。
        self.queue.set_position(first_index);
        self.drop_stale_prepared_after_navigation();
        self.open_or_log(path);
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
        // 光标先于 open 归位（F2），同 start_playing_inserted_if_stopped。
        let position = first_index + relative_idx;
        self.queue.set_position(position);
        self.drop_stale_prepared_after_navigation();
        self.open_or_log(path);
        self.send_event(PlaybackEvent::QueuePositionChanged(position));
    }

    /// 往队列新增一个 [`QueueItemData`]。若当前没在播任何东西，就开始播放它。
    fn queue_item(&mut self, item: &QueueItemData) {
        info!(
            "Adding file to queue: {}",
            crate::media::redact_media_path(&item.get_path().to_string_lossy())
        );

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
            let position = first_index + relative_idx;
            // 随机播放开启时 `queue_items` 会把追加的整批打乱，被打开的那个条目
            // 便不再位于 `first_index + relative_idx`；经原始顺序解析出它的真实
            // 槽位，让游标跟着已打开的路径走（jump 自己会持久化状态）。
            // 光标先于 open 归位（F2）：open 期间的流刷新钩子（identity/display/
            // URL 回写）按 queue.current_* 解析，须指向被打开的条目。
            let position = match self.queue.jump_unshuffled(position) {
                JumpResult::Jumped { .. } => self.queue.current_position().unwrap_or(position),
                JumpResult::OutOfBounds => {
                    self.queue.set_position(position);
                    position
                }
            };
            self.drop_stale_prepared_after_navigation();
            self.open_or_log(path);
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
        info!(
            "Inserting file to queue at position {}: {}",
            position,
            crate::media::redact_media_path(&item.get_path().to_string_lossy())
        );

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
                // queue.jump 已置光标：open 之前丢弃为旧"下一曲"预备/在途的预备
                // （L-prepared 爪1）。
                self.drop_stale_prepared_after_navigation();
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
                // 同 jump：光标已落定，open 之前丢弃陈旧预备（L-prepared 爪1）。
                self.drop_stale_prepared_after_navigation();
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
        self.queue.set_session_playing(false);
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
        // 重排改写 next 身份：旧 next 上的预备失败冷却对洗牌后的新 next 没有
        // 意义（UI 开关走的 toggle_shuffle 经 QueueUpdated 钩子清除，本入口
        // 同步语义）。
        self.engine.clear_prepare_failure_cooldown();
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
        // 循环模式改写 next 身份（列表循环回绕、单曲循环指向当前曲）：旧 next
        // 上的预备失败冷却对新 next 没有意义。RepeatChanged 不触发 send_event
        // 的 QueueUpdated 分支，这里显式清。
        self.engine.clear_prepare_failure_cooldown();

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
            // 退避窗内的静默无产出：对本函数的 bool 契约而言与 NothingToDo 同义，
            // 区别只在 main_loop 是否把它计入无进展跳曲（见 in_device_backoff 分支）。
            EngineCycleResult::DeviceBackoff => false,
        }
    }

    /// 导航改写当前曲目后，为旧"下一曲"预备/在途的预备已过期：它既压制真实
    /// 下一首的 gapless 预备（maybe_prepare_next 的 is_next_prepared 早退门），
    /// 又白持一条 HTTP 连接直到下次 open 才被丢弃。与 send_event 的
    /// QueueUpdated 钩子同一判定；导航不发 QueueUpdated，故在光标落定后、
    /// open 之前显式触发。无预备时零成本（免队列锁+stat）。
    fn drop_stale_prepared_after_navigation(&mut self) {
        if !self.engine.is_next_prepared() {
            return;
        }
        let next = self.queue.peek_next_path();
        self.engine.drop_prepared_unless_next(next.as_deref());
    }

    /// 已完成预备的落地处理：刷新救回的路径回写队列条目，随后校验预备落地时
    /// next 是否仍指向它——预备在途期间 next 可能被导航/循环模式改写（后者不
    /// 发任何钩子事件）。陈旧预备压真实下一首的预备、白持连接，落地即弃；
    /// drop_prepared 的 2s 冷却键在陈旧路径上，不阻塞真下一首的重新预备。
    /// 时序：poll_prepared 先于 play_audio 与 maybe_prepare_next 执行，此处的
    /// drop 在同一轮的 is_next_prepared 早退门之前落地，真下一首当轮即可重新
    /// 布防，无空窗。
    fn handle_prepared_report(&mut self, report: PrepareReport) {
        if report.path != report.requested {
            self.queue
                .replace_next_path(&report.requested, report.path.clone());
            // 失败冷却以*被请求的*路径为键（引擎侧 failed_prepare 记录的是
            // requested）；这次成功之后队列条目已被直改（replace_next_path
            // 不发任何事件），回写前若是失败的旧 next，冷却会无差别门禁新
            // next ≤30s——回写即身份落地，一并清。
            self.engine.clear_prepare_failure_cooldown();
        }
        let still_next = self
            .queue
            .peek_next_path()
            .is_some_and(|p| p == report.path);
        if !still_next {
            self.engine.drop_prepared();
        }
    }

    fn send_event(&mut self, event: PlaybackEvent) {
        // 队列变更可能改变接下来要播哪一曲：从此刻起无缝衔接的预打开（若有）就
        // 已过期——但仅限"下一条条目确实变了"的情况。别处的编辑（元数据刷新、
        // 不触及 next 的重排）留下的预打开仍然有效，不必在预备冷却窗口里重付
        // 一次完整打开。无预备时直接跳过 peek（省一次队列锁+stat）。
        if matches!(event, PlaybackEvent::QueueUpdated) && self.engine.is_next_prepared() {
            let next = self.queue.peek_next_path();
            self.engine.drop_prepared_unless_next(next.as_deref());
        }
        // 队列变更同时使预备失败冷却过期：冷却键记在旧 next 身份上，broken-next
        // 场景（预备失败 + 无预备流）上面的守卫块恒不进入，clear 必须独立判定，
        // 否则新 next 会被无差别门禁整个 30s 冷却窗。
        if matches!(event, PlaybackEvent::QueueUpdated) {
            self.engine.clear_prepare_failure_cooldown();
        }
        // Err 只代表 UI（连同它的接收端）已经没了
        let _ = self.events_tx.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback::tests::harness::write_wav_i16;
    use crate::test_support::{TestDir, register_test_media_providers};

    /// 组装一个真实的 PlaybackThread（真实通道 + QueueManager + 引擎），引擎
    /// 保持 Idle（不 initialize 设备）：open 只走媒体级。按 thread.rs 构造形态
    /// 用同模块私有字段直填。
    fn thread_with_queue(
        items: Vec<QueueItemData>,
    ) -> (PlaybackThread, UnboundedReceiver<PlaybackEvent>) {
        let queue = Arc::new(RwLock::new(Arc::new(items)));
        let (storage_tx, _storage_rx) = watch::channel(PlaybackSessionData::default());
        let queue_manager = QueueManager::new(
            queue,
            PlaybackSettings::default(),
            PlaybackSessionData::default(),
            storage_tx,
        );
        let (events_tx, events_rx) = unbounded_channel();
        let thread = PlaybackThread {
            playback_settings: PlaybackSettings::default(),
            commands_rx: unbounded_channel().1,
            events_tx: events_tx.clone(),
            last_timestamp: u64::MAX,
            last_broadcast_timestamp: u64::MAX,
            position_broadcast_active: true,
            engine: AudioEngine::new(events_tx, spectrum_tap().0),
            queue: queue_manager,
            initial_volume: 1.0,
            rg_auto_hint: ReplayGainAutoHint::PreferTrack,
            last_track_gain: None,
            last_album_gain: None,
            last_track_peak: None,
            last_album_peak: None,
            duration_ms: None,
            stop_after_current: false,
            no_progress_cycles: 0,
            pending_command: None,
            command_batch: Vec::new(),
            #[cfg(feature = "online_sources")]
            initial_online_open: false,
        };
        (thread, events_rx)
    }

    /// 建临时目录并写入 count 个有效 wav 条目。TestDir 句柄必须随条目存活到
    /// 测试结束（Drop 即删目录），故一并返回。
    fn wav_dir_items(prefix: &str, count: usize) -> (TestDir, Vec<QueueItemData>) {
        let dir = TestDir::new(prefix);
        let items = (0..count)
            .map(|i| {
                let path = dir.join(&format!("t{i}.wav"));
                write_wav_i16(&path, 44_100, 2, &[0_i16; 1_024 * 2]);
                QueueItemData::new(path, None, None)
            })
            .collect();
        (dir, items)
    }

    /// 收取事件流里首个 SongChanged 与首个 QueuePositionChanged 的到达序号。
    fn event_order(
        events_rx: &mut UnboundedReceiver<PlaybackEvent>,
    ) -> (Option<usize>, Option<usize>, Option<usize>) {
        let mut order = 0;
        let (mut song, mut position, mut position_value) = (None, None, None);
        while let Ok(event) = events_rx.try_recv() {
            match event {
                PlaybackEvent::SongChanged(_) if song.is_none() => song = Some(order),
                PlaybackEvent::QueuePositionChanged(idx) if position.is_none() => {
                    position = Some(order);
                    position_value = Some(idx);
                }
                _ => {}
            }
            order += 1;
        }
        (song, position, position_value)
    }

    /// 停止态 play 先把光标归位再 open（F2）：open 期间的流刷新钩子按
    /// queue.current_* 解析，须指向被打开的条目。事件序契约与 jump 一致：
    /// SongChanged 先于 QueuePositionChanged。
    #[test]
    fn stopped_play_positions_cursor_before_open() {
        register_test_media_providers();
        let (_dir, items) = wav_dir_items("hb-thread-play-cursor", 2);
        let (mut thread, mut events_rx) = thread_with_queue(items);

        thread.play();

        assert_eq!(thread.queue.current_position(), Some(0));
        let (song, position, position_value) = event_order(&mut events_rx);
        // open 成功（真实 wav）：SongChanged 必须存在且先于 QueuePositionChanged。
        assert_eq!(position_value, Some(0));
        let song = song.expect("open succeeded; SongChanged must be emitted");
        let position = position.expect("QueuePositionChanged must be emitted");
        assert!(
            song < position,
            "SongChanged must precede QueuePositionChanged"
        );
    }

    /// 停止态 previous 从最后一个可播条目起播（F2）：光标同样先于 open 归位。
    #[test]
    fn stopped_previous_positions_cursor_on_last_playable() {
        register_test_media_providers();
        let (_dir, items) = wav_dir_items("hb-thread-prev-cursor", 2);
        let (mut thread, mut events_rx) = thread_with_queue(items);

        thread.previous();

        assert_eq!(thread.queue.current_position(), Some(1));
        let (song, position, position_value) = event_order(&mut events_rx);
        assert_eq!(position_value, Some(1));
        // open 成功（真实 wav）：与 play 路径同款次序钉——光标归位先于 open。
        let song = song.expect("open succeeded; SongChanged must be emitted");
        let position = position.expect("QueuePositionChanged must be emitted");
        assert!(
            song < position,
            "SongChanged must precede QueuePositionChanged"
        );
    }

    /// 导航（jump）丢弃为旧"下一曲"布防的预备（L-prepared 爪1），且不阻塞
    /// 真实下一首的重新布防。
    #[test]
    fn navigation_drops_stale_prepare() {
        register_test_media_providers();
        let (_dir, items) = wav_dir_items("hb-thread-nav-prepared", 4);
        let (mut thread, _events_rx) = thread_with_queue(items);

        thread.jump(0);
        // 逼近曲尾，让 maybe_prepare_next 为 B（下标 1）布防。
        thread.duration_ms = Some(1_000);
        thread.last_timestamp = 0;
        thread.maybe_prepare_next();
        assert!(thread.engine.is_next_prepared());

        // 导航到 C（下标 2）：为 B 的预备已过期，jump 内的爪1 应同步丢弃。
        thread.jump(2);
        assert!(!thread.engine.is_next_prepared());

        // 真下一首 D（下标 3）不受陈旧冷却键影响，当轮即可重新布防。
        thread.maybe_prepare_next();
        assert!(thread.engine.is_next_prepared());
    }

    /// 预备落地时 next 已被（不发钩子的）导航改写：报告落地即弃，不压制
    /// 真实下一首（L-prepared 爪2）。
    #[test]
    fn stale_prepare_report_is_dropped_on_landing() {
        register_test_media_providers();
        let (_dir, items) = wav_dir_items("hb-thread-stale-report", 4);
        let (mut thread, _events_rx) = thread_with_queue(items);

        thread.jump(0);
        thread.duration_ms = Some(1_000);
        thread.last_timestamp = 0;
        thread.maybe_prepare_next();

        // 绕过爪1：直接移队列光标，模拟未发 QueueUpdated 的改写（如 set_repeat）。
        assert!(matches!(thread.queue.jump(2), JumpResult::Jumped { .. }));

        // 本地 wav 的预备线程 µs 级完成；仍给有界自旋。
        let mut report = None;
        for _ in 0..200 {
            if let Some(r) = thread.engine.poll_prepared() {
                report = Some(r);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let report = report.expect("prepare outcome must arrive within the spin budget");
        assert!(report.succeeded, "local wav prepare must succeed");

        // poll_prepared 落地后 prepared 持有 B 的流；报告处理时 next 已是 D → 弃。
        thread.handle_prepared_report(report);
        assert!(!thread.engine.is_next_prepared());
    }

    /// EOF 前进不清与「即将打开的这首」匹配的预备（终审回归钉）：next() Changed
    /// 臂的 drop 谓词按 path 比对——自然 EOF（预备==path）保留直挂、用户跳过
    /// （旧目标≠path）才清。旧写法按 peek_next 比对会把每次自然切歌的无缝
    /// 衔接流误杀成完整同步 open。
    #[test]
    fn eof_advance_predicate_keeps_matching_prepared() {
        register_test_media_providers();
        let (_dir, items) = wav_dir_items("hb-thread-eof-prepared", 3);
        // 第三首的路径：用户"跳到别处"的比对目标（items 稍后 move 进队列）
        let elsewhere = items[2].get_path().to_path_buf();
        let (mut thread, _events_rx) = thread_with_queue(items);

        thread.jump(0);
        thread.duration_ms = Some(1_000);
        thread.last_timestamp = 0;
        thread.maybe_prepare_next();
        assert!(thread.engine.is_next_prepared());

        let next_path = thread.queue.peek_next_path().expect("B prepared");
        // 自然 EOF 前进：即将打开的就是预备的这首——谓词必须保留。
        thread.engine.drop_prepared_unless_next(Some(&next_path));
        assert!(
            thread.engine.is_next_prepared(),
            "EOF advance must keep the matching prepared stream (gapless)"
        );
        // 用户跳过到别处（path 不匹配）——同一谓词必须清。
        // （current_path 访问器是 online_sources 门控的，测试直接用另一首的路径）
        thread.engine.drop_prepared_unless_next(Some(&elsewhere));
        assert!(!thread.engine.is_next_prepared());
    }

    /// 稳定起播门控（秒播引擎）：即使距离 3 分钟曲尾还非常远，开播满 3 秒后即提前布防下一曲。
    #[test]
    fn stable_playback_prefetches_next_early() {
        register_test_media_providers();
        let (_dir, items) = wav_dir_items("hb-thread-stable-prefetch", 3);
        let (mut thread, _events_rx) = thread_with_queue(items);

        thread.jump(0);
        // 当前曲目总时长 180 秒（远离 15 秒曲尾）
        thread.duration_ms = Some(180_000);

        // 开播未满 3 秒（例如连跳中途）：不预取
        thread.last_timestamp = 1_000;
        thread.maybe_prepare_next();
        assert!(!thread.engine.is_next_prepared());

        // 稳定播放满 3 秒：即刻启动后台预备
        thread.last_timestamp = 3_500;
        thread.maybe_prepare_next();
        assert!(thread.engine.is_next_prepared());
    }

    /// 预备失败冷却门（路径无关布尔门）：冷却期内 maybe_prepare_next 在取队列
    /// 锁之前就地早退，不为注定被拒的路径扫描注册表；next 身份失效源清除冷却
    /// 后当轮即可重新布防。这里钉 QueueUpdated 钩子与 set_repeat 两个失效源；
    /// 换曲 open 在引擎侧直清、落地回写在 handle_prepared_report 直清。
    #[test]
    fn prepare_failure_cooldown_gates_maybe_prepare_next() {
        register_test_media_providers();
        let (_dir, items) = wav_dir_items("hb-thread-prepare-cooldown", 3);
        let (mut thread, _events_rx) = thread_with_queue(items);

        thread.jump(0);
        thread.duration_ms = Some(180_000);
        thread.last_timestamp = 3_500;

        // 注入失败冷却（不走真实失败路径）：稳定门满足也不发起预备。
        thread.engine.force_prepare_failure_cooldown();
        thread.maybe_prepare_next();
        assert!(!thread.engine.is_next_prepared());

        // 失效源 1：QueueUpdated——send_event 的 clear 是独立判定，broken-next
        // 场景（无预备流）上面的 is_next_prepared 守卫块不进入，清除必须仍发生。
        thread.send_event(PlaybackEvent::QueueUpdated);
        assert!(!thread.engine.prepare_failure_cooldown_active());

        // 失效源 2：set_repeat——RepeatChanged 不触发 QueueUpdated 分支，靠显式清。
        thread.engine.force_prepare_failure_cooldown();
        thread.set_repeat(RepeatState::Repeating);
        assert!(!thread.engine.prepare_failure_cooldown_active());

        // 引擎级 clear（换曲 open / replace_next_path 回写走的同一个入口）之后
        // 当轮即可重新布防。
        thread.engine.clear_prepare_failure_cooldown();
        thread.maybe_prepare_next();
        assert!(thread.engine.is_next_prepared());
    }
}
