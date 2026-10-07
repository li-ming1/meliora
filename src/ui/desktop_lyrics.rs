//! 桌面歌词悬浮窗：置顶无边框透明 PopUp（`WS_EX_TOOLWINDOW | WS_EX_TOPMOST`，
//! 无任务栏入口），逐字卡拉OK复用共享 [`Lyrics`] 实体（不加载第二份歌词）。
//!
//! 生命周期：controls 的开关按钮 → [`toggle`]；悬浮窗悬停浮出的关闭按钮
//! → [`close_and_disable`]（两者落盘同一设置位）。开关状态与拖动后的位
//! 置持久化在 interface settings（`desktop_lyrics` / `desktop_lyrics_pos`）。
//! 主窗口关闭时经其 root 的 `on_release` 联动关闭本窗（避免只剩悬浮窗的
//! 孤儿会话）；悬浮窗自身被 Alt+F4 关闭时设置位保持 true，主窗开关再次
//! 点击即重开（自愈）。窗口存在期间强制位置广播 33ms 档（桌面歌词的主
//! 场景是主窗口失焦，250ms 档的逐字扫色会卡顿），关闭后由主窗口激活事
//! 件恢复常规档位。
//!
//! 可读性：常态纯透明与壁纸融为一体（用户约定），悬停/拖动时才浮出半
//! 透明黑药丸底 + 描边提示可抓取；OS 层（DWM）给 PopUp 自动叠加的描边/
//! 投影/圆角在 [`disable_os_window_chrome`] 里关闭，否则常态透明也会露
//! 出一圈系统像素。逐字扫色在暗底色与曲目主题色之间插值（与沉浸页同一
//! `word_progress`/`lerp_color` 管线）；无词行整行主题色；无歌词时显示
//! 占位。

use gpui::prelude::FluentBuilder;
use gpui::*;

use std::sync::Arc;

use crate::{
    playback::interface::PlaybackInterface,
    settings::{SettingsGlobal, save_settings},
    ui::{
        components::icons::{CROSS, icon},
        lyrics::{Lyrics, lerp_color, lrc::LrcLine, word_progress},
        models::PlaybackInfo,
        theme::Theme,
    },
};

const WIDTH: f32 = 720.0;
const HEIGHT: f32 = 84.0;
/// 主屏默认位置：底部居中，任务栏上方留白。
const BOTTOM_MARGIN: f32 = 108.0;

/// 悬浮窗句柄 + 共享歌词实体的宿主：controls 的开关、启动恢复、主窗口
/// 关闭联动都经这个 Global。
pub struct DesktopLyricsState {
    pub lyrics: Entity<Lyrics>,
    window: Option<WindowHandle<DesktopLyricsView>>,
    /// 主窗口句柄：悬浮窗关闭时按其激活态恢复位置广播档位（33ms 强制档
    /// 只在悬浮窗存在期间有效，关闭后不得滞留）。
    main_window: Option<AnyWindowHandle>,
}
impl Global for DesktopLyricsState {}

impl DesktopLyricsState {
    /// 主窗口构建时的初始注入；窗口句柄由开关流程（toggle/open/close）
    /// 独占管理。
    pub fn new(lyrics: Entity<Lyrics>, main_window: AnyWindowHandle) -> Self {
        Self {
            lyrics,
            window: None,
            main_window: Some(main_window),
        }
    }

    fn is_open(&self) -> bool {
        self.window.is_some()
    }
}

pub fn toggle(cx: &mut App) {
    let opening = !cx.global::<DesktopLyricsState>().is_open();
    if opening {
        let lyrics = cx.global::<DesktopLyricsState>().lyrics.clone();
        let pos = cx
            .global::<SettingsGlobal>()
            .model
            .read(cx)
            .interface
            .desktop_lyrics_pos;
        let handle = open_window(cx, lyrics, pos);
        cx.global_mut::<DesktopLyricsState>().window = Some(handle);
    } else {
        close(cx);
    }
    set_enabled(cx, opening);
    sync_broadcast(cx);
}

/// 打开悬浮窗（设置位为 true 时的启动恢复与 `toggle` 共用）。
pub fn open(cx: &mut App) {
    if cx.global::<DesktopLyricsState>().is_open() {
        return;
    }
    let lyrics = cx.global::<DesktopLyricsState>().lyrics.clone();
    let pos = cx
        .global::<SettingsGlobal>()
        .model
        .read(cx)
        .interface
        .desktop_lyrics_pos;
    let handle = open_window(cx, lyrics, pos);
    cx.global_mut::<DesktopLyricsState>().window = Some(handle);
    set_enabled(cx, true);
    sync_broadcast(cx);
}

pub fn close(cx: &mut App) {
    if let Some(handle) = cx.global_mut::<DesktopLyricsState>().window.take() {
        handle
            .update(cx, |_, window, _| window.remove_window())
            .ok();
    }
    restore_broadcast_after_close(cx);
}

/// 悬浮窗自身的关闭按钮路径：在窗口自己的事件分发内，只能直接
/// `window.remove_window()`（分发结束后 gpui 按标志销毁窗口）。此时本
/// 窗口已从 `cx.windows` 取出，对全局句柄再走一次 `handle.update` 会撞
/// 上空槽静默失败（gpui "window not found"），且全局态会与实际窗口脱
/// 钩——2026-10-07 ✕ 按钮无反应、播放栏开关看似失效的根因。
fn close_from_within(window: &mut Window, cx: &mut App) {
    window.remove_window();
    cx.global_mut::<DesktopLyricsState>().window = None;
    restore_broadcast_after_close(cx);
    set_enabled(cx, false);
}

/// 33ms 强制档随悬浮窗关闭立即撤销：按主窗口当前激活态恢复双档制。
/// 不撤销的话，主窗失焦期间广播会滞留在 30fps（纯浪费的重绘泵）。
fn restore_broadcast_after_close(cx: &mut App) {
    let main_active = cx
        .global::<DesktopLyricsState>()
        .main_window
        .and_then(|handle| {
            handle
                .update(cx, |_view, window, _cx| window.is_window_active())
                .ok()
        })
        .unwrap_or(false);
    cx.global::<PlaybackInterface>()
        .set_position_broadcast_active(main_active);
}

/// 位置广播档位：悬浮窗存在即 33ms（主窗失焦场景的逐字扫色需要）；
/// 关闭后由主窗口激活事件（app.rs 的 activation 观察者）恢复常规双档。
/// app.rs 的档位写入点以 [`is_open`] 为 OR 条件，两条写入路径不再互踩。
pub fn is_open(cx: &App) -> bool {
    cx.global::<DesktopLyricsState>().is_open()
}

fn sync_broadcast(cx: &mut App) {
    let active = is_open(cx) || {
        // 悬浮窗关闭时交还主窗口激活态决定；这里无法拿到主窗口句柄，
        // 交由 app.rs 的 activation 观察者在下一次事件中校正。
        false
    };
    if active {
        cx.global::<PlaybackInterface>()
            .set_position_broadcast_active(true);
    }
}

fn set_enabled(cx: &mut App, on: bool) {
    let model = cx.global::<SettingsGlobal>().model.clone();
    model.update(cx, |s, cx| {
        s.interface.desktop_lyrics = on;
        save_settings(cx, s);
        cx.notify();
    });
}

fn open_window(
    cx: &mut App,
    lyrics: Entity<Lyrics>,
    pos: Option<[f64; 2]>,
) -> WindowHandle<DesktopLyricsView> {
    let bounds = match pos {
        Some([x, y]) => Bounds {
            origin: point(px(x as f32), px(y as f32)),
            size: size(px(WIDTH), px(HEIGHT)),
        },
        None => {
            let screen = cx.primary_display().map(|d| d.bounds());
            match screen {
                Some(b) => Bounds {
                    origin: point(
                        px((b.size.width.to_f64() - WIDTH as f64) as f32),
                        px((b.size.height.to_f64() - HEIGHT as f64 - BOTTOM_MARGIN as f64) as f32),
                    ),
                    size: size(px(WIDTH), px(HEIGHT)),
                },
                None => Bounds {
                    origin: point(px(120.0), px(600.0)),
                    size: size(px(WIDTH), px(HEIGHT)),
                },
            }
        }
    };
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_background: WindowBackgroundAppearance::Transparent,
        titlebar: None,
        focus: false,
        show: true,
        kind: WindowKind::PopUp,
        app_id: Some("meliora-desktop-lyrics".to_string()),
        ..Default::default()
    };
    let handle = cx
        .open_window(options, |_window, cx| {
            cx.new(|cx| DesktopLyricsView::new(lyrics, cx))
        })
        .unwrap_or_else(|error| {
            tracing::error!(?error, "desktop lyrics: failed to open overlay window");
            panic!("desktop lyrics window");
        });
    // 窗口建起来立即摘掉 OS 层装饰，常态透明时才不会露出系统描边/投影。
    handle
        .update(cx, |_, window, _| disable_os_window_chrome(window))
        .ok();
    handle
}

/// 关闭 Win32/DWM 给 PopUp 窗口自动套上的装饰：非客户区描边、窗口投影
/// 与圆角裁剪。窗口内容全透明时这些系统层像素照样合成上屏（亮色壁纸尤
/// 其突兀），全部关掉后屏幕上只剩 gpui 实际绘制的内容。
#[cfg(windows)]
fn disable_os_window_chrome(window: &mut Window) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::Graphics::Dwm::{
        DWMNCRP_DISABLED, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_NCRENDERING_POLICY,
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DwmSetWindowAttribute,
    };

    let Ok(handle) = window.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };
    let hwnd = HWND(win32.hwnd.get() as *mut _);
    unsafe {
        // 老式 NC 渲染：系统边框 + 投影。
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_NCRENDERING_POLICY,
            &DWMNCRP_DISABLED as *const _ as *const _,
            std::mem::size_of_val(&DWMNCRP_DISABLED) as u32,
        );
        // Win11 专有的 1px 描边，上一条管不到它；COLOR_NONE = 不画。
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &DWMWA_COLOR_NONE as *const _ as *const _,
            std::mem::size_of_val(&DWMWA_COLOR_NONE) as u32,
        );
        // 圆角裁剪一并关掉（透明窗口下不可见，防御性）。
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &DWMWCP_DONOTROUND as *const _ as *const _,
            std::mem::size_of_val(&DWMWCP_DONOTROUND) as u32,
        );
    }
}

#[cfg(not(windows))]
fn disable_os_window_chrome(_window: &mut Window) {}

/// 拖动结束后持久化窗口位置（`start_window_move` 的 OS 模态循环返回即
/// 拖动结束；屏幕坐标为逻辑 px，跨 DPI 变更可能漂移，可接受）。
fn save_position(window: &mut Window, cx: &mut App) {
    let pos = match window.window_bounds() {
        WindowBounds::Windowed(rect) => [rect.origin.x.to_f64(), rect.origin.y.to_f64()],
        _ => return,
    };
    let model = cx.global::<SettingsGlobal>().model.clone();
    model.update(cx, |s, cx| {
        s.interface.desktop_lyrics_pos = Some(pos);
        save_settings(cx, s);
        cx.notify();
    });
}

pub struct DesktopLyricsView {
    lyrics: Entity<Lyrics>,
    parsed: Option<Arc<Vec<LrcLine>>>,
    current_line: Option<usize>,
    accent: Option<Rgba>,
    /// 鼠标悬停态：驱动药丸底/描边与关闭按钮的出现（常态全透明）。
    /// 用 Rust 状态而非 `.hover()` 样式 refinement，因为关闭按钮要整体
    /// 按此条件渲染（未渲染的元素没有 hitbox，不会隐形吃点击）。
    hovered: bool,
}

impl DesktopLyricsView {
    fn new(lyrics: Entity<Lyrics>, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            lyrics: lyrics.clone(),
            parsed: None,
            current_line: None,
            accent: None,
            hovered: false,
        };
        view.sync_from_lyrics(cx);

        // 歌词实体变化（换歌/翻译落地/主题色提取完成）→ 重镜像。
        cx.observe(&lyrics, |this, _lyrics, cx| this.sync_from_lyrics(cx))
            .detach();
        // 位置推进 → 行切换与逐字扫色（仅在实际有变化时 notify）。
        let position = cx.global::<PlaybackInfo>().position.clone();
        cx.observe(&position, |this, pos, cx| {
            let pos_ms = *pos.read(cx);
            this.track_line(pos_ms, cx);
        })
        .detach();
        view
    }

    /// 与沉浸页同一镜像协议：`parsed_lines()` 是引用计数克隆，
    /// `parsed_generation` 的增量语义由观察者代替（observe 本身按变更触发）。
    fn sync_from_lyrics(&mut self, cx: &mut Context<Self>) {
        let lyrics = self.lyrics.read(cx);
        self.parsed = lyrics.parsed_lines();
        self.accent = lyrics.accent();
        let pos_ms = *cx.global::<PlaybackInfo>().position.read(cx);
        self.current_line = self.line_at(pos_ms);
        cx.notify();
    }

    fn line_at(&self, pos_ms: u64) -> Option<usize> {
        let parsed = self.parsed.as_ref()?;
        let idx = parsed.partition_point(|line| line.time_ms <= pos_ms);
        if idx == 0 { None } else { Some(idx - 1) }
    }

    /// 行切换或当前行仍在逐字飞行中才重绘；尾音窗口与上一帧输出相同，
    /// 不再 notify。
    fn track_line(&mut self, pos_ms: u64, cx: &mut Context<Self>) {
        let new_line = self.line_at(pos_ms);
        let sweeping = new_line.is_some_and(|i| {
            self.parsed
                .as_ref()
                .and_then(|p| p.get(i))
                .is_some_and(|line| {
                    line.words
                        .last()
                        .is_some_and(|word| pos_ms < word.time_ms + word.duration_ms)
                })
        });
        if sweeping || new_line != self.current_line {
            self.current_line = new_line;
            cx.notify();
        }
    }

    fn current_line(&self) -> Option<&LrcLine> {
        let idx = self.current_line?;
        self.parsed.as_ref()?.get(idx)
    }
}

impl Render for DesktopLyricsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let pos_ms = *cx.global::<PlaybackInfo>().position.read(cx);
        let accent = self.accent.unwrap_or(theme.text);
        // 未唱底色：主题文本色向次级文本色压暗（与沉浸页邻居行同一档）。
        let unsung = lerp_color(theme.text, theme.text_secondary, 0.4);

        let content: AnyElement = match self.current_line() {
            Some(line) if !line.words.is_empty() => {
                let mut row = div().flex().flex_wrap().items_baseline();
                for word in &line.words {
                    let progress = word_progress(word, pos_ms);
                    let color = if progress > 0.0 {
                        lerp_color(unsung, accent, progress.min(1.0))
                    } else {
                        unsung
                    };
                    row = row.child(
                        div()
                            .text_size(px(30.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(color)
                            .child(word.text.clone()),
                    );
                }
                row.into_any_element()
            }
            Some(line) => div()
                .text_size(px(30.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(accent)
                .child(line.text.clone())
                .into_any_element(),
            None => div()
                .text_size(px(24.0))
                .text_color(unsung)
                .child("♪ 暂无歌词")
                .into_any_element(),
        };

        let set_hovered = cx.listener(|this, hovered: &bool, _window, cx| {
            if this.hovered != *hovered {
                this.hovered = *hovered;
                cx.notify();
            }
        });

        let close_button = div()
            .id("desktop-lyrics-close")
            // stop_propagation：不拦的话事件冒泡到药丸，点击会先进拖拽模
            // 态循环。窗口移除走分发结束后的 trail，无需 defer。
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                cx.stop_propagation();
                close_from_within(window, cx);
            })
            .cursor_pointer()
            .absolute()
            .top(px(2.0))
            .right(px(6.0))
            .size(px(16.0))
            .rounded_full()
            .bg(white().opacity(0.16))
            .flex()
            .items_center()
            .justify_center()
            .child(icon(CROSS).size(px(10.0)).text_color(white().opacity(0.9)));

        div()
            .size_full()
            .bg(transparent_black())
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("desktop-lyrics-pill")
                    // 整面药丸即拖把：按下交给 OS 移动模态循环，返回即拖动
                    // 结束，随手持久化位置。
                    .on_mouse_down(MouseButton::Left, |_, window, cx| {
                        window.start_window_move();
                        save_position(window, cx);
                    })
                    .on_hover(set_hovered)
                    .cursor_default()
                    .relative()
                    .rounded(px(16.0))
                    // 常态全透明与壁纸融为一体；悬停（含拖动全程——进入拖
                    // 动前鼠标必然已悬停，且 OS 移动模态循环期间 hover 态
                    // 不会被 mouse-move 清掉）才浮出药丸底 + 描边 + 关闭按
                    // 钮。边框常驻（透明色）避免悬停时布局位移。
                    .border_1()
                    .border_color(if self.hovered {
                        white().opacity(0.25)
                    } else {
                        transparent_black()
                    })
                    .bg(if self.hovered {
                        black().opacity(0.42)
                    } else {
                        transparent_black()
                    })
                    .px(px(24.0))
                    .py(px(10.0))
                    .max_w_full()
                    .overflow_hidden()
                    .child(content)
                    .when(self.hovered, |pill| pill.child(close_button)),
            )
    }
}
