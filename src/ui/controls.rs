mod info_section;
mod replaygain;

pub use info_section::InfoSection;

use crate::{
    playback::{events::RepeatState, interface::PlaybackInterface, thread::PlaybackState},
    settings::SettingsGlobal,
    settings::storage::{DEFAULT_CONTROLS_LEFT_WIDTH, DEFAULT_CONTROLS_RIGHT_WIDTH},
    ui::{
        components::{
            context::context,
            icons::{
                BUBBLE_TEXT, MAXIMIZE, MENU, MICROPHONE, NEXT_TRACK, PAUSE, PLAY, PREV_TRACK,
                REPEAT, REPEAT_OFF, REPEAT_ONCE, SHUFFLE, icon,
            },
            menu::{menu, menu_check_item, menu_item},
            resizable::{ResizeEdge, resizable},
            slider::{SLIDER_CHANGE_INTERVAL, slider},
            tooltip::{build_tooltip, tooltip_container},
            transition::{HoverTransition, TransitionExt},
        },
        design::{ICON_MD, ICON_SM},
        global_actions::{Next, PlayPause, Previous, StopAfterCurrent},
        models::{Models, POSITION_DISPLAY_QUANTUM_MS, PlaybackInfo},
        theme::Theme,
        util::{CachedTimeText, format_duration, seek_to_fraction, volume_toggle_target},
    },
};
use cntp_i18n::tr;
use gpui::{InteractiveElement, prelude::FluentBuilder, *};

use self::replaygain::ReplayGainButton;

/// `entity` 变化时重新渲染；这是那些自身不带额外状态的全局模型镜像的默认接线。
pub(crate) fn observe_notify<O: 'static, T: 'static>(cx: &mut Context<O>, entity: &Entity<T>) {
    cx.observe(entity, |_, _, cx| {
        cx.notify();
    })
    .detach();
}

/// 走带按钮共用的按下处理：阻止冒泡并抑制默认处理，把动作留给点击处理器。
fn swallow_press(_: &MouseDownEvent, window: &mut Window, cx: &mut App) {
    cx.stop_propagation();
    window.prevent_default();
}

/// 上/下一曲按钮各自派发哪种走带动作。
#[derive(Clone, Copy)]
enum TransportAction {
    Previous,
    Next,
}

impl TransportAction {
    fn dispatch(self, window: &mut Window, cx: &mut App) {
        match self {
            TransportAction::Previous => window.dispatch_action(Box::new(Previous), cx),
            TransportAction::Next => window.dispatch_action(Box::new(Next), cx),
        }
    }
}

/// 「上一曲 / 播放 / 下一曲」这组按钮的一个端头：30×28 的按钮，点击时派发自己的
/// 动作、吞掉按下事件、悬停时淡入背景。外侧圆角由调用方加。
fn transport_button(
    id: &'static str,
    icon_path: &'static str,
    tooltip: impl Fn(&mut Window, &mut App) -> AnyView + 'static,
    action: TransportAction,
    theme: &Theme,
) -> HoverTransition {
    div()
        .w(px(30.0))
        .h(px(28.0))
        .bg(theme.playback_button)
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .id(id)
        .active(|style| style.bg(theme.playback_button_active))
        .on_mouse_down(MouseButton::Left, swallow_press)
        .on_click(move |_, window, cx| action.dispatch(window, cx))
        .child(icon(icon_path).size(ICON_MD))
        .tooltip(tooltip)
        .into_transition(theme.playback_button, theme.playback_button_hover)
}

/// 播放栏 25×25 方形开关（音量、队列、歌词、ReplayGain）共用的外观：圆角方形，
/// 带标准播放按钮的背景、悬停与激活着色。调用方传入一个有状态的 div，
/// 在其上叠加图标、tooltip 与处理器。
fn playback_toggle_button<D>(button: D, theme: &Theme) -> D
where
    D: Styled + StatefulInteractiveElement,
{
    button
        .rounded(px(theme.radius_sm))
        .w(px(25.0))
        .h(px(25.0))
        .mt(px(2.0))
        .flex()
        .items_center()
        .justify_center()
        .border_color(theme.playback_button_border)
        .bg(theme.playback_button)
        .cursor_pointer()
        .hover(|this| this.bg(theme.playback_button_hover))
        .active(|this| this.bg(theme.playback_button_active))
}

pub struct Controls {
    info_section: Entity<InfoSection>,
    scrubber: Entity<Scrubber>,
    secondary_controls: Entity<SecondaryControls>,
    left_width: Entity<Pixels>,
    right_width: Entity<Pixels>,
}

impl Controls {
    pub fn new(cx: &mut App, show_queue: Entity<bool>, show_lyrics: Entity<bool>) -> Entity<Self> {
        let models = cx.global::<Models>();
        let left_width = models.controls_left_width.clone();
        let right_width = models.controls_right_width.clone();
        cx.new(|cx| Self {
            info_section: InfoSection::new(cx),
            scrubber: Scrubber::new(cx),
            secondary_controls: SecondaryControls::new(cx, show_queue, show_lyrics),
            left_width,
            right_width,
        })
    }
}

impl Render for Controls {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _render = crate::tally_render(crate::RenderedView::Controls);
        let decorations = window.window_decorations();
        let theme = cx.global::<Theme>();

        div()
            .flex()
            .h(px(68.0))
            .w_full()
            .bg(theme.background_secondary)
            .border_t_1()
            .border_color(theme.border_color)
            .map(|div| match decorations {
                Decorations::Server => div,
                Decorations::Client { tiling } => div
                    .when(!(tiling.bottom || tiling.left), |div| {
                        div.rounded_bl(px(theme.radius_md))
                    })
                    .when(!(tiling.bottom || tiling.right), |div| {
                        div.rounded_br(px(theme.radius_md))
                    }),
            })
            .on_any_mouse_down(|_, _, cx| {
                cx.stop_propagation();
            })
            .child(
                resizable(
                    "controls-left-resizable",
                    self.left_width.clone(),
                    ResizeEdge::Right,
                )
                .min_size(px(150.0))
                .max_size(px(500.0))
                .default_size(DEFAULT_CONTROLS_LEFT_WIDTH)
                .border_width(px(0.0))
                // 刻意不加缓存：外壳重构后这个子树会失效（缓存重放会嫁接旧框架）
                .child(self.info_section.clone()),
            )
            .child(self.scrubber.clone())
            .child(
                resizable(
                    "controls-right-resizable",
                    self.right_width.clone(),
                    ResizeEdge::Left,
                )
                .min_size(px(180.0))
                .max_size(px(500.0))
                .default_size(DEFAULT_CONTROLS_RIGHT_WIDTH)
                .border_width(px(0.0))
                .child(
                    AnyView::from(self.secondary_controls.clone())
                        .cached(StyleRefinement::default().flex().w_full().h_full()),
                ),
            )
    }
}

pub struct PlaybackSection {
    info: PlaybackInfo,
}

impl PlaybackSection {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let info = cx.global::<PlaybackInfo>().clone();
            observe_notify(cx, &info.playback_state);
            observe_notify(cx, &info.shuffling);
            observe_notify(cx, &info.repeating);
            observe_notify(cx, &info.stop_after_current);

            Self { info }
        })
    }
}

impl Render for PlaybackSection {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _render = crate::tally_render(crate::RenderedView::PlaybackSection);
        let state = self.info.playback_state.read(cx);
        let shuffling = self.info.shuffling.read(cx);
        let repeating = *self.info.repeating.read(cx);
        let stop_after_current = *self.info.stop_after_current.read(cx);
        let theme = cx.global::<Theme>();
        let always_repeat = cx
            .global::<SettingsGlobal>()
            .model
            .read(cx)
            .playback
            .always_repeat;
        let repeat_icon_color = match repeating {
            RepeatState::NotRepeating => theme.text,
            RepeatState::Repeating => theme.playback_button_toggled,
            RepeatState::RepeatingOne => theme.playback_button_repeat_one,
        };

        div()
            .mr(auto())
            .ml(auto())
            .mt(px(5.0))
            .flex()
            .w_full()
            .absolute()
            .child(
                div()
                    .rounded(px(theme.radius_sm))
                    .w(px(28.0))
                    .h(px(25.0))
                    .mt(px(3.0))
                    .mr(px(6.0))
                    .ml_auto()
                    .border_color(theme.playback_button_border)
                    .flex()
                    .items_center()
                    .justify_center()
                    .hover(|style| style.bg(theme.playback_button_hover).cursor_pointer())
                    .id("header-shuffle-button")
                    .active(|style| style.bg(theme.playback_button_active))
                    .on_mouse_down(MouseButton::Left, swallow_press)
                    .on_click(|_, _, cx| {
                        cx.global::<PlaybackInterface>().toggle_shuffle();
                    })
                    .child(icon(SHUFFLE).size(ICON_SM).when(*shuffling, |this| {
                        this.text_color(theme.playback_button_toggled)
                    }))
                    .when_else(
                        *shuffling,
                        |this| this.tooltip(build_tooltip(tr!("STOP_SHUFFLING", "Stop Shuffling"))),
                        |this| this.tooltip(build_tooltip(tr!("SHUFFLE"))),
                    ),
            )
            .child(
                div()
                    .rounded(px(theme.radius_sm))
                    .border_color(theme.playback_button_border)
                    .border_1()
                    .flex()
                    .child(
                        transport_button(
                            "header-prev-button",
                            PREV_TRACK,
                            build_tooltip(tr!("PREVIOUS_TRACK", "Previous Track")),
                            TransportAction::Previous,
                            theme,
                        )
                        .rounded_l(px(theme.radius_sm)),
                    )
                    .child(
                        context("header-play-button-context")
                            .with(
                                div()
                                    .w(px(32.0))
                                    .h(px(28.0))
                                    .bg(theme.playback_button)
                                    .border_l(px(1.0))
                                    .border_r(px(1.0))
                                    .border_color(theme.playback_button_border)
                                    .relative()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .id("header-play-button")
                                    .active(|style| style.bg(theme.playback_button_active))
                                    .on_mouse_down(MouseButton::Left, swallow_press)
                                    .on_click(|_, window, cx| {
                                        window.dispatch_action(Box::new(PlayPause), cx);
                                    })
                                    .when(*state == PlaybackState::Playing, |div| {
                                        div.child(icon(PAUSE).size(ICON_MD))
                                            .tooltip(build_tooltip(tr!("PAUSE")))
                                    })
                                    .when(*state != PlaybackState::Playing, |div| {
                                        div.child(icon(PLAY).size(ICON_MD))
                                            .tooltip(build_tooltip(tr!("PLAY")))
                                    })
                                    .when(stop_after_current, |this| {
                                        this.child(
                                            div()
                                                .id("stop-after-current-indicator")
                                                .absolute()
                                                .top(px(3.0))
                                                .right(px(3.0))
                                                .size(px(6.0))
                                                .rounded_full()
                                                .bg(theme.stop_after_current_indicator)
                                                .tooltip(build_tooltip(tr!(
                                                    "STOP_AFTER_CURRENT_TOOLTIP",
                                                    "Will stop after current track"
                                                ))),
                                        )
                                    })
                                    .into_transition(
                                        theme.playback_button,
                                        theme.playback_button_hover,
                                    ),
                            )
                            .child(menu().item(menu_check_item(
                                "stop-after-current-menu-item",
                                stop_after_current,
                                tr!("ACTION_STOP_AFTER_CURRENT"),
                                |_, window, cx| {
                                    window.dispatch_action(Box::new(StopAfterCurrent), cx);
                                },
                            ))),
                    )
                    .child(
                        transport_button(
                            "header-next-button",
                            NEXT_TRACK,
                            build_tooltip(tr!("NEXT_TRACK", "Next Track")),
                            TransportAction::Next,
                            theme,
                        )
                        .rounded_r(px(theme.radius_sm)),
                    ),
            )
            .child(
                div().mr_auto().child(
                    context("repeat-context")
                        .with(
                            div()
                                .rounded(px(theme.radius_sm))
                                .w(px(28.0))
                                .h(px(25.0))
                                .mt(px(3.0))
                                .ml(px(6.0))
                                .border_color(theme.playback_button_border)
                                .flex()
                                .items_center()
                                .justify_center()
                                .hover(|style| {
                                    style.bg(theme.playback_button_hover).cursor_pointer()
                                })
                                .id("header-repeat-button")
                                .active(|style| style.bg(theme.playback_button_active))
                                .on_mouse_down(MouseButton::Left, swallow_press)
                                .on_click(move |_, _, cx| match repeating {
                                    RepeatState::NotRepeating => cx
                                        .global::<PlaybackInterface>()
                                        .set_repeat(RepeatState::Repeating),
                                    RepeatState::Repeating => cx
                                        .global::<PlaybackInterface>()
                                        .set_repeat(RepeatState::RepeatingOne),
                                    RepeatState::RepeatingOne => cx
                                        .global::<PlaybackInterface>()
                                        .set_repeat(RepeatState::NotRepeating),
                                })
                                .tooltip(build_tooltip(match repeating {
                                    RepeatState::NotRepeating => {
                                        tr!("REPEAT")
                                    }
                                    RepeatState::Repeating => tr!("REPEAT_ONE"),
                                    RepeatState::RepeatingOne => {
                                        if always_repeat {
                                            tr!("REPEAT")
                                        } else {
                                            tr!("STOP_REPEATING", "Stop Repeating")
                                        }
                                    }
                                }))
                                .child(
                                    icon(match repeating {
                                        RepeatState::NotRepeating | RepeatState::Repeating => {
                                            REPEAT
                                        }
                                        RepeatState::RepeatingOne => REPEAT_ONCE,
                                    })
                                    .size(ICON_SM)
                                    .text_color(repeat_icon_color),
                                ),
                        )
                        .child(
                            div().bg(theme.elevated_background).child(
                                menu()
                                    .when(!always_repeat, |menu| {
                                        menu.item(menu_item(
                                            "repeat-not-repeat",
                                            Some(REPEAT_OFF),
                                            tr!("REPEAT_OFF", "Off"),
                                            move |_, _, cx| {
                                                cx.global::<PlaybackInterface>()
                                                    .set_repeat(RepeatState::NotRepeating);
                                            },
                                        ))
                                    })
                                    .item(menu_item(
                                        "repeat-repeat",
                                        Some(REPEAT),
                                        tr!("REPEAT", "Repeat"),
                                        move |_, _, cx| {
                                            cx.global::<PlaybackInterface>()
                                                .set_repeat(RepeatState::Repeating);
                                        },
                                    ))
                                    .item(menu_item(
                                        "repeat-repeat-one",
                                        Some(REPEAT_ONCE),
                                        tr!("REPEAT_ONE", "Repeat One"),
                                        move |_, _, cx| {
                                            cx.global::<PlaybackInterface>()
                                                .set_repeat(RepeatState::RepeatingOne);
                                        },
                                    )),
                            ),
                        ),
                ),
            )
    }
}

pub struct Scrubber {
    position: Entity<u64>,
    duration: Entity<u64>,
    playback_section: Entity<PlaybackSection>,
    time_labels: CachedTimeText<(SharedString, SharedString, SharedString)>,
    /// 上次触发重绘的 position 量化值（`POSITION_DISPLAY_QUANTUM_MS`
    /// 粒度）：存原始位置做跨档比较，暂停中的亚档位 seek（如
    /// 500→400ms）跨档时仍即时重绘，同档内偏差 ≤1 档、视觉不可见。
    drawn_position_quantum: u64,
}

impl Scrubber {
    fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let position_model = cx.global::<PlaybackInfo>().position.clone();
            let duration_model = cx.global::<PlaybackInfo>().duration.clone();
            // 进度条每拍实际移动亚像素、时间标签整秒缓存——按 250ms 档
            // 量化 notify，压掉"播放中主窗口 30fps 持续重绘"泵。
            cx.observe(&position_model, |this: &mut Self, pos, cx| {
                let quantum = *pos.read(cx) / POSITION_DISPLAY_QUANTUM_MS;
                if quantum != this.drawn_position_quantum {
                    this.drawn_position_quantum = quantum;
                    cx.notify();
                }
            })
            .detach();
            observe_notify(cx, &duration_model);

            Self {
                position: position_model,
                duration: duration_model,
                playback_section: PlaybackSection::new(cx),
                // 整秒键缓存：u64::MAX 种子永不匹配真实秒数，强制首建。
                time_labels: CachedTimeText::unseeded(),
                drawn_position_quantum: 0,
            }
        })
    }
}

impl Render for Scrubber {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _render = crate::tally_render(crate::RenderedView::Scrubber);
        let theme = cx.global::<Theme>();
        let position_ms = *self.position.read(cx);
        let duration_ms = *self.duration.read(cx);
        let position_secs = position_ms / 1_000;
        let duration_secs = duration_ms / 1_000;

        // 整秒键没变时复用已格式化的标签
        let (position_text, duration_text, remaining_text) = self.time_labels.resolve(
            position_secs,
            duration_secs,
            |position_secs, duration_secs| {
                let remaining_secs = duration_secs.saturating_sub(position_secs);
                (
                    SharedString::from(format_duration(position_secs as i64, true)),
                    SharedString::from(format_duration(duration_secs as i64, true)),
                    SharedString::from(format!(
                        "-{}",
                        format_duration(remaining_secs as i64, true)
                    )),
                )
            },
        );

        let window_width = window.viewport_size().width;

        div()
            .pl(px(13.0))
            .pr(px(13.0))
            .border_x(px(1.0))
            .border_color(theme.border_color)
            .flex_grow(1.0)
            .flex()
            .flex_col()
            .text_size(px(15.0))
            .font_weight(FontWeight::SEMIBOLD)
            .relative()
            .child(
                div()
                    .w_full()
                    .flex()
                    .relative()
                    .items_end()
                    .mt(px(6.0))
                    .mb(px(6.0))
                    .child(
                        div()
                            .mr(px(6.0))
                            .line_height(rems(1.0))
                            .child(position_text),
                    )
                    .when(window_width > px(900.0), |this| {
                        this.child(
                            div()
                                .line_height(rems(1.0))
                                .border_color(rgb(0x4b5563))
                                .border_l(px(2.0))
                                .pl(px(6.0))
                                .text_color(rgb(0xcbd5e1))
                                .child(duration_text),
                        )
                    })
                    .child(self.playback_section.clone())
                    .child(div().h(px(30.0)))
                    .child(
                        div()
                            .ml(auto())
                            .line_height(rems(1.0))
                            .child(remaining_text),
                    ),
            )
            .child(
                slider()
                    .w_full()
                    .h(px(6.0))
                    .rounded(px(theme.radius_sm))
                    .id("scrubber-back")
                    .change_interval(SLIDER_CHANGE_INTERVAL)
                    .value(if duration_ms > 0 {
                        position_ms as f32 / duration_ms as f32
                    } else {
                        0.0
                    })
                    .on_change(move |v, _, cx| {
                        // 门控 + seek 共享实现（immersive 沉浸页进度条同款）：
                        // duration_ms > 0 且非 Stopped 才 seek。
                        seek_to_fraction(v as f64, duration_ms, cx);
                    }),
            )
    }
}

#[derive(IntoElement)]
struct SidebarToggleButton {
    div: Stateful<Div>,
    icon_path: &'static str,
    active: bool,
}

impl StatefulInteractiveElement for SidebarToggleButton {}

impl InteractiveElement for SidebarToggleButton {
    fn interactivity(&mut self) -> &mut gpui::Interactivity {
        self.div.interactivity()
    }
}

impl Styled for SidebarToggleButton {
    fn style(&mut self) -> &mut StyleRefinement {
        self.div.style()
    }
}

impl RenderOnce for SidebarToggleButton {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let icon_color = if self.active {
            theme.playback_button_toggled
        } else {
            theme.text
        };

        playback_toggle_button(self.div, theme)
            .child(icon(self.icon_path).size(ICON_SM).text_color(icon_color))
    }
}

fn sidebar_toggle_button(
    id: impl Into<ElementId>,
    icon_path: &'static str,
    active: bool,
) -> SidebarToggleButton {
    SidebarToggleButton {
        div: div().id(id.into()),
        icon_path,
        active,
    }
}

pub struct SecondaryControls {
    info: PlaybackInfo,
    show_queue: Entity<bool>,
    show_lyrics: Entity<bool>,
    immersive: Entity<bool>,
    replaygain_button: Entity<ReplayGainButton>,
}

impl SecondaryControls {
    pub fn new(cx: &mut App, show_queue: Entity<bool>, show_lyrics: Entity<bool>) -> Entity<Self> {
        cx.new(|cx| {
            let info = cx.global::<PlaybackInfo>().clone();
            observe_notify(cx, &info.volume);

            let immersive = cx.global::<Models>().immersive.clone();
            cx.observe(&immersive, |_, _, cx| cx.notify()).detach();

            Self {
                info,
                show_queue,
                show_lyrics,
                immersive,
                replaygain_button: ReplayGainButton::new(cx),
            }
        })
    }
}

impl Render for SecondaryControls {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let volume = *self.info.volume.read(cx);
        let prev_volume = *self.info.prev_volume.read(cx);
        // 静音切换的判定（图标 + 目标音量）与沉浸播放卡共用；下面的 tooltip
        // 仍留在本地。
        let (volume_target, volume_icon) = volume_toggle_target(volume, prev_volume);
        let show_queue = self.show_queue.clone();
        let show_lyrics = self.show_lyrics.clone();
        let lyrics_active = *self.show_lyrics.read(cx);
        let queue_active = *self.show_queue.read(cx);
        let immersive_active = *self.immersive.read(cx);
        // 桌面歌词开关态来自 interface settings 实体（读取即注册渲染依赖，
        // toggle 的 set_enabled notify 会驱动本按钮重绘）。
        let desktop_lyrics_active = cx
            .global::<SettingsGlobal>()
            .model
            .read(cx)
            .interface
            .desktop_lyrics;

        div().flex().w_full().h_full().child(
            div()
                .px(px(18.0))
                .flex()
                .w_full()
                .my_auto()
                .pb(px(2.0))
                .child(
                    playback_toggle_button(div().id("volume-button"), theme)
                        .child(icon(volume_icon).size(ICON_SM))
                        .on_click(move |_, _, cx| {
                            cx.global::<PlaybackInterface>().set_volume(volume_target);
                        })
                        .when(volume <= 0.0, |div| {
                            div.tooltip(build_tooltip(tr!("UNMUTE", "Unmute")))
                        })
                        .when(volume > 0.0, |div| {
                            div.tooltip(build_tooltip(tr!("MUTE", "Mute")))
                        }),
                )
                .child(
                    div()
                        .id("volume-container")
                        .mx(px(4.0))
                        .flex_1()
                        .min_w(px(50.0))
                        .hoverable_tooltip(build_volume_tooltip(self.info.volume.clone()))
                        .child(
                            slider()
                                .w_full()
                                .h(px(6.0))
                                .mt(px(11.0))
                                .rounded(px(theme.radius_sm))
                                .id("volume")
                                .change_interval(SLIDER_CHANGE_INTERVAL)
                                .value(volume as f32)
                                .on_double_click(|_, cx| {
                                    cx.global::<PlaybackInterface>().set_volume(1.0);
                                })
                                .on_change(move |v, _, cx| {
                                    cx.global::<PlaybackInterface>().set_volume(v as f64);
                                }),
                        )
                        .on_scroll_wheel(move |ev, _, cx| {
                            // 两个分支都化归到同一个缩放系数（0.01666666 ≈ 满音量的
                            // 每滚轮行 1/60）：精确增量以像素送达并直接相乘，
                            // 行增量则把一行当作 0.01666666 px 来换算。
                            let delta: f64 = if ev.delta.precise() {
                                f64::from(ev.delta.pixel_delta(px(1.0)).y) * 0.01666666
                            } else {
                                ev.delta.pixel_delta(px(0.01666666)).y.into()
                            };
                            cx.global::<PlaybackInterface>().set_volume(f64::clamp(
                                volume + delta,
                                0_f64,
                                1_f64,
                            ));
                        }),
                )
                .child(self.replaygain_button.clone())
                .child(
                    div()
                        .h(px(24.0))
                        .w(px(1.0))
                        .mt(px(3.0))
                        .mx(px(4.0))
                        .bg(theme.border_color),
                )
                .child(
                    sidebar_toggle_button("queue-button", MENU, queue_active)
                        .on_click(move |_, _, cx| {
                            show_queue.update(cx, |m, cx| {
                                *m = !*m;
                                crate::log_mem_event(if *m {
                                    "sidebar: queue show"
                                } else {
                                    "sidebar: queue hide"
                                });
                                cx.notify();
                            })
                        })
                        .tooltip(build_tooltip(tr!("QUEUE_TITLE"))),
                )
                .child(
                    sidebar_toggle_button("lyrics-button", MICROPHONE, lyrics_active)
                        .on_click(move |_, _, cx| {
                            show_lyrics.update(cx, |m, cx| {
                                *m = !*m;
                                crate::log_mem_event(if *m {
                                    "sidebar: lyrics show"
                                } else {
                                    "sidebar: lyrics hide"
                                });
                                cx.notify();
                            })
                        })
                        .tooltip(build_tooltip(tr!("LYRICS", "Lyrics"))),
                )
                .child(
                    sidebar_toggle_button("immersive-button", MAXIMIZE, immersive_active)
                        .on_click(|_, _, cx| {
                            let entering = !*cx.global::<Models>().immersive.read(cx);
                            crate::ui::immersive::set_immersive(entering, cx);
                        })
                        .tooltip(build_tooltip(tr!("IMMERSIVE_ENTER", "Immersive Mode"))),
                )
                .child(
                    sidebar_toggle_button(
                        "desktop-lyrics-button",
                        BUBBLE_TEXT,
                        desktop_lyrics_active,
                    )
                    .on_click(|_, _, cx| {
                        crate::ui::desktop_lyrics::toggle(cx);
                    })
                    .tooltip(build_tooltip(tr!("DESKTOP_LYRICS", "Desktop Lyrics"))),
                ),
        )
    }
}

// --- 音量 tooltip（由 ui/components/volume_tooltip.rs 并入）---

pub struct VolumeTooltip {
    volume: Entity<f64>,
}

impl VolumeTooltip {
    pub fn new(volume: Entity<f64>, cx: &mut Context<Self>) -> Self {
        observe_notify(cx, &volume);

        Self { volume }
    }
}

impl Render for VolumeTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let volume = *self.volume.read(cx);
        let percentage = (volume * 100.0).round() as i32;

        tooltip_container(theme).child(format!("{}%", percentage))
    }
}

pub fn build_volume_tooltip(
    volume: Entity<f64>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |_window, cx| cx.new(|cx| VolumeTooltip::new(volume.clone(), cx)).into()
}
