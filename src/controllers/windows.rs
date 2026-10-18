use std::{ffi::c_void, path::Path, time::Duration};

use async_trait::async_trait;
use raw_window_handle::RawWindowHandle;
use tokio::sync::mpsc::UnboundedSender;
use windows::{
    Foundation::TypedEventHandler,
    Media::{
        AutoRepeatModeChangeRequestedEventArgs, MediaPlaybackAutoRepeatMode, MediaPlaybackStatus,
        MediaPlaybackType, PlaybackPositionChangeRequestedEventArgs,
        ShuffleEnabledChangeRequestedEventArgs, SystemMediaTransportControls,
        SystemMediaTransportControlsButton, SystemMediaTransportControlsButtonPressedEventArgs,
        SystemMediaTransportControlsDisplayUpdater, SystemMediaTransportControlsTimelineProperties,
    },
    Storage::Streams::{DataWriter, InMemoryRandomAccessStream, RandomAccessStreamReference},
    Win32::{Foundation::HWND, System::WinRT::ISystemMediaTransportControlsInterop},
    core::HSTRING,
};

use crate::{
    media::metadata::Metadata,
    playback::{
        events::{PlaybackCommand, RepeatState},
        thread::PlaybackState,
    },
};

use super::PlaybackController;

pub struct WindowsController {
    controls: SystemMediaTransportControls,
    display: SystemMediaTransportControlsDisplayUpdater,
    timeline: SystemMediaTransportControlsTimelineProperties,
    cmd_tx: UnboundedSender<PlaybackCommand>,
    /// SMTC 缩略图的常驻后备流，每次更新都就地覆写。每首曲目都重建一个全新的
    /// InMemoryRandomAccessStream + DataWriter，会让被替换掉的缩略图链交给延后的
    /// COM 回收，长时间后台会话里每首曲目漏掉几 MB（与 [mem] 探针的每曲爬升吻合）。
    /// 一条长生命周期的流让 shell 的引用保持稳定，封面字节数也有界。
    ///
    /// 只保留流；DataWriter 每次更新重建。它的写入位置会活过 StoreAsync，而这个
    /// 绑定不暴露任何 reset，所以复用同一个 writer 会把每首曲目的封面追加到已截断
    /// 末尾之后，后备流按每曲一个封面大小增长——即流改成常驻之后又复现的那条每曲
    /// 爬升。新建的 writer 位置恒为 0，store 之后的 SetSize 把流钉成恰好封面的长度。
    album_art_stream: Option<InMemoryRandomAccessStream>,
}

impl WindowsController {
    /// 注册 SMTC 的按钮、跳转定位、随机与循环请求处理器。
    pub fn connect_events(&mut self) -> anyhow::Result<()> {
        self.controls.SetIsEnabled(true)?;
        self.controls.SetIsNextEnabled(true)?;
        self.controls.SetIsPreviousEnabled(true)?;
        self.controls.SetIsPlayEnabled(true)?;
        self.controls.SetIsPauseEnabled(true)?;
        self.controls.SetIsStopEnabled(true)?;

        let cmd_tx = self.cmd_tx.clone();
        self.controls.ButtonPressed(&TypedEventHandler::<
            SystemMediaTransportControls,
            SystemMediaTransportControlsButtonPressedEventArgs,
        >::new(move |_, args| {
            // 事件负载或 getter 失败时跳过这次按钮事件，而不是让 SMTC 任务 panic
            // （那会让 SMTC 永久失效）。
            let Some(args) = args.as_ref() else {
                return Ok(());
            };
            let Ok(event) = args.Button() else {
                return Ok(());
            };

            match event {
                SystemMediaTransportControlsButton::Play => {
                    let _ = cmd_tx.send(PlaybackCommand::Play);
                }
                SystemMediaTransportControlsButton::Pause => {
                    let _ = cmd_tx.send(PlaybackCommand::Pause);
                }
                SystemMediaTransportControlsButton::Next => {
                    let _ = cmd_tx.send(PlaybackCommand::Next);
                }
                SystemMediaTransportControlsButton::Previous => {
                    let _ = cmd_tx.send(PlaybackCommand::Previous);
                }
                SystemMediaTransportControlsButton::Stop => {
                    let _ = cmd_tx.send(PlaybackCommand::Stop);
                }
                _ => (),
            }

            Ok(())
        }))?;

        let cmd_tx = self.cmd_tx.clone();
        self.controls
            .PlaybackPositionChangeRequested(&TypedEventHandler::<
                SystemMediaTransportControls,
                PlaybackPositionChangeRequestedEventArgs,
            >::new(move |_, args| {
                // 负载/getter 失败时跳过这次跳转定位事件
                let Some(args) = args.as_ref() else {
                    return Ok(());
                };
                let Ok(position) = args.RequestedPlaybackPosition() else {
                    return Ok(());
                };

                // TimeSpan 以 100ns 为最小单位计量
                let _ = cmd_tx.send(PlaybackCommand::Seek(
                    position.Duration as f64 / 10_000_000.0,
                ));

                Ok(())
            }))?;

        let cmd_tx = self.cmd_tx.clone();
        self.controls
            .ShuffleEnabledChangeRequested(&TypedEventHandler::<
                SystemMediaTransportControls,
                ShuffleEnabledChangeRequestedEventArgs,
            >::new(move |_, args| {
                // 负载/getter 失败时跳过这次随机播放事件
                let Some(args) = args.as_ref() else {
                    return Ok(());
                };
                let Ok(shuffle) = args.RequestedShuffleEnabled() else {
                    return Ok(());
                };
                let _ = cmd_tx.send(PlaybackCommand::SetShuffle(shuffle));

                Ok(())
            }))?;

        let cmd_tx = self.cmd_tx.clone();
        self.controls
            .AutoRepeatModeChangeRequested(&TypedEventHandler::<
                SystemMediaTransportControls,
                AutoRepeatModeChangeRequestedEventArgs,
            >::new(move |_, args| {
                // 负载/getter 失败时跳过这次循环模式事件
                let Some(args) = args.as_ref() else {
                    return Ok(());
                };
                let Ok(mode) = args.RequestedAutoRepeatMode() else {
                    return Ok(());
                };

                let _ = cmd_tx.send(PlaybackCommand::SetRepeat(match mode {
                    MediaPlaybackAutoRepeatMode::List => RepeatState::Repeating,
                    MediaPlaybackAutoRepeatMode::Track => RepeatState::RepeatingOne,
                    _ => RepeatState::NotRepeating,
                }));

                Ok(())
            }))?;

        Ok(())
    }
}

impl WindowsController {
    pub fn init(
        cmd_tx: UnboundedSender<PlaybackCommand>,
        handle: Option<RawWindowHandle>,
    ) -> anyhow::Result<Box<dyn PlaybackController>> {
        let interop: ISystemMediaTransportControlsInterop = windows::core::factory::<
            SystemMediaTransportControls,
            ISystemMediaTransportControlsInterop,
        >()?;

        let hwnd = match handle {
            Some(RawWindowHandle::Win32(handle)) => handle,
            // 这里的失败必须表现为 error 而不是 panic：init 跑在启动时的
            // `main_window.update` 里、在主线程上，而调用方遇到 Err 会退化为
            // "不做桌面集成"。
            _ => anyhow::bail!(
                "non-Win32 window handle/invalid window handle during creation of SMTC"
            ),
        };

        let controls: SystemMediaTransportControls = unsafe {
            let pointer = hwnd.hwnd.get() as *mut c_void;
            interop.GetForWindow(HWND(pointer))?
        };

        let display = controls.DisplayUpdater()?;
        let timeline = SystemMediaTransportControlsTimelineProperties::new()?;

        let mut controller = WindowsController {
            controls,
            display,
            timeline,
            cmd_tx,
            album_art_stream: None,
        };

        controller.connect_events()?;

        Ok(Box::new(controller))
    }
}

#[async_trait]
impl PlaybackController for WindowsController {
    async fn position_changed(&mut self, new_position: u64) -> anyhow::Result<()> {
        self.timeline
            .SetPosition(Duration::from_secs(new_position).into())?;
        self.controls.UpdateTimelineProperties(&self.timeline)?;

        Ok(())
    }
    async fn duration_changed(&mut self, new_duration: u64) -> anyhow::Result<()> {
        self.timeline.SetStartTime(Duration::from_secs(0).into())?;
        self.timeline
            .SetMinSeekTime(Duration::from_secs(0).into())?;
        self.timeline
            .SetMaxSeekTime(Duration::from_secs(new_duration).into())?;
        self.timeline
            .SetEndTime(Duration::from_secs(new_duration).into())?;
        self.timeline.SetPosition(Duration::from_secs(0).into())?;
        self.controls.UpdateTimelineProperties(&self.timeline)?;

        Ok(())
    }

    async fn volume_changed(&mut self, _new_volume: f64) -> anyhow::Result<()> {
        Ok(())
    }

    async fn metadata_changed(&mut self, metadata: &Metadata) -> anyhow::Result<()> {
        // 整次更新只取一次 MusicProperties()：每次调用都会构建一个新的 WinRT 对象。
        // 属性写入顺序不变；getter 失败时整次更新直接跳过，而不是让控制器任务
        // panic（下一个播放事件会重试）。
        let Ok(music) = self.display.MusicProperties() else {
            return Ok(());
        };

        if let Some(title) = &metadata.name {
            music.SetTitle(&HSTRING::from(title))?;
        }

        if let Some(artist) = &metadata.artist {
            music.SetArtist(&HSTRING::from(artist))?;
        }

        if let Some(album) = &metadata.album {
            music.SetAlbumTitle(&HSTRING::from(album))?;
        }

        if let Some(track_number) = metadata.track_current {
            music.SetTrackNumber(track_number as u32)?;
        }

        if let Some(track_max) = metadata.track_max {
            music.SetAlbumTrackCount(track_max as u32)?;
        }

        self.display.Update()?;

        Ok(())
    }

    async fn album_art_changed(&mut self, album_art: &[u8]) -> anyhow::Result<()> {
        if album_art.is_empty() {
            return Ok(());
        }
        // 就地覆写常驻的缩略图后备流，而不是每首曲目新建一个
        // InMemoryRandomAccessStream：shell 的引用保持稳定，封面字节数也有界。
        // DataWriter 每次更新重建（见字段注释），store 之后立刻丢弃；SetSize 把流
        // 钉成恰好封面的长度，因此也不会留下过期的尾部。
        let stream = match self.album_art_stream.as_ref() {
            Some(stream) => stream.clone(),
            None => {
                let stream = InMemoryRandomAccessStream::new()
                    .map_err(|_| anyhow::anyhow!("could not create RAS"))?;
                self.album_art_stream = Some(stream.clone());
                stream
            }
        };
        stream.SetSize(0)?;
        stream.Seek(0)?;
        let writer = DataWriter::CreateDataWriter(&stream)?;
        writer.WriteBytes(album_art)?;
        writer.StoreAsync()?.await?;
        stream.SetSize(album_art.len() as u64)?;
        let reference = RandomAccessStreamReference::CreateFromStream(&stream)?;

        self.display.SetThumbnail(&reference)?;
        self.display.Update()?;

        Ok(())
    }

    async fn repeat_state_changed(&mut self, repeat_state: RepeatState) -> anyhow::Result<()> {
        self.controls.SetAutoRepeatMode(match repeat_state {
            RepeatState::NotRepeating => MediaPlaybackAutoRepeatMode::None,
            RepeatState::Repeating => MediaPlaybackAutoRepeatMode::List,
            RepeatState::RepeatingOne => MediaPlaybackAutoRepeatMode::Track,
        })?;

        Ok(())
    }

    async fn playback_state_changed(
        &mut self,
        playback_state: PlaybackState,
    ) -> anyhow::Result<()> {
        let playback_state = match playback_state {
            PlaybackState::Stopped => MediaPlaybackStatus::Stopped,
            PlaybackState::Playing => MediaPlaybackStatus::Playing,
            PlaybackState::Paused => MediaPlaybackStatus::Paused,
        };

        self.controls.SetPlaybackStatus(playback_state)?;

        Ok(())
    }
    async fn shuffle_state_changed(&mut self, shuffling: bool) -> anyhow::Result<()> {
        self.controls.SetShuffleEnabled(shuffling)?;

        Ok(())
    }
    async fn new_file(&mut self, path: &Path) -> anyhow::Result<()> {
        self.display.ClearAll()?;
        self.display.SetType(MediaPlaybackType::Music)?;
        // 以 ".." 结尾的路径没有文件名，非 UTF-8 的名字会让 `to_str()` 失败；任一处
        // unwrap 都会在 pbc 任务里 panic，把它 unwind 掉并永久停用 SMTC。改为退回
        // 完整路径并替换非法 UTF-8。
        let file_name = path.file_name().unwrap_or(path.as_os_str());
        let title_string = HSTRING::from(file_name.to_string_lossy().as_ref());
        // 与 metadata_changed 同理：MusicProperties() 失败时跳过这次更新，而不是让
        // 控制器任务 panic。
        let Ok(music) = self.display.MusicProperties() else {
            return Ok(());
        };
        music.SetTitle(&title_string)?;
        self.display.Update()?;

        Ok(())
    }
}
