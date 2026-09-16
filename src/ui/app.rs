use std::{
    cell::OnceCell,
    fs,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use clap::Parser;
use cntp_i18n::{I18N_MANAGER, Locale, tr};
use gpui::*;
use gpui_platform::current_platform;
use prelude::FluentBuilder;
use sqlx::SqlitePool;
use tracing::{debug, info};

use crate::{
    library::{
        db::create_pool,
        scan::{ScanEvent, ScanInterface, start_scanner},
    },
    paths,
    playback::{
        interface::PlaybackInterface, queue::QueueItemData,
        session_storage::PlaybackSessionStorageWorker, thread::PlaybackThread,
    },
    power::PowerManager,
    controllers::{init_pbc_task, register_pbc_event_handlers},
    settings::{
        SettingsGlobal, setup_settings,
        storage::{Storage, StorageData},
    },
    toasts,
    ui::{
        assets::MelioraAssetSource,
        caching::MelioraImageCache,
        command_palette::{CommandPalette, CommandPaletteHolder},
        library::missing_folder_dialog::MissingFolderDialog,
        models::WindowInformation,
        settings::corrupt_settings_dialog::CorruptSettingsDialog,
        toasts::ToastLayer,
    },
};

use super::{
    about::about_dialog,
    artist_picker::ArtistPickerView,
    components::{
        modal::{self, ModalActive},
        window_chrome::window_chrome,
    },
    controls::Controls,
    global_actions::register_actions,
    header::Header,
    library::{sidebar::Sidebar, Library},
    models::{self, CurrentTrack, Models, PlaybackInfo, build_models},
    right_sidebar::RightSidebar,
    search::SearchView,
    settings::close_orphaned_settings_windows,
    theme::setup_theme,
};

struct MainWindow {
    pub controls: Entity<Controls>,
    pub sidebar: Entity<Sidebar>,
    pub right_sidebar: RightSidebar,
    pub library: Entity<Library>,
    pub header: Entity<Header>,
    pub search: Entity<SearchView>,
    pub artist_picker: Entity<ArtistPickerView>,
    pub show_queue: Entity<bool>,
    pub show_lyrics: Entity<bool>,
    pub show_about: Entity<bool>,
    pub about_focus: FocusHandle,
    pub missing_folder_dialog: Entity<MissingFolderDialog>,
    pub corrupt_settings_dialog: Entity<CorruptSettingsDialog>,
    pub palette: Entity<CommandPalette>,
    pub image_cache: Entity<MelioraImageCache>,
    pub toast_layer: Entity<ToastLayer>,
}

impl Render for MainWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        cx.global::<ModalActive>().0.store(false, Ordering::Relaxed);

        let show_about = *self.show_about.read(cx);
        let show_corrupt_settings_dialog = matches!(
            cx.global::<Models>().settings_health.read(cx),
            models::SettingsHealth::Corrupt { .. }
        );
        let show_missing_folder_dialog = !show_corrupt_settings_dialog
            && matches!(
                &*cx.global::<Models>().scan_state.read(cx),
                ScanEvent::WaitingForMissingFolderDecision { .. }
            );
        let show_queue = *self.show_queue.read(cx);
        let show_lyrics = *self.show_lyrics.read(cx);
        let show_sidebar = show_queue || show_lyrics;

        div()
            .image_cache(self.image_cache.clone())
            .key_context("app")
            .size_full()
            .child(window_chrome(
                div()
                    .cursor(CursorStyle::Arrow)
                    .on_drop(|ev: &ExternalPaths, _, cx| {
                        let items = ev
                            .paths()
                            .iter()
                            .map(|path| QueueItemData::new(cx, path.clone(), None, None))
                            .collect();

                        let playback_interface = cx.global::<PlaybackInterface>();
                        playback_interface.queue_list(items);
                    })
                    .overflow_hidden()
                    .size_full()
                    .flex()
                    // the whole application has to be flipped upside down otherwise sidebar icons
                    // overlap menu bar menus
                    .flex_col_reverse()
                    .max_w_full()
                    .max_h_full()
                    .child(
                        // 侧边栏占满全高（贴到窗口最底部），播放栏只在其右侧
                        div()
                            .flex()
                            .flex_grow(1.0)
                            .min_h(px(0.0))
                            .w_full()
                            .child(self.sidebar.clone())
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .min_h(px(0.0))
                                    .overflow_hidden()
                                    .child(
                                        div()
                                            .w_full()
                                            .flex_grow(1.0)
                                            .min_h(px(0.0))
                                            .flex()
                                            .max_w_full()
                                            .max_h_full()
                                            .overflow_hidden()
                                            .child(
                                                AnyView::from(self.library.clone()).cached(
                                                    StyleRefinement::default()
                                                        .flex_1()
                                                        .min_w(px(0.0))
                                                        .h_full()
                                                        .max_w_full()
                                                        .max_h_full(),
                                                ),
                                            )
                                            .when(show_sidebar, |this| {
                                                this.child(
                                                    self.right_sidebar
                                                        .render(cx, show_queue, show_lyrics),
                                                )
                                            }),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .w_full()
                                            .h(px(68.0))
                                            .child(
                                                AnyView::from(self.controls.clone()).cached(
                                                    StyleRefinement::default()
                                                        .flex_1()
                                                        .w_full()
                                                        .h_full(),
                                                ),
                                            ),
                                    ),
                            ),
                    )
                    .child(self.header.clone())
                    .child(self.search.clone())
                    .child(self.artist_picker.clone())
                    .child(self.palette.clone())
                    .when(show_about, |this| {
                        this.child(about_dialog(self.about_focus.clone(), &|_, cx| {
                            let show_about = cx.global::<Models>().show_about.clone();
                            show_about.write(cx, false);
                        }))
                    })
                    .when(show_missing_folder_dialog, |this| {
                        this.child(self.missing_folder_dialog.clone())
                    })
                    .when(show_corrupt_settings_dialog, |this| {
                        this.child(self.corrupt_settings_dialog.clone())
                    })
                    .child(self.toast_layer.clone()),
            ))
    }
}

pub fn find_fonts(cx: &mut App) -> gpui::Result<()> {
    let paths = cx.asset_source().list("!bundled:fonts")?;
    let mut fonts = vec![];
    for path in paths {
        if (path.ends_with(".ttf") || path.ends_with(".otf"))
            && let Some(v) = cx.asset_source().load(&path)?
        {
            fonts.push(v);
        }
    }

    let results = cx.text_system().add_fonts(fonts);
    debug!("loaded fonts: {:?}", cx.text_system().all_font_names());
    results
}

pub struct Pool(pub SqlitePool);

impl Global for Pool {}

fn find_main_window(cx: &App) -> Option<WindowHandle<MainWindow>> {
    cx.windows()
        .into_iter()
        .find_map(|window| window.downcast::<MainWindow>())
}

pub(super) fn has_main_window(cx: &App) -> bool {
    find_main_window(cx).is_some()
}

fn focus_main_window(window: WindowHandle<MainWindow>, cx: &mut App) {
    cx.activate(true);
    cx.defer(move |cx| {
        let _ = window.update(cx, |_, window, _| {
            window.activate_window();
        });
    });
}

fn main_window_bounds(cx: &mut App) -> WindowBounds {
    let window_information = cx.global::<Models>().window_information.read(cx).clone();

    if let Some(window_information) = window_information {
        if window_information.maximized {
            WindowBounds::Maximized(Bounds::centered(None, window_information.size, cx))
        } else {
            WindowBounds::Windowed(Bounds::centered(None, window_information.size, cx))
        }
    } else {
        WindowBounds::Maximized(Bounds::centered(None, size(px(1024.0), px(700.0)), cx))
    }
}

fn main_window_options(window_bounds: WindowBounds) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(window_bounds),
        window_background: WindowBackgroundAppearance::Opaque,
        window_decorations: Some(WindowDecorations::Client),
        window_min_size: Some(size(px(800.0), px(600.0))),
        titlebar: Some(TitlebarOptions {
            title: Some(tr!("APP_NAME").into()),
            appears_transparent: true,
            traffic_light_position: Some(Point {
                x: px(12.0),
                y: px(11.0),
            }),
        }),
        app_id: Some("org.li-ming1.meliora".to_string()),
        kind: WindowKind::Normal,
        ..Default::default()
    }
}

fn build_main_window(
    window: &mut Window,
    cx: &mut App,
    toast_layer: Entity<ToastLayer>,
) -> Entity<MainWindow> {
    let window_title = tr!("APP_NAME").to_string();
    window.set_window_title(&window_title);

    let palette = CommandPalette::new(cx, window);
    cx.set_global(CommandPaletteHolder::new(palette.clone()));

    cx.new(|cx| {
        cx.observe_window_activation(window, |_, window, cx| {
            cx.global::<PlaybackInterface>()
                .set_position_broadcast_active(window.is_window_active());
        })
        .detach();

        cx.observe_window_bounds(window, |_, window, cx| {
            let window_information = cx.global::<Models>().window_information.clone();

            let maximized = window.is_maximized();
            let size = if maximized {
                window_information.read(cx).clone()
            } else {
                None
            }
            .map(|v| v.size)
            .unwrap_or(window.bounds().size);

            window_information.write(cx, Some(WindowInformation { maximized, size }));
        })
        .detach();

        cx.observe_window_appearance(window, |_, _, cx| {
            cx.refresh_windows();
        })
        .detach();

        let show_queue = cx.global::<Models>().show_queue.clone();
        let show_lyrics = cx.global::<Models>().show_lyrics.clone();
        let show_about = cx.global::<Models>().show_about.clone();
        let about_focus = cx.focus_handle();

        cx.observe(&show_queue, |_, _, cx| {
            cx.notify();
        })
        .detach();
        cx.observe(&show_lyrics, |_, _, cx| {
            cx.notify();
        })
        .detach();
        cx.observe(&show_about, |_, _, cx| {
            cx.notify();
        })
        .detach();

        // 侧边栏宽度/折叠状态不在这里观察：真正的消费者（Sidebar、Header）各自
        // observe 这些实体，MainWindow 级观察只会在折叠动画期间引发整窗逐帧重建。

        // 右侧栏是内联渲染（非实体），布局跟随分栏宽度/歌词高度变化，必须在此观察
        let queue_width = cx.global::<Models>().queue_width.clone();
        cx.observe(&queue_width, |_, _, cx| cx.notify()).detach();
        let lyrics_height = cx.global::<Models>().lyrics_height.clone();
        cx.observe(&lyrics_height, |_, _, cx| cx.notify()).detach();

        MainWindow {
            controls: Controls::new(cx, show_queue.clone(), show_lyrics.clone()),
            sidebar: {
                let nav_model = cx.global::<Models>().switcher_model.clone();
                Sidebar::new(cx, nav_model)
            },
            right_sidebar: RightSidebar::new(cx),
            library: Library::new(cx),
            header: Header::new(cx),
            search: SearchView::new(cx),
            artist_picker: ArtistPickerView::new(cx),
            show_queue,
            show_lyrics,
            show_about,
            about_focus,
            missing_folder_dialog: MissingFolderDialog::new(cx),
            corrupt_settings_dialog: CorruptSettingsDialog::new(cx),
            palette,
            // use a really small global image cache
            // this is literally just to ensure that images are *always* removed
            // from memory *at some point*
            //
            // if your view uses a lot of images you need to have your own image
            // cache
            image_cache: MelioraImageCache::new(12, cx),
            toast_layer,
        }
    })
}

fn ensure_main_window(
    cx: &mut App,
    toast_layer: Entity<ToastLayer>,
) -> gpui::Result<WindowHandle<MainWindow>> {
    if let Some(window) = find_main_window(cx) {
        focus_main_window(window, cx);
        return Ok(window);
    }

    let bounds = main_window_bounds(cx);
    let options = main_window_options(bounds);
    let window = cx.open_window(options, |window, cx| {
        build_main_window(window, cx, toast_layer)
    })?;
    focus_main_window(window, cx);
    Ok(window)
}

pub fn run() -> anyhow::Result<()> {
    let toast_receiver = toasts::init();
    let data_dir = paths::data_dir();
    fs::create_dir_all(&data_dir).inspect_err(|error| {
        tracing::error!(
            ?error,
            "couldn't create data directory '{}'",
            data_dir.display(),
        )
    })?;

    // 一次性启动打点：create_pool（连接 + 迁移）的耗时决定它是否值得异步化
    // （MelioraAssetSource 在窗口创建前就需要 pool，异步化是大手术）。数据
    // 说话——日志里 elapsed_ms 长期小于几十毫秒就不动（教义 §1/§39）。
    let pool_create_started_at = std::time::Instant::now();
    let pool = crate::RUNTIME
        .block_on(create_pool(data_dir.join("library.db")))
        .inspect_err(|error| {
            tracing::error!(?error, "fatal: unable to create database pool");
        })?;
    tracing::info!(
        elapsed_ms = pool_create_started_at.elapsed().as_millis() as u64,
        "[startup] database pool ready"
    );

    let application = Application::with_platform(current_platform(false))
        .with_assets(MelioraAssetSource::new(pool.clone()));
    let toast_layer: Rc<OnceCell<Entity<ToastLayer>>> = Rc::new(OnceCell::new());
    let toast_layer_for_reopen = toast_layer.clone();
    application.on_reopen(move |cx| {
        if let Some(toast_layer) = toast_layer_for_reopen.get() {
            let _ = ensure_main_window(cx, toast_layer.clone());
        }
    });
    application.run(move |cx: &mut App| {
        // Fontconfig isn't read currently so fall back to the most "okay" font rendering
        // option - I'm sure people will disagree with this but Grayscale font rendering
        // results in text that is at least displayed correctly on all screens, unlike
        // sub-pixel AA
        #[cfg(target_os = "linux")]
        cx.set_text_rendering_mode(TextRenderingMode::Grayscale);

        find_fonts(cx).expect("unable to load fonts");

        let storage = Storage::new(data_dir.join("app_data.json"));
        let storage_data = storage.load_or_default();

        let session_file = data_dir.join("playback_session.json");
        let playback_session = PlaybackSessionStorageWorker::load(&session_file);
        let initial_position = playback_session
            .queue_position
            .filter(|position| *position < playback_session.queue.len());
        let initial_track = initial_position
            .and_then(|position| playback_session.queue.get(position))
            .map(|item| CurrentTrack::new(item.get_path().clone()));

        let queue: Arc<RwLock<Vec<QueueItemData>>> =
            Arc::new(RwLock::new(playback_session.queue.clone()));

        let (queue_tx, queue_rx) = tokio::sync::watch::channel(playback_session.clone());
        crate::RUNTIME.spawn(PlaybackSessionStorageWorker::new(session_file, queue_rx).run());

        setup_settings(cx, data_dir.join("settings.json"));
        setup_theme(cx, data_dir.clone());
        cx.set_global(Pool(pool.clone()));

        let settings = cx.global::<SettingsGlobal>().model.read(cx);
        let language = settings.interface.language.clone();
        let playback_settings = settings.playback.clone();
        let scanning_settings = settings.scanning.clone();
        let initial_repeat = if playback_settings.always_repeat
            && playback_session.repeat == crate::playback::events::RepeatState::NotRepeating
        {
            crate::playback::events::RepeatState::Repeating
        } else {
            playback_session.repeat
        };
        build_models(
            cx,
            models::Queue {
                data: queue.clone(),
                position: initial_position.unwrap_or(0),
            },
            &storage_data,
            initial_track,
            playback_session.shuffle,
            initial_repeat,
        );

        // Revive restored online queue items whose signed stream URL expired
        // while the app was off: re-fetch a fresh URL in the background and
        // keep the current track / info section keyed on the fresh URL.
        #[cfg(feature = "online_sources")]
        refresh_restored_online_urls(cx, &queue, &playback_settings);

        super::keymap::load_default_keymap(cx);

        cx.set_global(modal::ModalActive(AtomicBool::new(false)));

        let settings_model = cx.global::<SettingsGlobal>().model.clone();
        cx.observe(&settings_model, |_, cx| cx.refresh_windows())
            .detach();

        if !language.is_empty() {
            I18N_MANAGER
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .locale = Locale::new_from_locale_identifier(language);
        }

        let (scan_interface, scan_events) = start_scanner(pool.clone(), scanning_settings);
        let initial_health = cx.global::<Models>().settings_health.read(cx).clone();
        if matches!(initial_health, models::SettingsHealth::Ok) {
            scan_interface.scan();
        } else {
            tracing::warn!("Settings file is corrupt; holding scanner until resolved");
        }
        scan_interface.start_broadcast(scan_events, cx);

        cx.set_global(scan_interface);

        let settings_health = cx.global::<Models>().settings_health.clone();
        cx.observe(&settings_health, |health, cx| {
            if matches!(health.read(cx), models::SettingsHealth::Ok) {
                let scanning = cx
                    .global::<SettingsGlobal>()
                    .model
                    .read(cx)
                    .scanning
                    .clone();
                let scanner = cx.global::<ScanInterface>();
                scanner.update_settings(scanning);
                scanner.scan();
            }
        })
        .detach();

        let power_manager = PowerManager::new(cx, playback_settings.prevent_idle);
        cx.set_global(power_manager);

        register_actions(cx);

        let last_volume = *cx.global::<PlaybackInfo>().volume.read(cx);

        // Listening-stats recorder, fed by the playback event loop and flushed
        // to SQLite on quit.
        cx.set_global(crate::stats::StatsHandle::new());

        let mut playback_interface: PlaybackInterface = PlaybackThread::start(
            queue.clone(),
            playback_settings,
            last_volume,
            playback_session,
            queue_tx,
        );
        playback_interface.start_broadcast(cx);

        if !parse_args_and_prepare(cx, &playback_interface)
            && let Some(pos) = initial_position
        {
            playback_interface.jump(pos);
            playback_interface.pause();
        }
        cx.set_global(playback_interface);

        let toast_layer_entity = ToastLayer::new(cx, toast_receiver);
        toast_layer
            .set(toast_layer_entity.clone())
            .expect("toast layer initialized once");

        // Kick off the daily free-VIP claim on launch so an already-logged-in
        // account gets today's award without opening the settings page. The
        // claim itself is guarded by the persisted "claimed today" marker, so
        // this is a no-op (zero requests) once per calendar day.
        #[cfg(feature = "kugou")]
        crate::ui::kugou::claim_daily_vip_async();

        // Refresh the NetEase liked-songs cache on launch so the play-bar
        // star is accurate for online tracks right away (no-op when logged
        // out).
        #[cfg(feature = "netease")]
        crate::ui::netease::prime_liked_cache();

        // Update `StorageData` and save it to file system while quitting the app.
        cx.on_app_quit({
            let storage = storage.clone();
            move |cx| {
                let data = StorageData::new(cx);
                let storage = storage.clone();

                // Flush the partially-listened segment so the final seconds of
                // a session are not lost. `write_rows` is tokio-backed, so it
                // runs through RUNTIME's context on the background thread.
                let pending_rows = {
                    let stats = cx.global::<crate::stats::StatsHandle>();
                    let mut recorder = stats.0.lock().unwrap_or_else(|e| e.into_inner());
                    recorder.on_state(crate::playback::thread::PlaybackState::Stopped);
                    recorder.take_pending()
                };
                let stats_pool = cx.global::<Pool>().0.clone();

                cx.background_executor().spawn(async move {
                    storage.save(&data);
                    if !pending_rows.is_empty() {
                        crate::RUNTIME
                            .block_on(crate::stats::queries::write_rows(&stats_pool, pending_rows));
                    }
                    crate::logging::flush();
                })
            }
        })
        .detach();

        cx.on_window_closed(|cx, _window_id| {
            close_orphaned_settings_windows(cx);
        })
        .detach();

        if let Some(window_information) = storage_data.window_information {
            cx.global::<Models>()
                .window_information
                .clone()
                .write(cx, Some(window_information.clone()));
        }

        let main_window = ensure_main_window(cx, toast_layer_entity).unwrap();
        main_window
            .update(cx, |_, window, cx| {
                init_pbc_task(cx, window);
            })
            .unwrap();
        register_pbc_event_handlers(cx);
    });

    Ok(())
}

/// Restored online queue items carry signed stream URLs that expire while the
/// app is off; re-fetch a fresh URL for each one so a long-idle session still
/// plays. If the refreshed item is the one currently playing, the current
/// track path is updated in lockstep so the info section (cover/name/artist)
/// stays keyed on the fresh URL. Network calls hop onto the Tokio runtime.
#[cfg(feature = "online_sources")]
fn refresh_restored_online_urls(
    cx: &mut App,
    queue: &Arc<RwLock<Vec<QueueItemData>>>,
    playback: &crate::settings::playback::PlaybackSettings,
) {
    use crate::playback::queue::OnlineIdentity;

    let stale: Vec<(usize, OnlineIdentity)> = queue
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| {
            crate::media::is_http_path(item.get_path())
                .then(|| item.online_identity().cloned())
                .flatten()
                .map(|identity| (idx, identity))
        })
        .collect();
    if stale.is_empty() {
        return;
    }

    // Owned so the spawned task outlives this scope; `as_str` is feature-gated.
    #[cfg(feature = "kugou")]
    let kugou_quality = playback.online_quality.as_str().to_string();
    #[cfg(feature = "netease")]
    let netease_quality = playback.netease_quality.as_str().to_string();

    let queue = Arc::clone(queue);
    let current_track = cx.global::<PlaybackInfo>().current_track.clone();
    let current_track_path = current_track
        .read(cx)
        .as_ref()
        .map(|track| track.get_path().clone());

    cx.spawn(async move |cx| {
        for (idx, identity) in stale {
            let display = queue
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(idx)
                .and_then(|item| item.persisted_display())
                .unwrap_or((None, None, None, None));

            // Shared refresh: fetches a fresh URL and re-registers it in the
            // provider's stream registry (lyrics / like / download resolve by
            // that registry, so a refreshed URL must be re-registered or those
            // break once the refreshed stream comes up in rotation).
            let url = crate::ui::online::refresh_online_url(
                &identity,
                #[cfg(feature = "kugou")]
                kugou_quality.as_str(),
                #[cfg(feature = "netease")]
                netease_quality.as_str(),
                display,
            )
            .await;

            let Some(url) = url else { continue };

            let was_current = {
                let Ok(mut guard) = queue.write() else { continue };
                let Some(item) = guard.get_mut(idx) else { continue };
                // the queue may have shifted since the snapshot; never clobber
                // a different item
                if item.online_identity() != Some(&identity) {
                    continue;
                }
                let was_current = current_track_path
                    .as_ref()
                    .is_some_and(|path| path == item.get_path());
                item.replace_path(PathBuf::from(url.clone()));
                was_current
            };

            if was_current {
                current_track.update(cx, |track, cx| {
                    *track = Some(CurrentTrack::new(PathBuf::from(url)));
                    cx.notify();
                });
            }
        }
    })
    .detach();
}

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    #[arg()]
    files: Option<Vec<PathBuf>>,
}

/// Parses the arguments provided by the user and handles them. Returns true if files were provided
/// for playback as command line arguments.
fn parse_args_and_prepare(cx: &mut App, interface: &PlaybackInterface) -> bool {
    let args = Args::parse();

    if let Some(files) = &args.files {
        info!("Queueing files found in arguments: {:?}", files);

        interface.queue_list(
            files
                .iter()
                .map(|path| QueueItemData::new(cx, path.clone(), None, None))
                .collect(),
        );
    }

    args.files.is_some()
}
