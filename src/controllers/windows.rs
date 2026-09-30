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
    /// Persistent backing stream for the SMTC thumbnail, overwritten in
    /// place on every update. Building a fresh
    /// InMemoryRandomAccessStream + DataWriter per track left the replaced
    /// thumbnail chains to delayed COM reclamation, which over long
    /// background sessions leaked a few MB per track (matches the [mem]
    /// probe's per-track climb). One long-lived stream keeps the shell's
    /// reference stable and the artwork byte count bounded.
    ///
    /// Only the stream is kept; the DataWriter is rebuilt per update. Its
    /// write position survives StoreAsync and this binding exposes no
    /// reset, so a reused writer appended each track's artwork past the
    /// truncated end and the backing stream grew by the artwork size per
    /// track — the same per-track climb, resurfacing after the stream was
    /// made persistent. A fresh writer always starts at position 0, and
    /// the SetSize after the store pins the stream to exactly the artwork
    /// length.
    album_art_stream: Option<InMemoryRandomAccessStream>,
}

impl WindowsController {
    /// Registers the SMTC button, seek, shuffle, and repeat request handlers.
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
            // A failed event payload or getter skips this button event instead
            // of panicking the SMTC task (which would disable SMTC for good).
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
                // a failed payload/getter skips this seek event
                let Some(args) = args.as_ref() else {
                    return Ok(());
                };
                let Ok(position) = args.RequestedPlaybackPosition() else {
                    return Ok(());
                };

                // TimeSpan is measured in 100ns intervals
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
                // a failed payload/getter skips this shuffle event
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
                // a failed payload/getter skips this repeat event
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
            // A failure here must surface as an error, not a panic: init runs
            // inside the startup `main_window.update` on the main thread, and
            // the caller degrades to "no desktop integration" on Err.
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
        // One MusicProperties() fetch for the whole update: each call builds
        // a fresh WinRT object. Property write order is unchanged; a failed
        // getter skips this update entirely instead of panicking the
        // controller task (the next playback event retries).
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
        // Overwrite the persistent thumbnail stream in place instead of
        // building a fresh InMemoryRandomAccessStream per track: the shell's
        // reference stays stable and the artwork byte count bounded. The
        // DataWriter is rebuilt per update (see the field comment) and
        // dropped right after the store; SetSize pins the stream to exactly
        // the artwork length so no stale tail can survive either.
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
        // Paths ending in ".." have no file name and non-UTF-8 names fail
        // `to_str()`; either unwrap would panic inside the pbc task, unwinding
        // it and permanently disabling SMTC. Fall back to the full path and
        // replace invalid UTF-8 instead.
        let file_name = path.file_name().unwrap_or(path.as_os_str());
        let title_string = HSTRING::from(file_name.to_string_lossy().as_ref());
        // Same as metadata_changed: a failed MusicProperties() skips this
        // update instead of panicking the controller task.
        let Ok(music) = self.display.MusicProperties() else {
            return Ok(());
        };
        music.SetTitle(&title_string)?;
        self.display.Update()?;

        Ok(())
    }
}
