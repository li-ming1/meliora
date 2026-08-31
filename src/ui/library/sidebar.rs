use cntp_i18n::tr;
use gpui::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
// the online-source account avatars are the only users of these
#[cfg(any(feature = "kugou", feature = "netease"))]
use gpui::InteractiveElement;
use std::time::{Duration, Instant};

use crate::settings::{save_settings, Settings, SettingsGlobal};

use crate::settings::storage::DEFAULT_SIDEBAR_WIDTH;
use crate::ui::constants::COLLAPSED_SIDEBAR_WIDTH;
use crate::ui::scroll_follow::ease_out_cubic;
use crate::ui::theme::LIGHT_THEME_ID;

use crate::ui::components::icons::{FOLDER, MOON, MUSIC, SETTINGS, SIDEBAR, SIDEBAR_INACTIVE, SUN};
#[cfg(any(feature = "kugou", feature = "netease"))]
use crate::ui::components::icons::icon;
#[cfg(feature = "kugou")]
use crate::ui::components::icons::KUGOU;
#[cfg(feature = "netease")]
use crate::ui::components::icons::NETEASE;
#[cfg(feature = "kugou")]
use crate::kugou::shared_client;
#[cfg(feature = "netease")]
use crate::netease::shared_client as netease_shared_client;
use crate::ui::components::tooltip::build_tooltip;
use crate::{
    ui::{
        components::{
            icons::{DISC, USERS},
            nav_button::nav_button,
            resizable::{ResizeEdge, resizable},
            sidebar::{sidebar, sidebar_item, sidebar_separator},
        },
        library::{NavigationHistory, ViewSwitchMessage, sidebar::playlists::PlaylistList},
        models::Models,
        theme::Theme,
    },
};
#[cfg(any(feature = "kugou", feature = "netease"))]
use crate::ui::components::managed_image::{ManagedImageKey, managed_image};

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
                this.child(section_label(tr!("SIDEBAR_LIBRARY", "Music Library"), &theme))
            })
            .child(
                sidebar_item("albums")
                    .icon(DISC)
                    .when(!collapsed, |this| this.child(tr!("ALBUMS", "Albums")))
                    .when(collapsed, |this| {
                        this.collapsed().collapsed_label(tr!("ALBUMS"))
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.nav_model.update(cx, |_, cx| {
                            cx.emit(ViewSwitchMessage::Albums);
                        });
                    }))
                    .when(
                        matches!(
                            sidebar_view,
                            ViewSwitchMessage::Albums | ViewSwitchMessage::Release(_, _)
                        ),
                        |this| this.active(),
                    ),
            )
            .child(
                sidebar_item("artists")
                    .icon(USERS)
                    .when(!collapsed, |this| this.child(tr!("ARTISTS", "Artists")))
                    .when(collapsed, |this| {
                        this.collapsed().collapsed_label(tr!("ARTISTS"))
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.nav_model.update(cx, |_, cx| {
                            cx.emit(ViewSwitchMessage::Artists);
                        });
                    }))
                    .when(
                        matches!(
                            sidebar_view,
                            ViewSwitchMessage::Artists | ViewSwitchMessage::Artist(_)
                        ),
                        |this| this.active(),
                    ),
            )
            .child(
                sidebar_item("tracks")
                    .icon(MUSIC)
                    .when(!collapsed, |this| this.child(tr!("TRACKS", "Tracks")))
                    .when(collapsed, |this| {
                        this.collapsed().collapsed_label(tr!("TRACKS"))
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.nav_model.update(cx, |_, cx| {
                            cx.emit(ViewSwitchMessage::Tracks);
                        });
                    }))
                    .when(matches!(sidebar_view, ViewSwitchMessage::Tracks), |this| {
                        this.active()
                    }),
            )
            .child(
                sidebar_item("files")
                    .icon(FOLDER)
                    .when(!collapsed, |this| this.child(tr!("FILES", "Files")))
                    .when(collapsed, |this| {
                        this.collapsed().collapsed_label(tr!("FILES"))
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.nav_model.update(cx, |_, cx| {
                            cx.emit(ViewSwitchMessage::Files);
                        });
                    }))
                    .when(matches!(sidebar_view, ViewSwitchMessage::Files), |this| {
                        this.active()
                    }),
            )
            // 分组二：播放列表
            .child(sidebar_separator())
            .when(!collapsed, |this| {
                this.child(section_label(
                    tr!("SIDEBAR_PLAYLISTS", "Playlists"),
                    &theme,
                ))
            })
            .child(self.playlists.clone());

        // 分组三：酷狗音乐（顺序在播放列表之后）
        #[cfg(feature = "kugou")]
        let sidebar_content = sidebar_content.child(sidebar_separator()).child(
            div()
                .flex()
                .flex_col()
                .child(
                    sidebar_item("kugou-playlists")
                        .icon(KUGOU)
                        .when(!collapsed, |this| {
                            this.child(tr!("KUGOU_PLAYLISTS", "KuGou Playlists"))
                        })
                        .when(collapsed, |this| {
                            this.collapsed().collapsed_label(tr!("KUGOU_PLAYLISTS"))
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.nav_model.update(cx, |_, cx| {
                                cx.emit(ViewSwitchMessage::KugouPlaylists);
                            });
                        }))
                        .when(
                            matches!(sidebar_view, ViewSwitchMessage::KugouPlaylists),
                            |this| this.active(),
                        ),
                )
                .child(
                    sidebar_item("kugou-ranks")
                        .icon(MUSIC)
                        .when(!collapsed, |this| {
                            this.child(tr!("KUGOU_RANKS", "Rankings"))
                        })
                        .when(collapsed, |this| {
                            this.collapsed().collapsed_label(tr!("KUGOU_RANKS"))
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.nav_model.update(cx, |_, cx| {
                                cx.emit(ViewSwitchMessage::KugouRanks);
                            });
                        }))
                        .when(
                            matches!(sidebar_view, ViewSwitchMessage::KugouRanks),
                            |this| this.active(),
                        ),
                ),
        );

        // 分组四：网易云音乐（顺序在酷狗之后）
        #[cfg(feature = "netease")]
        let sidebar_content = sidebar_content.child(sidebar_separator()).child(
            div()
                .flex()
                .flex_col()
                .child(
                    sidebar_item("netease-playlists")
                        .icon(NETEASE)
                        // fallback for NETEASE_PLAYLISTS is defined by the
                        // playlists page (single definition point)
                        .when(!collapsed, |this| {
                            this.child(tr!("NETEASE_PLAYLISTS"))
                        })
                        .when(collapsed, |this| {
                            this.collapsed().collapsed_label(tr!("NETEASE_PLAYLISTS"))
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.nav_model.update(cx, |_, cx| {
                                cx.emit(ViewSwitchMessage::NeteasePlaylists);
                            });
                        }))
                        .when(
                            matches!(sidebar_view, ViewSwitchMessage::NeteasePlaylists),
                            |this| this.active(),
                        ),
                )
                .child(
                    sidebar_item("netease-ranks")
                        .icon(MUSIC)
                        .when(!collapsed, |this| {
                            this.child(tr!("NETEASE_RANKS"))
                        })
                        .when(collapsed, |this| {
                            this.collapsed().collapsed_label(tr!("NETEASE_RANKS"))
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.nav_model.update(cx, |_, cx| {
                                cx.emit(ViewSwitchMessage::NeteaseRanks);
                            });
                        }))
                        .when(
                            matches!(sidebar_view, ViewSwitchMessage::NeteaseRanks),
                            |this| this.active(),
                        ),
                ),
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
                cx.listener(
                    move |this, _, _window, cx| {
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
                    },
                )
            })
        };

        // 展开时三个按钮平分侧边栏宽度；折叠时竖排居中
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
                .child(div().flex_1().flex().justify_center().child(settings_button))
                .child(div().flex_1().flex().justify_center().child(theme_button))
                .child(div().flex_1().flex().justify_center().child(collapse_button))
        };

        let bottom = div()
            .mt_auto()
            .w_full()
            .flex()
            .flex_col()
            .pt(px(6.0))
            .child(sidebar_separator())
            .child(div().h(px(4.0)))
            .child(utility_row);

        // 酷狗账号头像：圆形圆环框，放侧边栏最底部；点击进入酷狗歌单 / 未登录打开设置
        #[cfg(feature = "kugou")]
        let avatar_button = {
            let profile = shared_client().cached_user_profile();
            let logged_in = profile.as_ref().is_some_and(|p| !p.nickname.is_empty());
            let tooltip_text = logged_in
                .then(|| {
                    profile
                        .as_ref()
                        .map(|p| SharedString::from(p.nickname.clone()))
                })
                .flatten()
                .unwrap_or_else(|| tr!("KUGOU_LOGIN").into());

            let avatar_url: Option<SharedString> = profile
                .as_ref()
                .filter(|p| !p.avatar_url.is_empty())
                .map(|p| p.avatar_url.clone().into());
            let avatar_inner = div()
                .size_full()
                .rounded_full()
                .overflow_hidden()
                .flex()
                .items_center()
                .justify_center()
                .when_some(avatar_url.clone(), |this, url| {
                    this.child(
                        managed_image(
                            ("kugou-sidebar-avatar", 0usize),
                            ManagedImageKey::HttpCover(url),
                        )
                        .size_full(),
                    )
                })
                .when(avatar_url.is_none(), |this| {
                    this.child(
                        icon(if logged_in { USERS } else { KUGOU })
                            .size(px(14.0))
                            .text_color(theme.text_secondary),
                    )
                });

            div()
                .id("kugou-avatar")
                .when(collapsed, |this| this.mx_auto())
                .w(px(28.0))
                .h(px(28.0))
                .rounded_full()
                .bg(theme.album_art_background)
                .cursor_pointer()
                .hover(|this| this.bg(theme.nav_button_hover))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if logged_in {
                        this.nav_model.update(cx, |_, cx| {
                            cx.emit(ViewSwitchMessage::KugouPlaylists);
                        });
                    } else {
                        crate::ui::settings::open_settings_window(cx);
                    }
                }))
                .tooltip(build_tooltip(tooltip_text))
                .child(avatar_inner)
        };

        #[cfg(feature = "kugou")]
        let bottom = bottom.child(avatar_button);

        // 网易云账号头像：与酷狗头像并排；点击进入网易云歌单 / 未登录打开设置
        #[cfg(feature = "netease")]
        let netease_avatar_button = {
            let profile = netease_shared_client().cached_user_profile();
            let logged_in = profile.as_ref().is_some_and(|p| !p.nickname.is_empty());
            let tooltip_text = logged_in
                .then(|| {
                    profile
                        .as_ref()
                        .map(|p| SharedString::from(p.nickname.clone()))
                })
                .flatten()
                .unwrap_or_else(|| tr!("NETEASE_LOGIN").into());

            let avatar_url: Option<SharedString> = profile
                .as_ref()
                .filter(|p| !p.avatar_url.is_empty())
                .map(|p| p.avatar_url.clone().into());
            let avatar_inner = div()
                .size_full()
                .rounded_full()
                .overflow_hidden()
                .flex()
                .items_center()
                .justify_center()
                .when_some(avatar_url.clone(), |this, url| {
                    this.child(
                        managed_image(
                            ("netease-sidebar-avatar", 0usize),
                            ManagedImageKey::HttpCover(url),
                        )
                        .size_full(),
                    )
                })
                .when(avatar_url.is_none(), |this| {
                    this.child(
                        icon(if logged_in { USERS } else { NETEASE })
                            .size(px(14.0))
                            .text_color(theme.text_secondary),
                    )
                });

            div()
                .id("netease-avatar")
                .when(collapsed, |this| this.mx_auto())
                .w(px(28.0))
                .h(px(28.0))
                .rounded_full()
                .bg(theme.album_art_background)
                .cursor_pointer()
                .hover(|this| this.bg(theme.nav_button_hover))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if logged_in {
                        this.nav_model.update(cx, |_, cx| {
                            cx.emit(ViewSwitchMessage::NeteasePlaylists);
                        });
                    } else {
                        crate::ui::settings::open_settings_window(cx);
                    }
                }))
                .tooltip(build_tooltip(tooltip_text))
                .child(avatar_inner)
        };

        #[cfg(feature = "netease")]
        let bottom = bottom.child(netease_avatar_button);

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