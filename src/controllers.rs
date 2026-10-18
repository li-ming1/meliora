#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod mpris;
#[cfg(target_os = "windows")]
mod windows;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Weak},
};

use async_trait::async_trait;
use gpui::{App, Entity, Global, Window};
use raw_window_handle::HasWindowHandle;
use tokio::sync::mpsc::Sender;
use tracing::{Instrument as _, debug, debug_span, error, info, trace_span, warn};

use crate::{
    media::metadata::Metadata,
    playback::{events::RepeatState, interface::PlaybackInterface, thread::PlaybackState},
    ui::models::{ImageEvent, Models, PlaybackInfo},
};

#[async_trait]
/// 把外部控制器（如系统媒体控制键）接到 Meliora 上。
///
/// 打开新文件时事件按此顺序发出：`new_file -> duration_changed
/// -> metadata_changed -> album_art_changed`；其中 `metadata_changed` 与
/// `album_art_changed` 仅在该曲目确实带有元数据 / 专辑封面时才发出。
/// 并非所有曲目都有元数据：仍应显示文件名，并允许控制播放。
///
/// 控制器由各平台模块的 `init` 关联函数创建，返回装箱的 [`PlaybackController`]。
///
/// 可同时挂载多个控制器；它们收到完全相同的事件与数据。并非每个
/// `PlaybackController` 都必须处理所有事件——不想处理某个事件时，
/// 直接实现该函数并返回 `Ok(())` 即可。
///
/// 本 trait 的所有实现都必须前置 `#[async_trait]`，来自 [`async_trait`] crate。
pub trait PlaybackController: Send {
    /// 表示当前文件的播放位置发生变化。
    async fn position_changed(&mut self, new_position: u64) -> anyhow::Result<()>;

    /// 表示当前文件的时长发生变化。每个文件只应发生一次。
    async fn duration_changed(&mut self, new_duration: u64) -> anyhow::Result<()>;

    /// 表示播放音量发生变化。
    async fn volume_changed(&mut self, new_volume: f64) -> anyhow::Result<()>;

    /// 本 controller 是否消费 `volume_changed`。Windows SMTC 与 macOS 实现
    /// 把它当 no-op，因此它们的 volume 观察者整体跳过；MPRIS（Linux）确实
    /// 消费这些事件。编译期折叠为常量，等价于各平台实现分别覆写，
    /// 但不需要触碰平台模块。
    fn handles_volume(&self) -> bool {
        cfg!(target_os = "linux")
    }

    /// 表示解码器给出了新的元数据。每首曲目可能出现多次。
    async fn metadata_changed(&mut self, metadata: &Metadata) -> anyhow::Result<()>;

    /// 表示解码器给出了新的专辑封面。每首曲目可能出现多次。
    async fn album_art_changed(&mut self, album_art: &[u8]) -> anyhow::Result<()>;

    /// 表示单曲循环状态发生变化。
    async fn repeat_state_changed(&mut self, repeat_state: RepeatState) -> anyhow::Result<()>;

    /// 表示播放状态发生变化。当传入状态为 [`PlaybackState::Stopped`] 时，
    /// 队列中没有任何待播放文件。
    async fn playback_state_changed(&mut self, playback_state: PlaybackState)
    -> anyhow::Result<()>;

    /// 表示随机播放状态发生变化。
    async fn shuffle_state_changed(&mut self, shuffling: bool) -> anyhow::Result<()>;

    /// 表示开始播放新文件。收到该事件时，元数据、时长、位置和专辑封面都应
    /// 重置为默认/空值。
    async fn new_file(&mut self, path: &Path) -> anyhow::Result<()>;
}

/// 启动时挂载的平台播放控制器；仅在桌面集成初始化成功时存在。
type ControllerList = Option<Box<dyn PlaybackController>>;

pub struct PbcHandle {
    /// 下方观察者写入播放控制器事件的发送端。
    event_tx: Sender<PbcEvent>,
    /// 上次交给播放控制器的封面之 `Arc` 身份。播放线程对同一专辑的每首曲目
    /// 都会重新发出同一内嵌封面；没有这个判断，后台整段会话里每次发出都会
    /// 重建一次 WinRT 缩略图（MB 级瞬时分配）。
    last_art: Option<Weak<[u8]>>,
    /// 上次发给控制器的位置（秒）。窗口聚焦时位置事件以 30 Hz 触发，但该值
    /// 每秒才变一次；逐个转发会让 Windows 控制器每个事件白做两次 WinRT 调用。
    last_position_secs: u64,
    /// 初始化时向 controller 查询一次的 volume 消费能力（见
    /// `PlaybackController::handles_volume`）；false 时 volume 实体不注册
    /// 观察者。
    handles_volume: bool,
}

impl Global for PbcHandle {}

#[derive(derive_more::Debug)]
enum PbcEvent {
    MetadataChanged(#[debug(skip)] Box<Metadata>),
    AlbumArtChanged(#[debug(skip)] Arc<[u8]>),
    PositionChanged(u64),
    DurationChanged(u64),
    NewFile(PathBuf),
    VolumeChanged(f64),
    RepeatStateChanged(RepeatState),
    PlaybackStateChanged(PlaybackState),
    ShuffleStateChanged(bool),
}

impl PbcEvent {
    async fn handle_event(&self, pbc: &mut dyn PlaybackController) -> anyhow::Result<()> {
        match self {
            Self::MetadataChanged(metadata) => pbc.metadata_changed(metadata).await,
            Self::AlbumArtChanged(art) => pbc.album_art_changed(art).await,
            Self::PositionChanged(pos) => pbc.position_changed(*pos).await,
            Self::DurationChanged(dur) => pbc.duration_changed(*dur).await,
            Self::NewFile(path) => pbc.new_file(path).await,
            Self::VolumeChanged(vol) => pbc.volume_changed(*vol).await,
            Self::RepeatStateChanged(state) => pbc.repeat_state_changed(*state).await,
            Self::PlaybackStateChanged(state) => pbc.playback_state_changed(*state).await,
            Self::ShuffleStateChanged(shuffle) => pbc.shuffle_state_changed(*shuffle).await,
        }
    }
}

/// 播放控制器事件队列的容量。要大到能吸收一阵连续切歌事件，又要小到让消费方
/// 停滞时（WinRT 调用偶尔会卡在 shell 上）堆不出封面体积的载荷：
/// `AlbumArtChanged` 携带完整内嵌封面，64 个槽位可能钉住约 64 张封面
/// （高解析度下单张 MB 级）的陈旧事件，而 16 仍能吸收多曲连跳（每曲约 5 个
/// 事件）。队列满时 `send_pbc_event` 的 `try_send` 语义 shed 掉最新事件，
/// 反正下一首曲目就会刷新 SMTC 状态。
const PBC_CHANNEL_CAP: usize = 16;

/// 发送一个播放控制器事件；消费方落后时 shed 负载。丢弃最新事件对每个变体都
/// 安全：SMTC 状态反正会被下一首曲目刷新，而 shell 卡住只是让更新稍晚一点。
fn send_pbc_event(tx: &Sender<PbcEvent>, event: PbcEvent) {
    use tokio::sync::mpsc::error::TrySendError;
    match tx.try_send(event) {
        Ok(()) => {}
        Err(TrySendError::Full(event)) => {
            debug!(?event, "pbc queue full; dropping event");
        }
        Err(TrySendError::Closed(event)) => {
            error!(?event, "pbc channel closed; event dropped");
        }
    }
}

/// 观察 `entity`，把每次通知都转成 `make_event` 构造的事件发给播放控制器。
fn forward_pbc_events<T: 'static>(
    cx: &mut App,
    entity: &Entity<T>,
    make_event: impl Fn(&T) -> PbcEvent + 'static,
) {
    cx.observe(entity, move |e, cx| {
        let event = make_event(e.read(cx));
        let handle = cx.global::<PbcHandle>();
        send_pbc_event(&handle.event_tx, event);
    })
    .detach();
}

pub fn register_pbc_event_handlers(cx: &mut App) {
    let handles_volume = cx.global::<PbcHandle>().handles_volume;
    let models = cx.global::<Models>();
    let metadata = models.metadata.clone();
    let albumart = models.albumart.clone();

    let playback_info = cx.global::<PlaybackInfo>();
    let position = playback_info.position.clone();
    let duration = playback_info.duration.clone();
    let track = playback_info.current_track.clone();
    let volume = playback_info.volume.clone();
    let repeat = playback_info.repeating.clone();
    let state = playback_info.playback_state.clone();
    let shuffle = playback_info.shuffling.clone();

    cx.observe(&track, |e, cx| {
        if let Some(track) = e.read(cx) {
            let path = track.get_path().clone();
            let handle = cx.global_mut::<PbcHandle>();
            // new_file 会清掉 SMTC 缩略图，所以下一次封面事件必须重发，
            // 哪怕内容与之前发出的完全相同。
            handle.last_art = None;
            send_pbc_event(&handle.event_tx, PbcEvent::NewFile(path));
        }
    })
    .detach();

    forward_pbc_events(cx, &metadata, |meta| {
        PbcEvent::MetadataChanged(Box::new(meta.clone()))
    });
    forward_pbc_events(cx, &duration, |dur| PbcEvent::DurationChanged(*dur / 1_000));
    // 不消费 volume 的 controller（Windows SMTC / macOS）连观察者都不注册，
    // 省去向 PBC 队列推送注定为 no-op 的事件。
    if handles_volume {
        forward_pbc_events(cx, &volume, |vol| PbcEvent::VolumeChanged(*vol));
    }
    forward_pbc_events(cx, &repeat, |repeat| PbcEvent::RepeatStateChanged(*repeat));
    forward_pbc_events(cx, &state, |state| PbcEvent::PlaybackStateChanged(*state));
    forward_pbc_events(cx, &shuffle, |shuffle| {
        PbcEvent::ShuffleStateChanged(*shuffle)
    });

    cx.subscribe(&albumart, |_, ImageEvent(img), cx| {
        let handle = cx.global_mut::<PbcHandle>();
        // 相同封面去重：重复发出时重建 SMTC 缩略图，在长时间后台会话里纯属空转。
        let already_sent = handle
            .last_art
            .as_ref()
            .and_then(|last| last.upgrade())
            .is_some_and(|current| Arc::ptr_eq(&current, img));
        if already_sent {
            return;
        }
        handle.last_art = Some(Arc::downgrade(img));
        send_pbc_event(&handle.event_tx, PbcEvent::AlbumArtChanged(img.clone()));
    })
    .detach();

    cx.observe(&position, |e, cx| {
        let &pos = e.read(cx);
        let secs = pos / 1_000;
        // 位置广播每 33-250 ms 到达一次，但秒值每秒才变。跳过这成百上千次重复
        // 转发，SMTC 时间线就不会以 4-30 Hz 反复重建。先用只读
        // global() 比较再取可变借用：global_mut 每次调用都入队一个
        // NotifyGlobalObservers effect（本仓未注册任何 global 观察者），
        // 30Hz 下去重通过也推空 effect。
        let handle = cx.global::<PbcHandle>();
        if handle.last_position_secs == secs {
            return;
        }
        let handle = cx.global_mut::<PbcHandle>();
        handle.last_position_secs = secs;
        send_pbc_event(&handle.event_tx, PbcEvent::PositionChanged(secs));
    })
    .detach();
}

pub fn init_pbc_task(cx: &mut App, window: &Window) {
    let mut controller: ControllerList = None;

    let cmd_tx = cx.global::<PlaybackInterface>().get_sender();

    let rwh = if cfg!(target_os = "linux") {
        // X11 窗口调用它会 panic（unimplemented），而这里用不到
        None
    } else {
        HasWindowHandle::window_handle(window)
            .ok()
            .map(|v| v.as_raw())
    };

    #[cfg(target_os = "macos")]
    {
        match macos::MacMediaPlayerController::init(cmd_tx, rwh) {
            Ok(macos_pc) => controller = Some(macos_pc),
            Err(_) => {
                error!("Failed to initialize MacMediaPlayerController!");
                warn!("Desktop integration will be unavailable.");
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        match mpris::MprisController::init(cmd_tx, rwh) {
            Ok(mpris_pc) => controller = Some(mpris_pc),
            Err(_) => {
                error!("Failed to initialize MprisController!");
                warn!("Desktop integration will be unavailable.");
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        match windows::WindowsController::init(cmd_tx, rwh) {
            Ok(windows_pc) => controller = Some(windows_pc),
            Err(_) => {
                error!("Failed to initialize WindowsController!");
                warn!("Desktop integration will be unavailable.");
            }
        }
    }

    let (pbc_tx, mut pbc_rx) = tokio::sync::mpsc::channel::<PbcEvent>(PBC_CHANNEL_CAP);
    // controller 随后 move 进消费任务，能力查询必须在其 move 之前完成。
    let handles_volume = controller
        .as_ref()
        .map(|c| c.handles_volume())
        .unwrap_or(true);
    crate::RUNTIME.spawn(async move {
        let span = debug_span!("pbc_task");

        while let Some(event) = pbc_rx.recv().await {
            let span = trace_span!(parent: &span, "handle_all", ?event);
            if let Some(pbc) = controller.as_mut()
                && let Err(err) = event
                    .handle_event(pbc.as_mut())
                    .instrument(span.clone())
                    .await
            {
                error!(?err, "playback controller: {err}");
            }
        }

        info!("channel closed, ending task");
    });

    cx.set_global(PbcHandle {
        event_tx: pbc_tx,
        last_art: None,
        last_position_secs: 0,
        handles_volume,
    });
}
