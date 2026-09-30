use cntp_i18n::tr;
use gpui::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
// the online-source account avatars are the only users of these
#[cfg(any(feature = "kugou", feature = "netease"))]
use gpui::InteractiveElement;
use std::time::{Duration, Instant};

use crate::settings::{Settings, SettingsGlobal, save_settings};

use crate::settings::storage::DEFAULT_SIDEBAR_WIDTH;
use crate::ui::constants::COLLAPSED_SIDEBAR_WIDTH;
use crate::ui::scroll_follow::ease_out_cubic;
use crate::ui::theme::LIGHT_THEME_ID;

#[cfg(feature = "kugou")]
use crate::kugou::shared_client;
#[cfg(feature = "netease")]
use crate::netease::shared_client as netease_shared_client;
#[cfg(feature = "kugou")]
use crate::ui::components::icons::KUGOU;
#[cfg(feature = "netease")]
use crate::ui::components::icons::NETEASE;
#[cfg(any(feature = "kugou", feature = "netease"))]
use crate::ui::components::icons::RANKING;
#[cfg(any(feature = "kugou", feature = "netease"))]
use crate::ui::components::icons::icon;
use crate::ui::components::icons::{FOLDER, MOON, MUSIC, SETTINGS, SIDEBAR, SIDEBAR_INACTIVE, SUN};
use crate::ui::components::tooltip::build_tooltip;
use crate::ui::design::ICON_SM;
use crate::ui::{
    components::{
        icons::{DISC, USERS},
        nav_button::nav_button,
        resizable::{ResizeEdge, resizable},
        sidebar::{SidebarItem, sidebar, sidebar_item, sidebar_separator},
    },
    library::{NavigationHistory, ViewSwitchMessage, sidebar::playlists::PlaylistList},
    models::Models,
    theme::Theme,
};
#[cfg(any(feature = "kugou", feature = "netease"))]
use gpui::{ClickEvent, Div, FontWeight, Stateful};

mod playlists;

/// 侧边栏分组标题：仅展开时显示，折叠时隐藏以节省宽度。
fn section_label(label: impl Into<SharedString>, theme: &Theme) -> impl IntoElement {
    div()
        .px(px(8.0))
        .pb(px(2.0))
        .pt(px(10.0))
        .flex_shrink_0()
        .overflow_hidden()
        .text_xs()
        .text_color(theme.text_secondary)
        .child(label.into())
}

/// SIDEBAR_LOGIN 的唯一定义点：酷狗/网易云两个账号药丸共用，避免 i18n 重复定义。
#[cfg(any(feature = "kugou", feature = "netease"))]
fn login_label() -> SharedString {
    tr!("SIDEBAR_LOGIN", "Log In").into()
}

/// 在线音源账号入口。展开时是「品牌图标 + 昵称/登录」药丸——恒用品牌图标而
/// 不是头像照片：导航语境里品牌可识别性优先，照片只会变成一张突兀的随机图；
/// 折叠时退化为 28px 图标圆钮，与折叠态其他条目的形态一致。
#[cfg(any(feature = "kugou", feature = "netease"))]
#[allow(clippy::too_many_arguments)] // pill styling facets; callers pass theme-derived values
fn account_pill(
    id: &'static str,
    brand_icon: &'static str,
    label: SharedString,
    tooltip_text: SharedString,
    logged_in: bool,
    collapsed: bool,
    theme: &Theme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    // 登录态用主文字色，未登录退到次要色，一眼可辨。
    let tint = if logged_in {
        theme.text
    } else {
        theme.text_secondary
    };

    let pill = div()
        .id(id)
        .h(px(28.0))
        .rounded_full()
        .border_1()
        // load-bearing color，同 sidebar_item：不设边框色 hover 就不生效
        .border_color(theme.background_primary)
        .bg(theme.background_primary)
        .cursor_pointer()
        .flex()
        .items_center()
        .overflow_hidden()
        .hover(|this| this.bg(theme.nav_button_hover))
        .active(|this| this.bg(theme.nav_button_active))
        .tooltip(build_tooltip(tooltip_text))
        .on_click(on_click);

    if collapsed {
        pill.w(px(28.0))
            .justify_center()
            .child(icon(brand_icon).size(ICON_SM).text_color(tint))
    } else {
        pill.w_full()
            .px(px(9.0))
            .gap(px(6.0))
            .child(
                icon(brand_icon)
                    .size(ICON_SM)
                    .flex_shrink_0()
                    .text_color(tint),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .truncate()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(tint)
                    .child(label),
            )
    }
}

/// Width animation driven on the sidebar view: collapses/expands `animated_sidebar_width`.
struct WidthTween {
    from: f32,
    to: f32,
    started_at: Instant,
}

const SIDEBAR_TWEEN_DURATION: Duration = Duration::from_millis(180);

pub struct Sidebar {
    playlists: Entity<PlaylistList>,
    nav_model: Entity<NavigationHistory>,
    settings: Entity<Settings>,
    width_tween: Option<WidthTween>,
}

impl Sidebar {
    pub fn new(cx: &mut App, nav_model: Entity<NavigationHistory>) -> Entity<Self> {
        cx.new(|cx| {
            cx.observe(&nav_model, |_, _, cx| cx.notify()).detach();

            let settings = cx.global::<SettingsGlobal>().model.clone();
            cx.observe(&settings, |_, _, cx| cx.notify()).detach();

            let sidebar_width = cx.global::<Models>().sidebar_width.clone();
            cx.observe(&sidebar_width, |_, _, cx| cx.notify()).detach();

            let sidebar_collapsed = cx.global::<Models>().sidebar_collapsed.clone();
            cx.observe(&sidebar_collapsed, |_, _, cx| cx.notify())
                .detach();

            let animated_sidebar_width = cx.global::<Models>().animated_sidebar_width.clone();
            cx.observe(&animated_sidebar_width, |_, _, cx| cx.notify())
                .detach();

            Self {
                playlists: PlaylistList::new(cx, nav_model.clone()),
                nav_model,
                settings,
                width_tween: None,
            }
        })
    }

    /// 一个主导航条目：展开时显示文字、折叠时退化短标签；点击切换到对应
    /// 视图，`active` 时高亮（如 Albums 同时覆盖唱片详情页）。
    fn nav_entry(
        cx: &mut Context<Self>,
        collapsed: bool,
        id: &'static str,
        icon: &'static str,
        label: impl Into<SharedString>,
        message: ViewSwitchMessage,
        active: bool,
    ) -> SidebarItem {
        let label = label.into();
        sidebar_item(id)
            .icon(icon)
            .when(!collapsed, |this| this.child(label.clone()))
            .when(collapsed, |this| this.collapsed().collapsed_label(label))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.nav_model.update(cx, |_, cx| cx.emit(message));
            }))
            .when(active, |this| this.active())
    }
}

impl Render for Sidebar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>().clone();
        let current_view = self.nav_model.read(cx).current();
        let two_column = cx
            .global::<SettingsGlobal>()
            .model
            .read(cx)
            .interface
            .two_column_library;

        // In two-column mode, the sidebar should reflect the *left* pane, not the
        // right (detail) pane.  Derive the effective view the same way Library does.
        let sidebar_view = if two_column && current_view.is_detail_page() {
            self.nav_model
                .read(cx)
                .last_matching(ViewSwitchMessage::is_key_page)
                .unwrap_or(current_view)
        } else {
            current_view
        };
        let sidebar_width = cx.global::<Models>().sidebar_width.clone();
        let sidebar_collapsed_entity = cx.global::<Models>().sidebar_collapsed.clone();
        let animated_sidebar_width = cx.global::<Models>().animated_sidebar_width.clone();
        let collapsed = *sidebar_collapsed_entity.read(cx);

        // Drive the open/close tween and keep the rendered width in sync with the source of truth.
        let target_width = if collapsed {
            f32::from(COLLAPSED_SIDEBAR_WIDTH)
        } else {
            f32::from(*sidebar_width.read(cx))
        };
        if let Some(tween) = &mut self.width_tween {
            let progress = (tween.started_at.elapsed().as_secs_f32()
                / SIDEBAR_TWEEN_DURATION.as_secs_f32())
            .clamp(0.0, 1.0);
            let width = tween.from + (tween.to - tween.from) * ease_out_cubic(progress);
            animated_sidebar_width.update(cx, |v, cx| {
                *v = px(width);
                cx.notify();
            });
            if progress >= 1.0 {
                self.width_tween = None;
                animated_sidebar_width.update(cx, |v, cx| {
                    *v = px(target_width);
                    cx.notify();
                });
            } else {
                window.request_animation_frame();
            }
        } else {
            animated_sidebar_width.update(cx, |v, cx| {
                if f32::from(*v) != target_width {
                    *v = px(target_width);
                    cx.notify();
                }
            });
        }
        let animated_width = *animated_sidebar_width.read(cx);

        let sidebar_content = sidebar()
            .width(animated_width)
            .id("main-sidebar")
            .h_full()
            .max_h_full()
            .pt(px(8.0))
            .pb(px(4.0))
            .pl(px(7.0))
            .pr(px(8.0))
            .when(!collapsed, |this| this.overflow_hidden())
            .flex()
            .flex_col()
            .when(collapsed, |this| this.items_center())
            // 分组一：音乐库
            .when(!collapsed, |this| {
                this.child(section_label(
                    tr!("SIDEBAR_LIBRARY", "Music Library"),
                    &theme,
                ))
            })
            .child(Self::nav_entry(
                cx,
                collapsed,
                "albums",
                DISC,
                tr!("ALBUMS", "Albums"),
                ViewSwitchMessage::Albums,
                matches!(
                    sidebar_view,
                    ViewSwitchMessage::Albums | ViewSwitchMessage::Release(_, _)
                ),
            ))
            .child(Self::nav_entry(
                cx,
                collapsed,
                "artists",
                USERS,
                tr!("ARTISTS", "Artists"),
                ViewSwitchMessage::Artists,
                matches!(
                    sidebar_view,
                    ViewSwitchMessage::Artists | ViewSwitchMessage::Artist(_)
                ),
            ))
            .child(Self::nav_entry(
                cx,
                collapsed,
                "tracks",
                MUSIC,
                tr!("TRACKS", "Tracks"),
                ViewSwitchMessage::Tracks,
                matches!(sidebar_view, ViewSwitchMessage::Tracks),
            ))
            .child(Self::nav_entry(
                cx,
                collapsed,
                "files",
                FOLDER,
                tr!("FILES", "Files"),
                ViewSwitchMessage::Files,
                matches!(sidebar_view, ViewSwitchMessage::Files),
            ))
            // 分组二：播放列表
            .child(sidebar_separator())
            .when(!collapsed, |this| {
                this.child(section_label(tr!("SIDEBAR_PLAYLISTS", "Playlists"), &theme))
            })
            .child(self.playlists.clone());

        // 分组三：酷狗音乐（顺序在播放列表之后）
        #[cfg(feature = "kugou")]
        let sidebar_content = sidebar_content
            .child(sidebar_separator())
            .when(!collapsed, |this| {
                this.child(section_label(tr!("KUGOU_SECTION"), &theme))
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(Self::nav_entry(
                        cx,
                        collapsed,
                        "kugou-playlists",
                        KUGOU,
                        tr!("KUGOU_PLAYLISTS", "KuGou Playlists"),
                        ViewSwitchMessage::KugouPlaylists,
                        matches!(sidebar_view, ViewSwitchMessage::KugouPlaylists),
                    ))
                    .child(Self::nav_entry(
                        cx,
                        collapsed,
                        "kugou-ranks",
                        RANKING,
                        tr!("KUGOU_RANKS", "Rankings"),
                        ViewSwitchMessage::KugouRanks,
                        matches!(sidebar_view, ViewSwitchMessage::KugouRanks),
                    )),
            );

        // 分组四：网易云音乐（顺序在酷狗之后）
        #[cfg(feature = "netease")]
        let sidebar_content = sidebar_content
            .child(sidebar_separator())
            .when(!collapsed, |this| {
                this.child(section_label(tr!("NETEASE_SECTION"), &theme))
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(Self::nav_entry(
                        cx,
                        collapsed,
                        "netease-playlists",
                        NETEASE,
                        // fallback for NETEASE_PLAYLISTS is defined by the
                        // playlists page (single definition point)
                        tr!("NETEASE_PLAYLISTS"),
                        ViewSwitchMessage::NeteasePlaylists,
                        matches!(sidebar_view, ViewSwitchMessage::NeteasePlaylists),
                    ))
                    .child(Self::nav_entry(
                        cx,
                        collapsed,
                        "netease-ranks",
                        RANKING,
                        tr!("NETEASE_RANKS"),
                        ViewSwitchMessage::NeteaseRanks,
                        matches!(sidebar_view, ViewSwitchMessage::NeteaseRanks),
                    )),
            );

        // 底部固定：酷狗账号卡片 + 设置 / 主题 / 折叠
        let settings_button = nav_button("open-settings", SETTINGS)
            .tooltip(build_tooltip(tr!("SETTINGS")))
            .w(px(36.0))
            .h(px(34.0))
            .on_click(|_, _, cx| {
                crate::ui::settings::open_settings_window(cx);
            });
        let theme_button = {
            let settings = self.settings.clone();
            let is_light =
                self.settings.read(cx).interface.theme.as_deref() == Some(LIGHT_THEME_ID);
            nav_button("toggle-theme", if is_light { MOON } else { SUN })
                .tooltip(build_tooltip(tr!("INTERFACE_THEME")))
                .w(px(36.0))
                .h(px(34.0))
                .on_click(move |_, _, cx| {
                    settings.update(cx, |s, cx| {
                        s.interface.theme = if s.interface.theme.as_deref() == Some(LIGHT_THEME_ID)
                        {
                            None
                        } else {
                            Some(LIGHT_THEME_ID.to_string())
                        };
                        save_settings(cx, s);
                        cx.notify();
                    });
                })
        };
        let collapse_button = {
            nav_button(
                "sidebar-toggle",
                if collapsed { SIDEBAR } else { SIDEBAR_INACTIVE },
            )
            .tooltip(build_tooltip(if collapsed {
                tr!("EXPAND_SIDEBAR", "Expand Sidebar")
            } else {
                tr!("COLLAPSE_SIDEBAR", "Collapse Sidebar")
            }))
            .w(px(36.0))
            .h(px(34.0))
            .on_click({
                let sidebar_collapsed_entity = sidebar_collapsed_entity.clone();
                let sidebar_width = sidebar_width.clone();
                let animated_sidebar_width = animated_sidebar_width.clone();
                cx.listener(move |this, _, _window, cx| {
                    let target_collapsed = !*sidebar_collapsed_entity.read(cx);
                    sidebar_collapsed_entity.update(cx, |v, cx| {
                        *v = target_collapsed;
                        cx.notify();
                    });
                    let from = f32::from(*animated_sidebar_width.read(cx));
                    let to = if target_collapsed {
                        f32::from(COLLAPSED_SIDEBAR_WIDTH)
                    } else {
                        f32::from(*sidebar_width.read(cx))
                    };
                    this.width_tween = Some(WidthTween {
                        from,
                        to,
                        started_at: Instant::now(),
                    });
                })
            })
        };

        // 展开时设置/主题靠左，折叠键（侧边栏自身操作）靠右；折叠时竖排居中
        let utility_row = if collapsed {
            div()
                .w_full()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(4.0))
                .child(settings_button)
                .child(theme_button)
                .child(collapse_button)
        } else {
            div()
                .w_full()
                .flex()
                .items_center()
                .px(px(5.0))
                .gap(px(2.0))
                .child(settings_button)
                .child(theme_button)
                .child(div().flex_1())
                .child(collapse_button)
        };

        let bottom = div()
            .mt_auto()
            .w_full()
            .flex()
            .flex_col()
            .pt(px(6.0))
            .child(sidebar_separator())
            .child(div().h(px(4.0)));

        // 酷狗账号药丸：点击深链到设置页酷狗分区（账户卡含登录/救赎等操作）
        #[cfg(feature = "kugou")]
        let kugou_pill = {
            let profile = shared_client().cached_user_profile();
            let logged_in = profile.as_ref().is_some_and(|p| !p.nickname.is_empty());
            let nickname = logged_in
                .then(|| {
                    profile
                        .as_ref()
                        .map(|p| SharedString::from(p.nickname.clone()))
                })
                .flatten();
            let label: SharedString = nickname.clone().unwrap_or_else(login_label);
            let tooltip_text = nickname.unwrap_or_else(|| tr!("KUGOU_LOGIN").into());

            account_pill(
                "kugou-account",
                KUGOU,
                label,
                tooltip_text,
                logged_in,
                collapsed,
                &theme,
                cx.listener(|_, _, _, cx| {
                    // 统一深链到设置页的酷狗分区（登录/救赎等账户操作都在那里）
                    crate::ui::settings::open_settings_window_with_section(
                        cx,
                        crate::ui::settings::SettingsSectionKind::Kugou,
                    );
                }),
            )
        };

        // 网易云账号药丸：与酷狗药丸并排；点击深链到设置页网易云分区
        #[cfg(feature = "netease")]
        let netease_pill = {
            let profile = netease_shared_client().cached_user_profile();
            let logged_in = profile.as_ref().is_some_and(|p| !p.nickname.is_empty());
            let nickname = logged_in
                .then(|| {
                    profile
                        .as_ref()
                        .map(|p| SharedString::from(p.nickname.clone()))
                })
                .flatten();
            let label: SharedString = nickname.clone().unwrap_or_else(login_label);
            let tooltip_text = nickname.unwrap_or_else(|| tr!("NETEASE_LOGIN").into());

            account_pill(
                "netease-account",
                NETEASE,
                label,
                tooltip_text,
                logged_in,
                collapsed,
                &theme,
                cx.listener(|_, _, _, cx| {
                    crate::ui::settings::open_settings_window_with_section(
                        cx,
                        crate::ui::settings::SettingsSectionKind::Netease,
                    );
                }),
            )
        };

        // 账号行：展开时两个药丸平分一行（内缩与导航条目对齐）；折叠时竖排圆钮
        #[cfg(any(feature = "kugou", feature = "netease"))]
        let bottom = {
            let account_row = if collapsed {
                let col = div().w_full().flex().flex_col().items_center().gap(px(4.0));
                #[cfg(feature = "kugou")]
                let col = col.child(kugou_pill);
                #[cfg(feature = "netease")]
                let col = col.child(netease_pill);
                col
            } else {
                let row = div().w_full().flex().gap(px(6.0)).px(px(9.0));
                #[cfg(feature = "kugou")]
                let row = row.child(kugou_pill);
                #[cfg(feature = "netease")]
                let row = row.child(netease_pill);
                row
            };
            bottom.child(account_row).child(div().h(px(4.0)))
        };

        let bottom = bottom.child(utility_row);

        let sidebar_content = sidebar_content.child(bottom);

        if collapsed || self.width_tween.is_some() {
            // During an open/close tween (or when collapsed) render at the animated width.
            div()
                .w(animated_width)
                .h_full()
                .flex_shrink_0()
                .border_r_1()
                .border_color(theme.border_color)
                .child(sidebar_content)
                .into_any_element()
        } else {
            resizable(
                "main-sidebar-resizable",
                sidebar_width.clone(),
                ResizeEdge::Right,
            )
            .min_size(px(175.0))
            .max_size(px(350.0))
            .default_size(DEFAULT_SIDEBAR_WIDTH)
            .h_full()
            .child(sidebar_content)
            .into_any_element()
        }
    }
}
