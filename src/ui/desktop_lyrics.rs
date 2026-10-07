//! 桌面歌词悬浮窗：置顶无边框透明 PopUp（`WS_EX_TOOLWINDOW | WS_EX_TOPMOST`，
//! 无任务栏入口），逐字卡拉OK复用共享 [`Lyrics`] 实体（不加载第二份歌词）。
//!
//! 生命周期：controls 的开关按钮 → [`toggle`]；悬浮窗悬停浮出的关闭按钮
//! → [`close_from_within`]（两者落盘同一设置位）。开关状态与拖动后的位
//! 置持久化在 interface settings（`desktop_lyrics` / `desktop_lyrics_pos`，
//! 后者在 view 的 on_release 统一落盘——Windows 的 start_window_move 是
//! 异步投递而非阻塞循环，拖动结果只能在销毁路径读 `last_bounds` 拿到）。
//! 主窗口关闭时经其 root 的 `on_release` 联动关闭本窗（避免只剩悬浮窗的
//! 孤儿会话）；悬浮窗自身被 Alt+F4 关闭时 view 的 on_release 清理 Global
//! 残留句柄并恢复广播档位，设置位保持 true，主窗开关再点即重开（自愈）。
//! 窗口存在期间强制位置广播 33ms 档（桌面歌词的主场景是主窗口失焦，
//! 250ms 档的逐字扫色会卡顿），关闭后由主窗口激活事件恢复常规档位。
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
/// 画布高度：两行 30px 歌词（超宽行折行后药丸 ~90-110px）完整放下。窗口
/// 常态全透明，画布加大零视觉成本——84px 时代超宽行折行的第二行会被窗
/// 口边界硬裁（无省略号）。药丸内容自适应尺寸并居中，短行观感不变。
const HEIGHT: f32 = 120.0;
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
    if cx.global::<DesktopLyricsState>().is_open() {
        close(cx);
        set_enabled(cx, false);
        sync_broadcast(cx);
    } else {
        open(cx);
    }
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
    let Some(handle) = open_window(cx, lyrics, pos) else {
        // 开窗失败（GDI/HWND 资源瞬态等）降级为未开启，不能 panic——
        // release 是 panic=abort，会让整个应用带着播放一起死。
        tracing::error!("desktop lyrics: open failed, degrading to disabled");
        set_enabled(cx, false);
        sync_broadcast(cx);
        return;
    };
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
    // 只升不降：降档统一由各关闭路径的 restore_broadcast_after_close 完成
    //（含 on_release 对 Alt+F4 的收口），此处不重复判档。
    if is_open(cx) {
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
) -> Option<WindowHandle<DesktopLyricsView>> {
    let bounds = match pos {
        Some([x, y]) => {
            let restored = Bounds {
                origin: point(px(x as f32), px(y as f32)),
                size: size(px(WIDTH), px(HEIGHT)),
            };
            // 恢复坐标钳制：副屏断开/投影切换后持久化原点可能落在所有
            // 屏幕之外——窗口不可达且无任何 UI 手段找回。与任一显示器
            // 无交集则回退默认底部居中位。
            let visible = cx
                .displays()
                .iter()
                .any(|display| display.bounds().intersects(&restored));
            if visible {
                restored
            } else {
                default_bounds(cx)
            }
        }
        None => default_bounds(cx),
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
        .map_err(|error| tracing::error!(?error, "desktop lyrics: failed to open overlay window"))
        .ok()?;
    // 窗口建起来立即摘掉 OS 层装饰，常态透明时才不会露出系统描边/投影；
    // 并登记 [frame] 探针的按窗口归因标签（悬浮窗重开覆盖同标签）。
    crate::register_frame_window_label(handle.into(), "desktop-lyrics");
    handle
        .update(cx, |_, window, _| disable_os_window_chrome(window))
        .ok();
    Some(handle)
}

/// 主屏默认位置：底部居中，任务栏上方留白。
fn default_bounds(cx: &App) -> Bounds<Pixels> {
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

/// 持久化窗口位置。调用点是 view 的 on_release（见 last_bounds 注释）：
/// Windows 的 start_window_move 是**异步投递** WM_NCLBUTTONDOWN 而非阻塞
/// 模态循环——按下时刻读 bounds 拿到的是拖动前坐标（旧实现的
/// "模态循环返回即拖动结束"前提不成立，位置持久化滞后一次交互）；
/// 渲染期缓存最后位置、销毁时落盘，是唯一覆盖全部销毁路径
/// （✕/toggle/Alt+F4/主窗关闭/进程退出）的时机。
fn persist_position(pos: [f64; 2], cx: &mut App) {
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
    /// 上次镜像时的 `Lyrics::parsed_generation`：歌词实体的逐帧动画
    /// notify（行强调 180ms + 跟随滚动，~60Hz 突发）不改变 parsed——
    /// 无守卫的话悬浮窗每次换行白重绘 10-30 帧（可见输出不变）。
    synced_generation: u64,
    /// 本窗口句柄（首次 render 捕获）：on_release 里与 Global 残留句柄
    /// 比对用。Alt+F4 直接 DestroyWindow，没有其它清理点能触达 Global。
    self_handle: Option<AnyWindowHandle>,
    /// 最近一帧渲染时的窗口位置（逻辑 px）：on_release 持久化的数据源
    ///（机制见 persist_position 注释）。
    last_bounds: Option<[f64; 2]>,
    /// 本次按压是否已进入 OS 移动循环：按住左键的首次 mouse-move 只触发
    /// 一次 start_window_move（NC 消息会被重分发回元素树，无守卫会自我
    /// 续链），下次按下复位。
    move_armed: bool,
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
            synced_generation: 0,
            self_handle: None,
            last_bounds: None,
            move_armed: false,
            hovered: false,
        };
        view.sync_from_lyrics(cx);

        // 销毁路径统一收口（Alt+F4 直接 DestroyWindow，没有别的钩子能
        // 触达）：持久化最后位置；若 Global 仍指向本窗口则清句柄并恢复
        // 广播档位——不清理的话 33ms 强制档对着已销毁窗口空转到会话结束，
        // 且主窗开关要点两次才重开。✕/toggle 路径已把 Global 清掉，此处
        // 句柄比对失配自然跳过（快速关开时旧视图的落盘值就是旧窗终位，
        // 不会覆盖新窗状态）。
        cx.on_release(|view, cx| {
            if let Some(pos) = view.last_bounds {
                persist_position(pos, cx);
            }
            let released_self = view
                .self_handle
                .zip(cx.global::<DesktopLyricsState>().window.as_ref())
                .is_some_and(|(mine, current)| mine == (*current).into());
            if released_self {
                cx.global_mut::<DesktopLyricsState>().window = None;
                restore_broadcast_after_close(cx);
            }
        })
        .detach();

        // 歌词实体变化 → 重镜像；实际重镜像与否由 sync_from_lyrics 内的
        // generation+accent 守卫决定（动画帧 notify 在此早退）。
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

    /// 镜像歌词实体的 parsed/accent。`parsed_lines()` 是引用计数克隆；
    /// generation+accent 双守卫——accent 由取色任务异步落地且**不 bump
    /// generation**（lyrics.rs resolve_accent），只比 generation 会漏接
    /// 主题色（immersive 同位观察者只比 generation 是对的：它自带取色
    /// 管线，不镜像 lyrics 的 accent）。首建时 parsed 必然伴随
    /// generation≥1（apply_loaded_lyrics 无条件 +1），不会漏初始镜像。
    fn sync_from_lyrics(&mut self, cx: &mut Context<Self>) {
        let lyrics = self.lyrics.read(cx);
        let generation = lyrics.parsed_generation();
        let accent = lyrics.accent();
        if generation == self.synced_generation && accent == self.accent {
            return;
        }
        self.synced_generation = generation;
        self.parsed = lyrics.parsed_lines();
        self.accent = accent;
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 渲染期缓存窗口位置与自身句柄：on_release 持久化/清理的唯一数据
        // 源（机制见 persist_position 与 self_handle 注释）。
        self.last_bounds = match window.window_bounds() {
            WindowBounds::Windowed(rect) => Some([rect.origin.x.to_f64(), rect.origin.y.to_f64()]),
            _ => self.last_bounds,
        };
        if self.self_handle.is_none() {
            self.self_handle = Some(window.window_handle());
        }
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
                    // 整面药丸即拖把。Windows 的 start_window_move 是异步
                    // 投递 WM_NCLBUTTONDOWN（非阻塞模态循环）：在 mouse-
                    // down 里触发的话 NC 消息会被重分发回元素树自我续链，
                    // 且按下时刻的坐标也读不到拖动结果——所以改为"按住
                    // 左键的首次移动"才触发（单击不再进移动循环），位置
                    // 持久化统一走 on_release 的 last_bounds。
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _, _| {
                            this.move_armed = false;
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, _| {
                        if event.pressed_button == Some(MouseButton::Left) && !this.move_armed {
                            this.move_armed = true;
                            window.start_window_move();
                        }
                    }))
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
