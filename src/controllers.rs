#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod mpris;
#[cfg(target_os = "windows")]
mod windows;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use gpui::{App, Global, Window};
use raw_window_handle::HasWindowHandle;
use tokio::sync::mpsc::Sender;
use tracing::{Instrument as _, debug, debug_span, error, trace_span, warn};

use crate::{
    media::metadata::Metadata,
    playback::{
        events::RepeatState,
        interface::PlaybackInterface,
        thread::PlaybackState,
    },
    ui::models::{ImageEvent, Models, PlaybackInfo},
};

#[async_trait]
/// Connects external controllers (like the system's media controls) to Meliora.
///
/// When a new file is opened, events are emitted in this order: `new_file -> duration_changed
/// -> metadata_changed -> album_art_changed`, with `metadata_changed` and `album_art_changed`
/// occurring only if the track being played has metadata and album art, respectively. Not all
/// tracks will have metadata: you should still display the file name for a track and allow
/// controlling of playback.
///
/// Controllers are created by each platform module's `init` associated function, which
/// returns a boxed [`PlaybackController`].
///
/// Multiple controllers can be attached at once; they will all be sent the same events and the
/// same data. Not all `PlaybackController`s must handle all events - if you wish not to handle
/// a given event, simply implement the function by returning `Ok(())`.
///
/// All implementations of this trait should be preceeded by `#[async_trait]`, from the
/// [`async_trait`] crate.
pub trait PlaybackController: Send {
    /// Indicates that the position in the current file has changed.
    async fn position_changed(&mut self, new_position: u64) -> anyhow::Result<()>;

    /// Indicates that the duration of the current file has changed. This should only occur once
    /// per file.
    async fn duration_changed(&mut self, new_duration: u64) -> anyhow::Result<()>;

    /// Indicates that the playback volume has changed.
    async fn volume_changed(&mut self, new_volume: f64) -> anyhow::Result<()>;

    /// Indicates that new metadata has been recieved from the decoder. This may occur more than
    /// once per track.
    async fn metadata_changed(&mut self, metadata: &Metadata) -> anyhow::Result<()>;

    /// Indicates that new album art has been recieved from the decoder. This may occur more than
    /// once per track.
    async fn album_art_changed(&mut self, album_art: &[u8]) -> anyhow::Result<()>;

    /// Indicates that the repeat state has changed.
    async fn repeat_state_changed(&mut self, repeat_state: RepeatState) -> anyhow::Result<()>;

    /// Indicates that the playback state has changed. When the provided state is
    /// [`PlaybackState::Stopped`], no file is queued for playback.
    async fn playback_state_changed(&mut self, playback_state: PlaybackState)
    -> anyhow::Result<()>;

    /// Indicates that the shuffle state has changed.
    async fn shuffle_state_changed(&mut self, shuffling: bool) -> anyhow::Result<()>;

    /// Indicates that a new file has started playing. The metadata, duration, position, and album
    /// art should be reset to default/empty values when this event is recieved.
    async fn new_file(&mut self, path: &Path) -> anyhow::Result<()>;
}

/// The platform playback controller attached at startup, if desktop integration
/// initialized successfully.
type ControllerList = Option<Box<dyn PlaybackController>>;

// has to be held in memory
#[allow(dead_code)]
pub struct PbcHandle(
    Sender<PbcEvent>,
    tokio::task::JoinHandle<()>,
    /// `Arc` identity of the artwork last handed to the playback controller.
    /// The playback thread re-emits the same embedded cover for every track
    /// of an album; without this, each emission rebuilt the WinRT thumbnail
    /// (transient MB-scale allocations) over the whole background session.
    Option<std::sync::Weak<[u8]>>,
    /// Last position (seconds) sent to the controller. Position events fire
    /// at 30 Hz while a window is focused, but the value only changes once a
    /// second; forwarding each one makes the Windows controller perform two
    /// WinRT calls per event for nothing.
    u64,
);

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

/// Capacity of the playback-controller event queue. Large enough to absorb a
/// burst of track-change events, small enough that a stalled consumer (WinRT
/// calls occasionally block on the shell) cannot pile up cover-sized payloads:
/// `AlbumArtChanged` carries the full embedded artwork, so 64 slots could pin
/// ~64 covers (MB-scale each for hi-res art) of stale events, while 16 still
/// absorbs a multi-track burst (~5 events per track). A full queue sheds the
/// newest event via `try_send` semantics in `send_pbc_event`, and the next
/// track refreshes SMTC state anyway.
const PBC_CHANNEL_CAP: usize = 16;

/// Sends a playback-controller event, shedding load when the consumer has
/// fallen behind. Dropping the newest event is safe for every variant: SMTC
/// state is refreshed by the next track anyway, and a stalled shell only
/// delays the update momentarily.
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

pub fn register_pbc_event_handlers(cx: &mut App) {
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
        if let Some(track) = e.read(cx)
            && let path = track.get_path().clone()
        {
            let PbcHandle(tx, _, last_art, _) = cx.global_mut::<PbcHandle>();
            // new_file clears the SMTC thumbnail, so the next artwork event
            // must re-send even if identical to what we sent before.
            *last_art = None;
            send_pbc_event(tx, PbcEvent::NewFile(path));
        }
    })
    .detach();

    cx.observe(&metadata, |e, cx| {
        let meta = e.read(cx).clone();
        let PbcHandle(tx, ..) = cx.global();
        send_pbc_event(tx, PbcEvent::MetadataChanged(Box::new(meta)));
    })
    .detach();

    cx.subscribe(&albumart, |_, ImageEvent(img), cx| {
        let PbcHandle(tx, _, last_art, _) = cx.global_mut::<PbcHandle>();
        // Deduplicate identical artwork: rebuilding the SMTC thumbnail for a
        // repeated emission is pure churn over a long background session.
        let already_sent = last_art
            .as_ref()
            .and_then(|last| last.upgrade())
            .is_some_and(|current| std::sync::Arc::ptr_eq(&current, &img));
        if already_sent {
            return;
        }
        *last_art = Some(std::sync::Arc::downgrade(&img));
        send_pbc_event(tx, PbcEvent::AlbumArtChanged(img.clone()));
    })
    .detach();

    cx.observe(&position, |e, cx| {
        let &pos = e.read(cx);
        let secs = pos / 1_000;
        let PbcHandle(tx, _, _, last_secs) = cx.global_mut::<PbcHandle>();
        // Position broadcasts arrive every 33-250 ms, but the second value
        // only changes once per second. Skip the hundreds of duplicate
        // forwards so the SMTC timeline isn't rebuilt at 4-30 Hz.
        if *last_secs == secs {
            return;
        }
        *last_secs = secs;
        send_pbc_event(tx, PbcEvent::PositionChanged(secs));
    })
    .detach();

    cx.observe(&duration, |e, cx| {
        let &dur = e.read(cx);
        let PbcHandle(tx, ..) = cx.global();
        send_pbc_event(tx, PbcEvent::DurationChanged(dur / 1_000));
    })
    .detach();

    cx.observe(&volume, |e, cx| {
        let &vol = e.read(cx);
        let PbcHandle(tx, ..) = cx.global();
        send_pbc_event(tx, PbcEvent::VolumeChanged(vol));
    })
    .detach();

    cx.observe(&repeat, |e, cx| {
        let &repeat = e.read(cx);
        let PbcHandle(tx, ..) = cx.global();
        send_pbc_event(tx, PbcEvent::RepeatStateChanged(repeat));
    })
    .detach();

    cx.observe(&state, |e, cx| {
        let &state = e.read(cx);
        let PbcHandle(tx, ..) = cx.global();
        send_pbc_event(tx, PbcEvent::PlaybackStateChanged(state));
    })
    .detach();

    cx.observe(&shuffle, |e, cx| {
        let &shuffle = e.read(cx);
        let PbcHandle(tx, ..) = cx.global();
        send_pbc_event(tx, PbcEvent::ShuffleStateChanged(shuffle));
    })
    .detach();
}

pub fn init_pbc_task(cx: &mut App, window: &Window) {
    let mut controller: ControllerList = None;

    let cmd_tx = cx.global::<PlaybackInterface>().get_sender();

    let rwh = if cfg!(target_os = "linux") {
        // X11 windows panic with unimplemented and we don't need it here
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
    let task = crate::RUNTIME.spawn(async move {
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

        tracing::info!("channel closed, ending task");
    });

    cx.set_global(PbcHandle(pbc_tx, task, None, 0));
}
