use std::{path::Path, ptr::NonNull};

use async_trait::async_trait;
use block2::RcBlock;
use objc2::{AnyThread, rc::Retained, runtime::ProtocolObject};
use objc2_app_kit::NSImage;
use objc2_core_foundation::CGSize;
use objc2_foundation::{NSData, NSMutableDictionary, NSNumber, NSString};
use objc2_media_player::{
    MPChangePlaybackPositionCommandEvent, MPMediaItemArtwork, MPMediaItemPropertyAlbumTitle,
    MPMediaItemPropertyArtist, MPMediaItemPropertyArtwork, MPMediaItemPropertyPlaybackDuration,
    MPMediaItemPropertyTitle, MPNowPlayingInfoCenter, MPNowPlayingInfoPropertyElapsedPlaybackTime,
    MPNowPlayingPlaybackState, MPRemoteCommand, MPRemoteCommandCenter, MPRemoteCommandEvent,
    MPRemoteCommandHandlerStatus,
};
use raw_window_handle::RawWindowHandle;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error};

use crate::{
    media::metadata::Metadata,
    playback::{
        events::{PlaybackCommand, RepeatState},
        thread::PlaybackState,
    },
};

use super::PlaybackController;

/// Reports Meliora's playback state to the macOS Now Playing info center and
/// forwards system remote-command events to the playback thread.
pub struct MacMediaPlayerController {
    cmd_tx: UnboundedSender<PlaybackCommand>,
}

impl MacMediaPlayerController {
    /// A fresh now-playing dictionary pre-populated from the currently
    /// published one, so partial updates keep the remaining keys.
    unsafe fn now_playing_with_previous() -> Retained<NSMutableDictionary<NSString>> {
        unsafe {
            let now_playing: Retained<NSMutableDictionary<NSString>> =
                NSMutableDictionary::dictionary();

            if let Some(prev_now_playing) = MPNowPlayingInfoCenter::defaultCenter().nowPlayingInfo()
            {
                now_playing.addEntriesFromDictionary(&prev_now_playing);
            }

            now_playing
        }
    }

    /// Publishes the now-playing dictionary to the shared info center.
    unsafe fn set_now_playing(now_playing: &NSMutableDictionary<NSString>) {
        unsafe {
            MPNowPlayingInfoCenter::defaultCenter().setNowPlayingInfo(Some(now_playing));
        }
    }

    unsafe fn new_file(&mut self, path: &Path) {
        unsafe {
            debug!("New file: {:?}", path);

            // Paths ending in ".." have no file name and non-UTF-8 names fail
            // `to_str()`; either unwrap would panic inside the pbc task,
            // unwinding it and permanently disabling now-playing integration.
            // Fall back to the full path and replace invalid UTF-8 instead.
            let file_name = path.file_name().unwrap_or_else(|| path.as_os_str());

            // Starts from an empty dictionary on purpose: a new file resets
            // the now-playing entry rather than updating it.
            let now_playing: Retained<NSMutableDictionary<NSString>> =
                NSMutableDictionary::dictionary();

            let ns_name = NSString::from_str(&file_name.to_string_lossy());
            now_playing
                .setObject_forKey(&ns_name, ProtocolObject::from_ref(MPMediaItemPropertyTitle));

            Self::set_now_playing(&now_playing);
        }
    }

    unsafe fn new_metadata(&mut self, metadata: &Metadata) {
        unsafe {
            let now_playing = Self::now_playing_with_previous();

            if let Some(title) = &metadata.name {
                debug!("Setting title: {}", title);
                let ns = NSString::from_str(title);
                now_playing
                    .setObject_forKey(&ns, ProtocolObject::from_ref(MPMediaItemPropertyTitle));
            }

            if let Some(artist) = &metadata.artist {
                debug!("Setting artist: {}", artist);
                let ns = NSString::from_str(artist);
                now_playing
                    .setObject_forKey(&ns, ProtocolObject::from_ref(MPMediaItemPropertyArtist));
            }

            if let Some(album_title) = &metadata.album {
                debug!("Setting album title: {}", album_title);
                let ns = NSString::from_str(album_title);
                now_playing
                    .setObject_forKey(&ns, ProtocolObject::from_ref(MPMediaItemPropertyAlbumTitle));
            }

            Self::set_now_playing(&now_playing);
        }
    }

    unsafe fn new_duration(&mut self, duration: u64) {
        unsafe {
            let now_playing = Self::now_playing_with_previous();

            let ns = NSNumber::numberWithUnsignedLong(duration);
            now_playing.setObject_forKey(
                &ns,
                ProtocolObject::from_ref(MPMediaItemPropertyPlaybackDuration),
            );

            Self::set_now_playing(&now_playing);
        }
    }

    unsafe fn new_position(&mut self, position: u64) {
        unsafe {
            let now_playing = Self::now_playing_with_previous();

            let ns = NSNumber::numberWithUnsignedLong(position);
            now_playing.setObject_forKey(
                &ns,
                ProtocolObject::from_ref(MPNowPlayingInfoPropertyElapsedPlaybackTime),
            );

            Self::set_now_playing(&now_playing);
        }
    }

    unsafe fn new_album_art(&mut self, art: &[u8]) {
        unsafe {
            debug!("Received album art");
            // get the image's dimensions, we'll need them to load the image into NP
            let Ok(size) = imagesize::blob_size(art) else {
                return;
            };

            let data = NSData::with_bytes(art);
            let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
                error!("Failed to create NSImage from album art");
                return;
            };
            // there's a good chance this leaks memory
            // the only way that it wouldn't is if, once it disappears in to macOS, the OS drops it
            // there's an even better chance that if it does, there's no way to fix it
            // TODO: figure out this mess
            let image = NonNull::new(Retained::into_raw(image)).unwrap();

            let request_handler = RcBlock::new(move |_cg: CGSize| image);
            let bounds_size = CGSize::new(size.width as f64, size.height as f64);
            let artwork = MPMediaItemArtwork::initWithBoundsSize_requestHandler(
                MPMediaItemArtwork::alloc(),
                bounds_size,
                &request_handler,
            );

            let now_playing = Self::now_playing_with_previous();
            now_playing.setObject_forKey(
                &artwork,
                ProtocolObject::from_ref(MPMediaItemPropertyArtwork),
            );

            Self::set_now_playing(&now_playing);
        }
    }

    unsafe fn new_playback_state(&mut self, state: PlaybackState) {
        unsafe {
            debug!("Setting playback state: {:?}", state);
            let media_center = MPNowPlayingInfoCenter::defaultCenter();
            media_center.setPlaybackState(match state {
                PlaybackState::Stopped => MPNowPlayingPlaybackState::Stopped,
                PlaybackState::Playing => MPNowPlayingPlaybackState::Playing,
                PlaybackState::Paused => MPNowPlayingPlaybackState::Paused,
            });
        }
    }

    /// Attaches a handler that forwards `action` to the playback thread each
    /// time `command` is triggered from the system remote.
    unsafe fn attach_command_handler(&self, command: &MPRemoteCommand, action: PlaybackCommand) {
        unsafe {
            let cmd_tx = self.cmd_tx.clone();
            // Handler blocks are `Fn` (they may fire any number of times), so
            // the command is cloned per invocation; the commands used here are
            // unit variants, so the clone is free.
            let handler = RcBlock::new(move |_| {
                let _ = cmd_tx.send(action.clone());
                MPRemoteCommandHandlerStatus::Success
            });

            command.setEnabled(true);
            command.addTargetWithHandler(&handler);
        }
    }

    unsafe fn attach_command_handlers(&self) {
        unsafe {
            let command_center = MPRemoteCommandCenter::sharedCommandCenter();

            self.attach_command_handler(&command_center.playCommand(), PlaybackCommand::Play);
            self.attach_command_handler(&command_center.pauseCommand(), PlaybackCommand::Pause);
            self.attach_command_handler(
                &command_center.togglePlayPauseCommand(),
                PlaybackCommand::TogglePlayPause,
            );
            self.attach_command_handler(
                &command_center.previousTrackCommand(),
                PlaybackCommand::Previous,
            );
            self.attach_command_handler(&command_center.nextTrackCommand(), PlaybackCommand::Next);

            // Seek
            let seek_tx = self.cmd_tx.clone();
            let seek_handler = RcBlock::new(move |mut event: NonNull<MPRemoteCommandEvent>| {
                if let Some(ev) = Retained::retain(event.as_mut()) {
                    let ev: Retained<MPChangePlaybackPositionCommandEvent> =
                        Retained::cast_unchecked(ev);
                    let _ = seek_tx.send(PlaybackCommand::Seek(ev.positionTime()));
                }
                MPRemoteCommandHandlerStatus::Success
            });

            let cmd = command_center.changePlaybackPositionCommand();
            cmd.setEnabled(true);
            cmd.addTargetWithHandler(&seek_handler);
        }
    }
}

#[async_trait]
impl PlaybackController for MacMediaPlayerController {
    async fn position_changed(&mut self, new_position: u64) -> anyhow::Result<()> {
        unsafe {
            self.new_position(new_position);
            Ok(())
        }
    }
    async fn duration_changed(&mut self, new_duration: u64) -> anyhow::Result<()> {
        unsafe {
            self.new_duration(new_duration);
            Ok(())
        }
    }
    async fn volume_changed(&mut self, _new_volume: f64) -> anyhow::Result<()> {
        Ok(())
    }
    async fn metadata_changed(&mut self, metadata: &Metadata) -> anyhow::Result<()> {
        unsafe {
            self.new_metadata(metadata);
            Ok(())
        }
    }
    async fn album_art_changed(&mut self, album_art: &[u8]) -> anyhow::Result<()> {
        unsafe {
            self.new_album_art(album_art);
            Ok(())
        }
    }
    async fn repeat_state_changed(&mut self, _repeat_state: RepeatState) -> anyhow::Result<()> {
        Ok(())
    }
    async fn playback_state_changed(
        &mut self,
        playback_state: PlaybackState,
    ) -> anyhow::Result<()> {
        unsafe {
            self.new_playback_state(playback_state);
            Ok(())
        }
    }
    async fn new_file(&mut self, path: &Path) -> anyhow::Result<()> {
        unsafe {
            self.new_file(path);
            Ok(())
        }
    }
    async fn shuffle_state_changed(&mut self, _shuffling: bool) -> anyhow::Result<()> {
        Ok(())
    }
}

impl MacMediaPlayerController {
    pub fn init(
        cmd_tx: UnboundedSender<PlaybackCommand>,
        _handle: Option<RawWindowHandle>,
    ) -> anyhow::Result<Box<dyn PlaybackController>> {
        let mmpc = MacMediaPlayerController { cmd_tx };
        unsafe { mmpc.attach_command_handlers() };
        Ok(Box::new(mmpc))
    }
}
