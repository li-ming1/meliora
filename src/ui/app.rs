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
use tracing::{debug, error, info};

use crate::sync::{RecoveredMutex, RecoveredRwLock};
use crate::{
    controllers::{init_pbc_task, register_pbc_event_handlers},
    library::{
        db::create_pool,
        scan::{ScanInterface, start_scanner},
    },
    paths,
    playback::{
        interface::PlaybackInterface, queue::QueueItemData,
        session_storage::PlaybackSessionStorageWorker, thread::PlaybackThread,
    },
    power::PowerManager,
    settings::{
        SettingsGlobal, setup_settings_with_outcome,
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
    immersive::ImmersiveView,
    library::{Library, sidebar::Sidebar},
    models::{self, CurrentTrack, Models, PlaybackInfo, build_models},
    right_sidebar::RightSidebar,
    search::SearchView,
    settings::close_orphaned_settings_windows,
    theme::setup_theme_with_options,
};

pub(crate) struct MainWindow {
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
    pub immersive: Entity<bool>,
    pub immersive_view: Entity<ImmersiveView>,
    pub about_focus: FocusHandle,
    pub missing_folder_dialog: Entity<MissingFolderDialog>,
    pub corrupt_settings_dialog: Entity<CorruptSettingsDialog>,
    pub palette: Entity<CommandPalette>,
    pub image_cache: Entity<MelioraImageCache>,
    pub toast_layer: Entity<ToastLayer>,
}

impl Render for MainWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _render = crate::tally_render(crate::RenderedView::MainWindow);
        // 启动可观测性：首帧落地的时刻直接读日志行的前缀（logging.rs 用的是
        // `fmt::time::uptime()`，即进程年龄），所以这里不需要自带耗时字段。
        // 代价 = 每次重绘一次 `Once` 的原子读。
        static FIRST_FRAME: std::sync::Once = std::sync::Once::new();
        FIRST_FRAME.call_once(|| tracing::info!("[startup] first frame"));
        cx.global::<ModalActive>().0.store(false, Ordering::Relaxed);

        let show_about = *self.show_about.read(cx);
        let immersive = *self.immersive.read(cx);
        if immersive {
            // 沉浸模式取代整个库布局；toast 层保留，通知仍然可见。window_chrome
            // 跳过——全屏下系统装饰本来就没有。
            return div()
                .image_cache(self.image_cache.clone())
                .key_context("app")
                .size_full()
                .child(self.immersive_view.clone())
                .child(self.toast_layer.clone());
        }
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
                            .map(|path| QueueItemData::new(path.clone(), None, None))
                            .collect();

                        let playback_interface = cx.global::<PlaybackInterface>();
                        playback_interface.queue_list(items);
                    })
                    .overflow_hidden()
                    .size_full()
                    .flex()
                    // 否则整个应用都得翻转过来，不然侧栏图标会压住菜单栏的菜单
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
                                                this.child(self.right_sidebar.render(
                                                    cx,
                                                    show_queue,
                                                    show_lyrics,
                                                ))
                                            }),
                                    )
                                    .child(div().flex().w_full().h(px(68.0)).child(
                                        AnyView::from(self.controls.clone()).cached(
                                            StyleRefinement::default().flex_1().w_full().h_full(),
                                        ),
                                    )),
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
                    // 两个对话框都保持挂载，由它们自己的 render 内部隐藏（空 div）。
                    // 在这里用 `scan_state` / `settings_health` 的读值来门控挂载，
                    // 会让根视图按扫描进度频率重渲染（扫描期间 250 ms 一次），
                    // 只为两个几乎永远为 false 的值——每 tick 整个窗口外壳都重建、
                    // 毫无收益。两种情况下实体都在树里，所以可见性与之前逐字节相同。
                    .child(self.missing_folder_dialog.clone())
                    .child(self.corrupt_settings_dialog.clone())
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

// Pool 本体下沉到 library 层（审计 A-3 第一批）：playback/stats/library 从
// 此不再为拿连接池而依赖 crate::ui；此处再导出维持既有 `ui::app::Pool` 引用。
pub use crate::library::Pool;

pub(crate) fn find_main_window(cx: &App) -> Option<WindowHandle<MainWindow>> {
    cx.windows()
        .into_iter()
        .find_map(|window| window.downcast::<MainWindow>())
}

/// 主窗口宿主（toast 层实体）：托盘唤回时主窗口已销毁的极端路径经它重建。
/// 正常托盘流程只显隐不销毁，此全局只为兜底。
pub struct MainWindowHost(pub Entity<ToastLayer>);

impl gpui::Global for MainWindowHost {}

/// 托盘唤回入口：窗口在则激活，不在则经 [`MainWindowHost`] 重建。
#[cfg(any(windows, target_os = "macos"))]
pub(crate) fn restore_main_window(cx: &mut App) {
    if let Some(window) = find_main_window(cx) {
        focus_main_window(window, cx);
        return;
    }
    let Some(layer) = cx.try_global::<MainWindowHost>().map(|host| host.0.clone()) else {
        return;
    };
    let _ = ensure_main_window(cx, layer);
}

/// 进入沉浸模式时主窗口是否处于最大化；决定退出全屏时的还原动作。纯 OS 窗口状态
/// 记忆——Models 里没有所属域可以容纳它。
static IMMERSIVE_WAS_MAXIMIZED: AtomicBool = AtomicBool::new(false);

/// 进入/离开主窗口的 OS 全屏（由 `ui::immersive::set_immersive` 调用，
/// 那边只持有应用上下文）。
///
/// Windows 拒绝给 zoomed 窗口套全屏矩形：剥掉标题栏样式会隐式把它还原，随后那次
/// 按显示器尺寸的 SetWindowPos 会被整个吞掉（实测：从最大化窗口进入全屏会退回
/// 最大化前的小矩形，任务栏仍然可见）。所以进入前先取消最大化、离开后再重新
/// 最大化。SW_RESTORE 是排队的 OS 消息，而 gpui 的样式切换跑在 executor 回调里
/// ——实测它落在 restore 之前，所以进入路径会等到 `is_maximized` 真的清掉之后再切。
pub(crate) fn set_main_window_fullscreen(fullscreen: bool, cx: &mut App) {
    let Some(window) = find_main_window(cx) else {
        error!(
            fullscreen,
            "immersive: main window not found for fullscreen toggle"
        );
        return;
    };
    if fullscreen {
        let update = window.update(cx, |_, window, _| {
            let maximized = window.is_maximized();
            info!(maximized, "immersive: entering OS fullscreen");
            if maximized {
                IMMERSIVE_WAS_MAXIMIZED.store(true, Ordering::Relaxed);
                window.zoom_window();
                enter_fullscreen_when_restored(window, 16);
            } else {
                window.toggle_fullscreen();
            }
        });
        if let Err(err) = update {
            error!(fullscreen, %err, "immersive: entering fullscreen update failed");
        }
    } else {
        let was_maximized = IMMERSIVE_WAS_MAXIMIZED.swap(false, Ordering::Relaxed);
        info!(was_maximized, "immersive: leaving OS fullscreen");
        let update = window.update(cx, |_, window, _| window.toggle_fullscreen());
        if let Err(err) = update {
            error!(fullscreen, %err, "immersive: leaving fullscreen update failed");
        }
        if was_maximized {
            cx.spawn(async move |cx| {
                // 与 gpui 内部 restore 派生任务同一个前台 executor FIFO：
                // 这段跑起来时，样式与边界都已经恢复。
                cx.update(|cx| {
                    let _ = window.update(cx, |_, window, _| {
                        if window.is_fullscreen() {
                            // restore 还没落地（罕见）：再推迟一帧。
                            window.on_next_frame(|window, _| {
                                if !window.is_fullscreen() {
                                    window.zoom_window();
                                }
                            });
                        } else {
                            window.zoom_window();
                        }
                    });
                });
            })
            .detach();
        }
    }
}

/// 等排队的 SW_RESTORE 真的落地之后再切一次全屏，每帧重试一次（有上限：
/// 约 16 帧之后仍未还原说明深处出了问题，我们跳过这次切换，绝不去切一个
/// zoomed 窗口）。
fn enter_fullscreen_when_restored(window: &mut Window, remaining: u8) {
    if window.is_maximized() && remaining > 0 {
        window
            .on_next_frame(move |window, _| enter_fullscreen_when_restored(window, remaining - 1));
    } else if !window.is_maximized() {
        window.toggle_fullscreen();
    }
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
    let Some(window_information) = cx.global::<Models>().window_information.read(cx).clone() else {
        return WindowBounds::Maximized(Bounds::centered(None, size(px(1024.0), px(700.0)), cx));
    };

    let bounds = Bounds::centered(None, window_information.size, cx);
    if window_information.maximized {
        WindowBounds::Maximized(bounds)
    } else {
        WindowBounds::Windowed(bounds)
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

/// 启动打点脚手架（2026-10-20）：构造 `build()` 并把耗时记进 `costs`，
/// 用于 build_main_window 的视图树分账。拿到分账数据、惰性化落地后，
/// 这个辅助函数连同全部调用点一起清理。
fn timed_build<T>(
    label: &'static str,
    costs: &mut Vec<(&'static str, u128)>,
    build: impl FnOnce() -> T,
) -> T {
    let started = std::time::Instant::now();
    let value = build();
    costs.push((label, started.elapsed().as_micros()));
    value
}

fn build_main_window(
    window: &mut Window,
    cx: &mut App,
    toast_layer: Entity<ToastLayer>,
) -> Entity<MainWindow> {
    let window_title = tr!("APP_NAME").to_string();
    window.set_window_title(&window_title);

    // 视图树构造分解打点（2026-10-20）：`[startup] main window created` 实测
    // ~303ms，惰性化改造之前先知道时间花在哪个子视图上（教义 §1/§39：先量
    // 后改）。这是测量脚手架：拿到分账数据、惰性化落地后一并清理。
    let palette_started = std::time::Instant::now();
    let palette = CommandPalette::new(cx, window);
    tracing::info!(
        elapsed_us = palette_started.elapsed().as_micros() as u64,
        "[startup] view command_palette"
    );
    cx.set_global(CommandPaletteHolder::new(palette.clone()));

    // 各子视图构造耗时累加在这里，闭包末尾汇总成一行（见下面的
    // `[startup] view tree breakdown`）。
    let mut view_build_us: Vec<(&'static str, u128)> = Vec::new();

    let window_root = cx.new(|cx| {
        cx.observe_window_activation(window, |_, window, cx| {
            // 33ms 档的 OR 条件：桌面歌词悬浮窗开着时，主窗口失焦也不降档
            // （其主场景恰是主窗失焦的逐字扫色）。
            let broadcast_active =
                window.is_window_active() || crate::ui::desktop_lyrics::is_open(cx);
            cx.global::<PlaybackInterface>()
                .set_position_broadcast_active(broadcast_active);
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

            // 与所有其他 Models 写入一样的 `!=` 守卫：拖动窗口边框会按 OS 频率
            // 触发这里，而一次值没变的写入仍要为没有任何观察者的值付一个
            // notify effect。
            let next = WindowInformation { maximized, size };
            let changed = window_information.read(cx).as_ref() != Some(&next);
            if changed {
                window_information.write(cx, Some(next));
            }
        })
        .detach();

        cx.observe_window_appearance(window, |_, _, cx| {
            cx.refresh_windows();
        })
        .detach();

        let show_queue = cx.global::<Models>().show_queue.clone();
        let show_lyrics = cx.global::<Models>().show_lyrics.clone();
        let show_about = cx.global::<Models>().show_about.clone();
        let immersive = cx.global::<Models>().immersive.clone();
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
        cx.observe(&immersive, |_, _, cx| {
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

        let right_sidebar_started = std::time::Instant::now();
        let right_sidebar = RightSidebar::new(cx);
        view_build_us.push((
            "right_sidebar",
            right_sidebar_started.elapsed().as_micros(),
        ));
        let lyrics_model = right_sidebar.lyrics();

        // 桌面歌词悬浮窗的全局宿主：controls 开关、启动恢复、主窗关闭联动。
        // 主窗口同步登记 [frame] 探针的按窗口归因标签。
        let main_handle = window.window_handle();
        crate::register_frame_window_label(main_handle, "main");
        cx.set_global(crate::ui::desktop_lyrics::DesktopLyricsState::new(
            lyrics_model.clone(),
            main_handle,
        ));

        // 主窗口关闭 → 联动关闭悬浮窗：避免"只剩悬浮窗"的孤儿会话（应用
        // 无托盘，悬浮窗自身无法重建主窗口），随后全部窗口关闭自然退出。
        // 退出兜底：防抖窗口内最后一笔设置（含悬浮窗位置）同步落盘。
        cx.on_release(|_: &mut MainWindow, cx| {
            crate::ui::desktop_lyrics::close(cx);
            crate::settings::flush_pending_settings();
        })
        .detach();

        // OS 关闭路径（Alt+F4 / 系统菜单）拦截：永远返回 false，关闭语义
        // 由托盘模块按"关闭到托盘"设置决定（隐藏或退出）——与 ✕ 按钮
        // （window_header.rs）收敛到同一决策。Linux 无托盘，不装拦截。
        #[cfg(any(windows, target_os = "macos"))]
        window.on_window_should_close(cx, |window, cx| {
            crate::tray::handle_close_request(window, cx)
        });

        MainWindow {
            controls: timed_build("controls", &mut view_build_us, || {
                Controls::new(cx, show_queue.clone(), show_lyrics.clone())
            }),
            sidebar: timed_build("sidebar", &mut view_build_us, || {
                let nav_model = cx.global::<Models>().switcher_model.clone();
                Sidebar::new(cx, nav_model)
            }),
            right_sidebar,
            library: timed_build("library", &mut view_build_us, || Library::new(cx)),
            header: timed_build("header", &mut view_build_us, || Header::new(cx)),
            search: timed_build("search", &mut view_build_us, || SearchView::new(cx)),
            artist_picker: timed_build("artist_picker", &mut view_build_us, || {
                ArtistPickerView::new(cx)
            }),
            show_queue,
            show_lyrics,
            show_about,
            immersive,
            immersive_view: timed_build("immersive_view", &mut view_build_us, || {
                ImmersiveView::new(cx, lyrics_model)
            }),
            about_focus,
            missing_folder_dialog: timed_build(
                "missing_folder_dialog",
                &mut view_build_us,
                || MissingFolderDialog::new(cx),
            ),
            corrupt_settings_dialog: timed_build(
                "corrupt_settings_dialog",
                &mut view_build_us,
                || CorruptSettingsDialog::new(cx),
            ),
            palette,
            // 用一个非常小的全局图片缓存
            // 这纯粹是为了保证图片*迟早*都会从内存里被移除
            //
            // 如果你的视图用了很多图片，你需要有自己的图片
            // 缓存
            image_cache: timed_build("image_cache", &mut view_build_us, || {
                MelioraImageCache::new(12, cx)
            }),
            toast_layer,
        }
    });

    // 视图树分账汇总：一行看完全部子视图的构造耗时（相邻 `[startup]` 行的
    // 时间差减去这里的各项，剩余即平台建窗开销）。惰性化对象的决策数据。
    let breakdown = view_build_us
        .iter()
        .map(|(label, us)| format!("{label}={us}us"))
        .collect::<Vec<_>>()
        .join(" ");
    tracing::info!(breakdown = %breakdown, "[startup] view tree breakdown");

    window_root
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

    // 启动预取：建窗前只有 pool 是硬依赖（MelioraAssetSource 要用它），而
    // app_data / playback_session / settings 三个 JSON 与主题目录枚举和
    // "连接 + 迁移"互不依赖——在 create_pool 的 block_on 期间并行读，首帧前
    // 的串行磁盘读从四次降到一次（pool 自己）。句柄带进下面的 run() 闭包
    // 收敛；正常情况下收敛点只是 join，磁盘 IO 已并行做完。
    let preload_app_data = {
        let path = data_dir.join("app_data.json");
        // `Storage::new` 只是包一个路径，读取逻辑原样跑在 blocking 池上
        crate::RUNTIME.spawn_blocking(move || Storage::new(path).load_or_default())
    };
    let preload_session = {
        let path = data_dir.join("playback_session.json");
        crate::RUNTIME.spawn_blocking(move || PlaybackSessionStorageWorker::load(&path))
    };
    let preload_settings = {
        let path = data_dir.join("settings.json");
        crate::RUNTIME.spawn_blocking(move || crate::settings::create_settings(&path))
    };
    let preload_theme_options = {
        let path = data_dir.clone();
        crate::RUNTIME.spawn_blocking(move || crate::ui::theme::discover_theme_options(&path))
    };

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
        // 目前并不读 Fontconfig，所以退到最「尚可」的字体渲染选项——
        // 肯定有人不同意这么选，但 Grayscale 字体渲染至少能保证文本在所有屏幕上
        // 都显示正确，亚像素 AA 做不到这点
        #[cfg(target_os = "linux")]
        cx.set_text_rendering_mode(TextRenderingMode::Grayscale);

        let fonts_started_at = std::time::Instant::now();
        find_fonts(cx).expect("unable to load fonts");
        // 启动关键路径上的同步工作：全部内嵌字体（实测 2.75MB / 10 个文件）
        // 在主线程注册。此前 `[startup]` 只有数据库池一个标记，"打开到能听/
        // 看得见"这段时间完全不可测（轮转日志也取不到启动行，见 §44 第十五轮）。
        tracing::info!(
            elapsed_ms = fonts_started_at.elapsed().as_millis() as u64,
            "[startup] fonts registered"
        );

        // 字体注册之后到首帧之间是一段**从未被拆开过的静默等待**：9 个会话里
        // 4 次跨 279-300ms、5 次只 50-69ms（双峰，旧构建同样存在，日志在那段
        // 一行不出）。下面按步打点，把"我们的这段"分成 存储恢复 / 会话恢复 /
        // 建窗 / 托盘与媒体键 / 桌面歌词窗口 五档，剩下的差额就归首绘
        // （`[startup] first frame`）。
        // 出数只需一次常规构建；本段任何耗时结论都必须来自这几档，不许猜。
        // 打点记的是**距字体注册的累计毫秒**（相邻两行相减即得每档耗时）：这样
        // 锚点只读不写，也不会有"最后一次赋值没人读"的告警。
        let startup_anchor = std::time::Instant::now();
        macro_rules! startup_step {
            ($label:literal) => {
                tracing::info!(
                    since_fonts_ms = startup_anchor.elapsed().as_millis() as u64,
                    concat!("[startup] ", $label)
                );
            };
        }

        let storage = Storage::new(data_dir.join("app_data.json"));
        // 启动预取的收敛点（spawn 见 run() 外）：pool 建立期间这些任务已在
        // tokio blocking 池上并行跑完，正常情况这里只是 join。任务 panic 只
        // 可能是启动期缺陷，与原先同步调用 panic 等价，直接带上下文退出。
        let storage_data = crate::RUNTIME
            .block_on(preload_app_data)
            .expect("startup preload: app data task failed");
        startup_step!("app data restored");

        let session_file = data_dir.join("playback_session.json");
        let playback_session = crate::RUNTIME
            .block_on(preload_session)
            .expect("startup preload: playback session task failed");
        startup_step!("session restored");
        let initial_position = playback_session
            .queue_position
            .filter(|position| *position < playback_session.queue.len());
        let initial_track = initial_position
            .and_then(|position| playback_session.queue.get(position))
            .map(|item| CurrentTrack::new(item.get_path().clone()));

        let queue: Arc<RwLock<Arc<Vec<QueueItemData>>>> =
            Arc::new(RwLock::new(Arc::clone(&playback_session.queue)));

        let (queue_tx, queue_rx) = tokio::sync::watch::channel(playback_session.clone());
        crate::RUNTIME.spawn(PlaybackSessionStorageWorker::new(session_file, queue_rx).run());

        setup_settings_with_outcome(
            cx,
            data_dir.join("settings.json"),
            crate::RUNTIME
                .block_on(preload_settings)
                .expect("startup preload: settings task failed"),
        );
        setup_theme_with_options(
            cx,
            data_dir.clone(),
            crate::RUNTIME
                .block_on(preload_theme_options)
                .expect("startup preload: theme options task failed"),
        );
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
                content_epoch: 0,
            },
            &storage_data,
            initial_track,
            playback_session.shuffle,
            initial_repeat,
        );

        // 唤醒恢复出来的在线队列条目：签名流 URL 在应用关闭期间过期了，
        // 就在后台重新取一个新 URL，并让当前曲目/信息区都以新 URL 为键。
        #[cfg(feature = "online_sources")]
        refresh_restored_online_urls(cx, &queue, &playback_settings);

        super::keymap::load_default_keymap(cx);

        cx.set_global(modal::ModalActive(AtomicBool::new(false)));

        let settings_model = cx.global::<SettingsGlobal>().model.clone();
        // 定向 refresh 第一批（2026-09-26 审计 A-4）：观察回调做内容级 diff，
        // 内容未变的后台写入不再触发整窗重建（内容相等 ⇒ 渲染结果逐字节等
        // 价）。这里直接比较 `Settings` 的克隆而不是 serde_json::Value：
        // 结构体没有 skip/自定义序列化字段，两种判据等价，但克隆省掉每次
        // 通知（滑条拖动可达 60Hz）建一整棵 JSON AST 的分配。
        let mut rendered_settings = settings_model.read(cx).clone();
        cx.observe(&settings_model, move |model, cx| {
            let current = model.read(cx);
            if *current == rendered_settings {
                return;
            }
            rendered_settings = current.clone();
            cx.refresh_windows();
        })
        .detach();

        if !language.is_empty() {
            I18N_MANAGER.write_recovered().locale = Locale::new_from_locale_identifier(language);
        }

        let (scan_interface, scan_events) = start_scanner(pool.clone(), scanning_settings);
        let initial_health = cx.global::<Models>().settings_health.read(cx).clone();
        if matches!(initial_health, models::SettingsHealth::Ok) {
            // 把首次扫描推到启动之后：增量遍历会对每个音频文件做 stat，
            // 那趟磁盘 IO 过去会与首帧以及启动期的 DB 读取抢跑。扫描器本就
            // 是后台的，所以让行 1 s 毫无代价。
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                cx.update(|cx| {
                    if cx.has_global::<ScanInterface>() {
                        cx.global::<ScanInterface>().scan();
                    }
                });
            })
            .detach();
        } else {
            tracing::warn!("Settings file is corrupt; holding scanner until resolved");
        }
        // 扫描事件桥的镜像实体由 UI 侧传入（架构审计：library::scan 不再
        // 反向 import ui::models），取实体的时机与原先桥内取用完全一致。
        let scan_state = cx.global::<Models>().scan_state.clone();
        let playlist_tracker = cx.global::<Models>().playlist_tracker.clone();
        scan_interface.start_broadcast(scan_events, scan_state, playlist_tracker, cx);

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

        // 收听统计记录器：由播放事件循环喂数据，退出时刷进 SQLite。
        cx.set_global(crate::stats::StatsHandle::new());

        let mut playback_interface: PlaybackInterface = PlaybackThread::start(
            queue.clone(),
            playback_settings,
            last_volume,
            playback_session,
            queue_tx,
        );
        // 事件消费循环在 UI 层（ui::playback_bridge）：图集回收漏斗的帧间
        // drain 是 UI 职责，playback 层只保留纯命令发送端。
        super::playback_bridge::start_broadcast(&mut playback_interface, cx);

        if !parse_args_and_prepare(&playback_interface)
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
        // 托盘唤回的兜底宿主（正常托盘流程只显隐不销毁，见 restore_main_window）。
        cx.set_global(MainWindowHost(toast_layer_entity.clone()));

        // 启动时发起每日免费 VIP 领取，让已登录的账号不必打开设置页也能拿到
        // 今天的奖励。领取本身由持久化的「今日已领」标记守卫，所以每个自然日
        // 这里只会是空操作（零请求）。
        #[cfg(feature = "kugou")]
        crate::ui::kugou::claim_daily_vip_async();

        // 自动更新：发布更新器状态、清理上一次更新重启留下的残余，并在启动
        // 高峰平息后检查一次 GitHub Releases。没有新版本时是空操作；
        // 下载/安装管线住在 updater.rs。
        #[cfg(feature = "online")]
        crate::updater::init(cx);
        #[cfg(feature = "online")]
        crate::updater::schedule_startup_check(cx);

        // 退出应用时更新 `StorageData` 并把它保存到文件系统。
        cx.on_app_quit({
            move |cx| {
                let data = StorageData::new(cx);
                let storage = storage.clone();

                // 刷出最后那段未听完的记录，免得一次会话的最后几秒丢失。
                // `write_rows` 走 tokio，所以在后台线程上经 RUNTIME 的上下文执行。
                let pending_rows = {
                    let stats = cx.global::<crate::stats::StatsHandle>();
                    let mut recorder = stats.0.lock_recovered();
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
            // 这个中间绑定让 `cx` 的共享借用先结束，`write` 才能拿到可变借用——
            // 把 global() 直接串进 write() 是编译不过的。
            let window_information_entity = cx.global::<Models>().window_information.clone();
            window_information_entity.write(cx, Some(window_information));
        }

        let main_window = ensure_main_window(cx, toast_layer_entity).unwrap();
        startup_step!("main window created");

        // 桌面集成（SMTC + 托盘壳）与首帧无依赖：元数据由随后的播放事件经
        // PbcHandle 推送，托盘壳只在用户交互时才被需要。整段挪出启动关键路径
        // ——用 0ms 定时器任务而不是 on_next_frame：后者依赖帧回调，而
        // start_minimized / 全遮挡下没有帧产生，桌面集成将永远装不上。
        // 定时器不依赖帧，且失败处理与原先同步调用完全一致（退化为"无桌面
        // 集成"）；PbcHandle 的全局与事件观察者在同一个 cx.update 块里装配，
        // 不存在"观察者先于全局存在"的窗口。
        let main_window_for_integrations = main_window.clone();
        cx.spawn(async move |cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(0))
                .await;
            cx.update(|cx| {
                let _ = main_window_for_integrations.update(cx, |_, window, cx| {
                    init_pbc_task(cx, window);
                });
                register_pbc_event_handlers(cx);

                // 系统托盘（Windows/macOS）：壳事件经 mpsc 编组回 GPUI drain 任务；
                // init 内部登记主窗口句柄，start_minimized 时以隐藏状态启动。
                // Linux 无托盘模块，不建壳（关闭行为由 window_header 就地回退）。
                #[cfg(any(windows, target_os = "macos"))]
                crate::tray::init(cx);
                startup_step!("tray and media keys wired");
            });
        })
        .detach();

        // 桌面歌词悬浮窗：设置开启即随主窗口恢复（句柄与歌词实体已在主
        // 窗口构建时注入 Global）。
        if cx
            .global::<SettingsGlobal>()
            .model
            .read(cx)
            .interface
            .desktop_lyrics
        {
            crate::ui::desktop_lyrics::open(cx);
        }
        startup_step!("desktop lyrics window opened");
    });

    Ok(())
}

/// 恢复出来的在线队列条目带的是签名流 URL，应用在关闭期间它们会过期；给每一条
/// 重新取一个新 URL，长时间闲置后的会话就还能播。若刷新后的条目正是当前播放的
/// 那首，当前曲目路径会同步更新，让信息区（封面/曲名/艺人）始终以新 URL 为键。
/// 网络调用都会跳到 Tokio 运行时上。
#[cfg(feature = "online_sources")]
fn refresh_restored_online_urls(
    cx: &mut App,
    queue: &Arc<RwLock<Arc<Vec<QueueItemData>>>>,
    playback: &crate::settings::playback::PlaybackSettings,
) {
    use crate::playback::queue::OnlineIdentity;

    let mut stale: Vec<(usize, OnlineIdentity)> = queue
        .read_recovered()
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

    // 取所有权，好让派生出的任务活得比本作用域更久；`as_str` 是 feature 门控的。
    #[cfg(feature = "kugou")]
    let kugou_quality = playback.online_quality.as_str().to_string();

    let queue = Arc::clone(queue);
    let current_track = cx.global::<PlaybackInfo>().current_track.clone();
    let current_track_path = current_track
        .read(cx)
        .as_ref()
        .map(|track| track.get_path().clone());

    // 此刻只有当前曲目必须有一个有效 URL；恢复出的队列里其余条目等真要被播放时
    // 由播放线程按需刷新（`PlaybackThread::refresh_expired_online_url`）。
    // 先刷当前这一条，给启动期的高峰封顶，让兜底路径去处理尾巴，而不是为队列里
    // 每一首曲目都烧掉一次串行的 HTTP 往返。
    const STARTUP_REFRESH_CAP: usize = 8;
    // 单次拿锁完成全部位置匹配：闭包里逐元素重锁在长队列下是 O(n) 次
    // 获取，且两次持有之间纯同步、无 await，合并安全。
    if let Some(current) = current_track_path.as_ref() {
        let q = queue.read_recovered();
        if let Some(pos) = stale
            .iter()
            .position(|(idx, _)| q.get(*idx).is_some_and(|item| item.get_path() == current))
        {
            stale.swap(0, pos);
        }
    }
    stale.truncate(STARTUP_REFRESH_CAP);

    cx.spawn(async move |cx| {
        for (idx, identity) in stale {
            let display = queue
                .read_recovered()
                .get(idx)
                .and_then(|item| item.persisted_display())
                .unwrap_or((None, None, None, None));

            // 共用的刷新：取一个新 URL 并把它重新登记进该音源的流注册表
            // （歌词 / 收藏 / 下载都按这张注册表解析，所以刷新过的 URL 必须
            // 重新登记，否则轮到刷新后的流播起来时这些功能就断）。
            let ctx = crate::online_sources::RefreshContext {
                #[cfg(feature = "kugou")]
                kugou_quality: kugou_quality.as_str(),
                display,
            };
            let url = crate::online_sources::refresh_online_url(&identity, &ctx).await;

            let Some(url) = url else { continue };

            let was_current = {
                let Ok(mut guard) = queue.write() else {
                    continue;
                };
                let Some(item) = Arc::make_mut(&mut guard).get_mut(idx) else {
                    continue;
                };
                // 自快照以来队列可能已经移位；绝不用来覆盖另一个条目
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

/// 解析用户给出的命令行参数并处理它们。如果命令行里提供了用于播放的文件，
/// 返回 true。
fn parse_args_and_prepare(interface: &PlaybackInterface) -> bool {
    let args = Args::parse();

    if let Some(files) = &args.files {
        info!("Queueing files found in arguments: {:?}", files);

        interface.queue_list(
            files
                .iter()
                .map(|path| QueueItemData::new(path.clone(), None, None))
                .collect(),
        );
    }

    args.files.is_some()
}
