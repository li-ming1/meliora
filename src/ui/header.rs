use super::{models::Models, theme::Theme};
use crate::{
    library::scan::ScanEvent,
    settings::SettingsGlobal,
    ui::{
        components::{
            icons::{FOLDER_BOLT, FOLDER_SEARCH, SEARCH, icon},
            tooltip::build_complex_tooltip,
            window_header::header,
        },
        constants::{
            TITLEBAR_LEFT_PAD_BOTTOM, TITLEBAR_LEFT_PAD_TOP, TITLEBAR_LEFT_PAD_X,
        },
        global_actions::Search,
        library::view_nav::nav_buttons,
    },
};
use cntp_i18n::tr;
use gpui::{prelude::FluentBuilder, *};

pub struct Header {
    scan_status: Entity<ScanStatus>,
}

impl Header {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let settings = cx.global::<SettingsGlobal>().model.clone();

        cx.new(|cx| {
            cx.observe(&settings, |_, _, cx| cx.notify()).detach();

            let sidebar_width = cx.global::<Models>().sidebar_width.clone();
            cx.observe(&sidebar_width, |_, _, cx| cx.notify()).detach();

            let animated_sidebar_width = cx.global::<Models>().animated_sidebar_width.clone();
            cx.observe(&animated_sidebar_width, |_, _, cx| cx.notify())
                .detach();

            let sidebar_collapsed = cx.global::<Models>().sidebar_collapsed.clone();
            cx.observe(&sidebar_collapsed, |_, _, cx| cx.notify())
                .detach();

            Self {
                scan_status: ScanStatus::new(cx),
            }
        })
    }
}

impl Render for Header {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut header = header().main_window(true);

        // 侧边栏视觉延伸块：与侧边栏同宽同背景，占满顶栏左侧高度，
        // 使侧边栏看起来一直延伸到顶栏。左右箭头随后放在其右侧。
        // macOS 顶栏由系统的应用菜单占位，不参与此视觉延伸。
        #[cfg(not(target_os = "macos"))]
        {
            let collapsed = *cx.global::<Models>().sidebar_collapsed.read(cx);
            let sidebar_width = *cx.global::<Models>().animated_sidebar_width.read(cx);
            let theme = cx.global::<Theme>();
            header = header.left(
                div()
                    .w(sidebar_width)
                    .h(px(37.0))
                    .ml(-TITLEBAR_LEFT_PAD_X)
                    .mt(-TITLEBAR_LEFT_PAD_TOP)
                    .mb(-TITLEBAR_LEFT_PAD_BOTTOM)
                    .flex_shrink_0()
                    .bg(theme.background_primary)
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(collapsed, |this| this.pl(px(8.0)))
                    .child(img("!bundled:images/logo.png").size(px(20.0)).rounded_sm())
                    .when(!collapsed, |this| {
                        this.gap(px(8.0))
                            .pl(px(14.0))
                            .child(
                                div()
                                    .text_color(theme.text)
                                    .font_weight(FontWeight::BOLD)
                                    .text_size(px(15.0))
                                    .child("Meliora"),
                            )
                    }),
            );
        }

        // macOS 顶栏由系统应用菜单占位，补一个等宽占位保持搜索框居中
        #[cfg(target_os = "macos")]
        {
            header = header.left(div().w(px(200.0)));
        }

        // 三区顶栏：左=品牌；中=弹性定位的居中搜索框；右=箭头+扫描状态。
        // 空菜单栏已移除（命令已迁移到设置页与命令面板），顶栏只留高频项。
        header = header.left(div().flex_1().min_w(px(8.0)));

        header = header.left(search_bar(cx));

        header = header.left(div().flex_1().min_w(px(8.0)));

        header = header.left(
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .child(nav_buttons())
                .child(self.scan_status.clone()),
        );

        header
    }
}

pub struct ScanStatus {
    scan_model: Entity<ScanEvent>,
}

impl ScanStatus {
    pub fn new(cx: &mut App) -> Entity<Self> {
        let scan_model = cx.global::<Models>().scan_state.clone();

        cx.new(|cx| {
            cx.observe(&scan_model, |_, _, cx| {
                cx.notify();
            })
            .detach();

            Self { scan_model }
        })
    }
}

impl Render for ScanStatus {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let status = self.scan_model.read(cx);

        div()
            .id("scan-status")
            .flex()
            .text_sm()
            .when(
                !matches!(
                    status,
                    ScanEvent::ScanCompleteIdle | ScanEvent::TargetedRescanComplete
                ),
                |this| {
                    this.child(
                        div().mr(px(8.0)).pt(px(5.0)).h_full().child(
                            icon(match status {
                                ScanEvent::Cleaning
                                | ScanEvent::PlaylistsUpdated(_)
                                | ScanEvent::ScanProgress { .. }
                                | ScanEvent::WaitingForMissingFolderDecision { .. } => {
                                    FOLDER_SEARCH
                                }
                                ScanEvent::ScanCompleteWatching => FOLDER_BOLT,
                                _ => unreachable!(),
                            })
                            .size(px(14.0)),
                        ),
                    )
                },
            )
            .tooltip(build_complex_tooltip(|_, cx| {
                let theme = cx.global::<Theme>();
                div()
                    .max_w(px(350.))
                    .text_color(theme.text)
                    .child(
                        div()
                            .mb(px(2.0))
                            .font_weight(FontWeight::BOLD)
                            .child(tr!("SCAN_WATCHING_TOOLTIP_HEADER", "Watching for changes")),
                    )
                    .child(div().child(tr!(
                        "SCAN_WATCHING_TOOLTIP_BODY",
                        "Meliora is watching your files for updates and will automatically \
                        update the library when changes are made."
                    )))
                    .child(
                        div()
                            .mt(px(2.0))
                            .text_xs()
                            .text_color(theme.text_secondary)
                            .child(tr!(
                                "SCAN_WATCHING_TOOLTIP_DISABLE_HINT",
                                "You can disable this functionality in the library settings."
                            )),
                    )
            }))
            .text_color(theme.text_secondary)
            .child(match status {
                ScanEvent::ScanCompleteIdle
                | ScanEvent::ScanCompleteWatching
                | ScanEvent::TargetedRescanComplete => SharedString::from(""),
                ScanEvent::ScanProgress { current, total } => {
                    if *total == u64::MAX {
                        // Total unknown (discovery still ongoing)
                        tr!(
                            "SCAN_PROGRESS_DISCOVERING",
                            "Scanning {{current}} files...",
                            current = current
                        )
                        .into()
                    } else {
                        // Total known (discovery complete)
                        tr!(
                            "SCAN_PROGRESS_SCANNING",
                            "Scanning {{percentage}}%",
                            percentage = (*current as f64 / *total as f64 * 100.0).round()
                        )
                        .into()
                    }
                }
                ScanEvent::Cleaning => SharedString::from(""),
                ScanEvent::PlaylistsUpdated(_) => SharedString::from(""),
                ScanEvent::WaitingForMissingFolderDecision { .. } => {
                    tr!("SCANNING_MISSING_DIALOG_TITLE").into()
                }
            })
    }
}

/// 顶栏搜索框：样式与常规搜索框一致，点击后打开搜索调色板
/// （复用原侧边栏搜索按钮的 Search action）。
pub fn search_bar(cx: &App) -> impl IntoElement {
    let theme = cx.global::<Theme>();

    div()
        .id("search-bar")
        .occlude()
        .flex()
        .items_center()
        .gap(px(6.0))
        .w(px(280.0))
        .h(px(30.0))
        .px(px(10.0))
        .rounded_md()
        .border_1()
        .border_color(theme.border_color)
        .bg(theme.background_primary)
        .cursor(CursorStyle::PointingHand)
        .hover(|style: StyleRefinement| {
            style
                .bg(theme.nav_button_hover)
                .border_color(theme.nav_button_hover_border)
        })
        .on_click(|_, window, cx| {
            window.dispatch_action(Box::new(Search), cx);
        })
        .child(icon(SEARCH).size(px(14.0)).text_color(theme.text_secondary))
        .child(
            div()
                .text_sm()
                .text_color(theme.text_secondary)
                .child(tr!("SEARCH_PLACEHOLDER", "Search songs, artists or lyrics")),
        )
}