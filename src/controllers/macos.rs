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

/// 把 Meliora 的播放状态上报给 macOS 的 Now Playing 信息中心，并把系统远程命令事件
/// 转发给播放线程。
pub struct MacMediaPlayerController {
    cmd_tx: UnboundedSender<PlaybackCommand>,
}

impl MacMediaPlayerController {
    /// 一份全新的 now-playing 字典，预先用当前已发布的那份填好，这样局部更新能保留
    /// 其余键。
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

    /// 把 now-playing 字典发布到共享信息中心。
    unsafe fn set_now_playing(now_playing: &NSMutableDictionary<NSString>) {
        unsafe {
            MPNowPlayingInfoCenter::defaultCenter().setNowPlayingInfo(Some(now_playing));
        }
    }

    unsafe fn new_file(&mut self, path: &Path) {
        unsafe {
            debug!("New file: {:?}", path);

            // 以 ".." 结尾的路径没有文件名，非 UTF-8 的名字会让 `to_str()` 失败；
            // 任一处 unwrap 都会在 pbc 任务里 panic，把它 unwind 掉并永久停用
            // now-playing 集成。改为退回完整路径并替换非法 UTF-8。
            let file_name = path.file_name().unwrap_or_else(|| path.as_os_str());

            // 刻意从空字典开始：新文件应当重置 now-playing 条目，而不是更新它。
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
            // 取图像尺寸，加载进 NP 时需要它
            let Ok(size) = imagesize::blob_size(art) else {
                return;
            };

            let data = NSData::with_bytes(art);
            let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
                error!("Failed to create NSImage from album art");
                return;
            };
            // 这里很可能在漏内存
            // 唯一不漏的情形是：它进入 macOS 之后被 OS 丢掉
            // 而更可能的是，即使被丢掉，我们也没有办法回收它
            // TODO: 把这一团乱弄清楚
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

    /// 挂一个处理器，每次系统远程触发 `command` 时把 `action` 转发给播放线程。
    unsafe fn attach_command_handler(&self, command: &MPRemoteCommand, action: PlaybackCommand) {
        unsafe {
            let cmd_tx = self.cmd_tx.clone();
            // 处理器闭包是 `Fn`（可能触发任意多次），所以每次调用都克隆 command；
            // 这里用到的 command 都是单元变体，克隆不花钱。
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

            // 跳转定位
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
