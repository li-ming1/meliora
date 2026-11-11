//! 全屏沉浸聆听视图，版式仿高保真歌词演示：封面作为清晰裁剪带背景铺满整屏
//! （见 `managed_image::backdrop_pipeline::backdrop_sharp_fill` — 按窗口纵横比
//! 居中裁剪、中值去噪、Lanczos 重采样到精确设备像素尺寸并 unsharp 补锐；酷狗
//! 阶梯现以 4096 打头，让缩采样像圆盘小封面那样平均掉噪声；一道竖向 + 一道
//! 横向渐变划出阅读区），左侧静止的圆盘尺寸圆形封面（「黑胶」已不再旋转——
//! 见下面的动画纪律）压在曲名之上，配一张磨砂玻璃播放卡（可拖动进度、走带、
//! 音量），右半是歌词栏，带逐字卡拉OK计时与点击定位。
//!
//! 歌词从侧栏的 [`Lyrics`] 实体镜像而来而不是二次加载：该实体持有
//! sidecar/DB/在线抓取管线（含酷狗）、它的有界缓存与扫描重置逻辑。
//! 镜像有代际守卫，重复 notify 绝不重复克隆。
//!
//! 曲目呈现（封面、名称、取色）以队列位置为键——「正在播什么」的唯一真相源，
//! 与 InfoSection 一致——并在 `SongChanged` 与 `QueuePositionChanged` 上都重新
//! 解析，因为前者到达时位置还指向上一个曲目。
//!
//! 动画纪律（见 `GPUI_HARDCORE_PERFORMANCE.md` 与 `scroll_follow.rs` 里的
//! 2026-09-27 图集泄漏教训）：
//! - 每一帧要么数据驱动（位置广播）、要么过渡驱动（背景 crossfade），一旦没
//!   有东西在动就立刻停掉帧循环；
//! - 歌词字形几何绝不插值：字号取自离散集合，只有颜色做 lerp（含卡拉OK逐字色）；
//! - 「黑胶」现在只是圆盘尺寸的圆形封面图：深色转盘、纹路环与光泽弧在
//!   2026-09-28 的反馈里全部去掉（旋转光泽弧经 `Svg::with_transformation` 在
//!   DirectX 上会渲染成位移/超尺寸，而露出的转盘环看着像给封面加了道无意义
//!   的边框）；
//! - 背景是它自己的一次解码，按窗口精确设备像素尺寸渲染成清晰裁剪带（由背景
//!   LRU 持有，切歌时经孤儿瓦片回收漏斗回收；交换是一次 400ms crossfade——
//!   淡出中的旧图在其下继续绘制直到淡出结束）。清晰方案在 2026-09-29 与用户
//!   的四轮比选中胜出：模糊填充（三档强度、带与不带亮度压暗）逐轮被否——
//!   指令是背景清晰度须与圆盘小封面齐平，故管线为居中裁剪 → 中值去噪
//!   （干掉 1:1 级重采样会直接上屏的 JPEG 块边）→ Lanczos → 阈值 unsharp，
//!   酷狗阶梯以 4096 打头，有该档的封面缩采样约 0.47×（圆盘级平均）。
//!   在线封面走阶梯（4096 → 2048 → 1280 → 480 → 原图），最大变体缺失时优雅
//!   降级，而不是静默把 256px 缩略图铺满整屏。

use std::{
    cell::Cell,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use cntp_i18n::tr;
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, ObjectFit, ParentElement, PathBuilder, Render, Rgba,
    ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Subscription, Task, Window,
    canvas, div, linear_color_stop, linear_gradient, point, px, relative,
};

use crate::sync::RecoveredRwLock;
use crate::{
    playback::{interface::PlaybackInterface, thread::PlaybackState},
    ui::{
        app::Pool,
        components::{
            icons::{
                GRID, HEART_FILLED, MINIMIZE, NEXT_TRACK, PAUSE, PLAY, PREV_TRACK, VOLUME, icon,
            },
            managed_image::{
                ImageCacheMode, ManagedImageKey, RENDER_CACHE_PRESSURE_KEEP,
                backdrop_cache_contains, backdrop_cache_shrink, backdrop_decode_allowed,
                managed_image, memory_pressure, render_cache_shrink,
            },
            slider::{SLIDER_CHANGE_INTERVAL, slider},
            tooltip::build_tooltip,
        },
        equalizer::spectrum::{SpectrumData, SpectrumState, ensure_analyzer},
        lyrics::{Lyrics, lerp_color, word_progress},
        models::{CurrentTrack, Models, PlaybackInfo, Queue},
        queue_metadata,
        scroll_follow::ease_out_cubic,
        theme::Theme,
        util::{CachedTimeText, reduced_motion_enabled, seek_to_fraction},
    },
};

/// 圆盘标签封面的解码尺寸——小图，走渲染缓存。
const LABEL_THUMB_PX: u32 = 512;
/// 大图回收队列未放行时的退避重试间隔：重试只是一次廉价的队列计数检查，
/// 期间旧背景保持绘制，不出现黑底。
const BACKDROP_RETRY: Duration = Duration::from_millis(500);
/// 背景切换 crossfade 时长。必须严格小于 managed_image 的
/// RECLAIM_DELAY_LARGE（1s）：淡出期间旧背景元素仍在绘制，其图集瓦片
/// 靠那条短年龄门兜底（门到时淡出早已结束、旧元素已卸载）。
const BACKDROP_FADE: Duration = Duration::from_millis(400);

/// 进入或离开沉浸模式：翻转 [`Models::immersive`] 并同步切换主窗口的 OS 全屏
/// 状态。所有入口（快捷键、侧栏开关、信息区双击、退出按钮）都汇入这里。
pub fn set_immersive(entering: bool, cx: &mut App) {
    let immersive = cx.global::<Models>().immersive.clone();
    if *immersive.read(cx) == entering {
        return;
    }
    immersive.write(cx, entering);
    // 这里运行在点击/按键处理器的窗口更新上下文里，同步调 window.update 是
    // 重入、会被静默吞掉（进入分支的日志从不出现）——推到本轮 effect 之后，
    // 与 focus_main_window 的 defer 模式一致。
    cx.defer(move |cx| crate::ui::app::set_main_window_fullscreen(entering, cx));
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum ImmersiveStageMode {
    #[default]
    PosterWall,
    KineticLyrics,
}

/// crossfade 槽位内容：`(封面键, 元素 id 代数, 该图解码时的设备像素目标尺寸)`。
type BackdropLayer = (ManagedImageKey, u64, (u32, u32));

/// 海报墙列数与卡片间距（逻辑像素）。
const POSTER_COLUMNS: usize = 6;
const POSTER_GAP: f32 = 14.0;
/// 海报墙左右内边距合计（`px(16)` × 2）。
const POSTER_PADDING: f32 = 32.0;
/// 海报墙上下内边距合计（`pt(20)` + `pb(24)`）：滚动内容高 = 最高列 + 它。
const POSTER_PADDING_V: f32 = 44.0;
/// 海报墙视口外上下各多实体化的缓冲高度：滚动事件同步 notify 本视图按新
/// offset 重算窗口，缓冲盖住快速滚动的帧间空档。
const POSTER_OVERSCAN: f32 = 640.0;

/// 海报卡片的解码尺寸：按 6 列布局的实际设备像素宽度取整到 64 的倍数，
/// 夹在 [256, 512]。固定 512 在 1080p@125% 下把 ~300 设备像素的卡片解码成
/// 512²（1MB/张，图集每页只放 4 张）；按显示尺寸解码后同样 1:1 上屏，
/// 每张像素少 ~60%，50 首队列的堆与图集页各降一半。512 仍是质量上限
/// （4K/高 DPR 与旧行为一致），256 是离线缩略图下限。
fn poster_thumb_size(viewport_width_device: f32) -> u32 {
    let card_width = ((viewport_width_device - POSTER_PADDING)
        - POSTER_GAP * (POSTER_COLUMNS - 1) as f32)
        / POSTER_COLUMNS as f32;
    (card_width.max(1.0).ceil() as u32)
        .next_multiple_of(64)
        .clamp(256, 512)
}

/// 海报墙单张卡片的高度。正在播放的高光大海报固定 340，其余按
/// `(列内序号 + 列号)` 在四档里轮换（瀑布流的错落感）。
fn poster_card_height(idx: usize, col_idx: usize, is_playing: bool) -> f32 {
    if is_playing {
        return 340.0;
    }
    match (idx + col_idx) % 4 {
        0 => 210.0,
        1 => 270.0,
        2 => 180.0,
        _ => 240.0,
    }
}

/// 计算海报墙某一列在当前滚动窗口内要实体化的卡片位置 `(队列下标, 顶部 y,
/// 高度)`，同时返回该列总高度（含间距）。窗口外的卡片不构建元素，因此不会
/// 被 `ManagedImage` 解码；但高度照常累加，滚动范围与整队列渲染完全一致。
fn poster_column_layout(
    queue_len: usize,
    col_idx: usize,
    current_pos: usize,
    window_top: f32,
    window_bottom: f32,
) -> (Vec<(usize, f32, f32)>, f32) {
    let mut slots = Vec::new();
    let mut y = 0.0f32;
    for idx in (col_idx..queue_len).step_by(POSTER_COLUMNS) {
        let h = poster_card_height(idx, col_idx, idx == current_pos);
        if y + h >= window_top && y <= window_bottom {
            slots.push((idx, y, h));
        }
        y += h + POSTER_GAP;
    }
    (slots, (y - POSTER_GAP).max(0.0))
}

/// 某一列的总高度（含卡片间距、不含容器上下内边距）。与
/// [`poster_column_layout`] 返回的第二元同值，但只做累加、不收集槽位，
/// 供渲染时先算出滚动内容高。
fn poster_column_height(queue_len: usize, col_idx: usize, current_pos: usize) -> f32 {
    let mut y = 0.0f32;
    for idx in (col_idx..queue_len).step_by(POSTER_COLUMNS) {
        y += poster_card_height(idx, col_idx, idx == current_pos) + POSTER_GAP;
    }
    (y - POSTER_GAP).max(0.0)
}

/// 海报墙滚动内容高 = 最高列 + 上下内边距，即 gpui 用来算滚动范围的那个值。
fn poster_content_height(queue_len: usize, current_pos: usize) -> f32 {
    (0..POSTER_COLUMNS)
        .map(|col| poster_column_height(queue_len, col, current_pos))
        .fold(0.0f32, f32::max)
        + POSTER_PADDING_V
}

/// 把滚动句柄里的原始偏移折算成内容坐标下的可见顶部，并钳到
/// `[0, 内容高 - 视口高]`。
///
/// 必须自己钳：`render` 早于 layout，gpui 的钳位（`Div::layout_scroll_offset`，
/// div.rs 里的 `scroll_offset.y.clamp(-scroll_max, 0)`）要到 layout 才发生，
/// 而句柄里的原始偏移可以越界——滚到底后继续滚（句柄继续累加）或滚过之后窗口
/// 变高（全屏化把内容高/视口高的差压小）都会留下它。用未钳位的偏移算窗口，
/// 那一帧算出的可见区比实际绘制位置更靠下，视口顶部就露出一条没被实体化的
/// 空白带；滚动停下后若没有新的 notify，空白带会一直留在屏幕上
/// （2026-11-10 实测：越界 ~865px，顶部落空带 ~225px，且恰好只吞掉首卡
/// 高 180 的奇数列、留下高 210 的偶数列）。UniformGrid 在 prepaint 里做的是
/// 同一件事（uniform_grid.rs「把 offset 保持在 [content_height - viewport, 0]
/// 内」）。
fn poster_scroll_top(handle_offset_y: f32, content_height: f32, viewport_height: f32) -> f32 {
    let max_scroll = (content_height - viewport_height).max(0.0);
    (-handle_offset_y).max(0.0).min(max_scroll)
}

/// render↔paint 滚动偏移探针（2026-11-11 诊断）。render 用句柄里的原始偏移
/// 算窗口化，而 gpui 要到 layout 阶段才把偏移钳进 `[0, 内容高-视口]` 并据此
/// 绘制——两侧可能错位，且滚动停止后没有后续帧自愈，空洞就留在屏幕上。
/// 探针随海报墙内容放一个 canvas：它的 prepaint 在容器的钳位**之后**执行，
/// 读到的句柄偏移就是本帧 paint 实际使用的值。与 render 用的值比对：
/// 错位时打 `[poster]` 日志并 notify 一帧按真实偏移重算（自愈）；
/// 数值不变后静默，防止 max 口径分歧时陷入每帧 notify 死循环。
struct PosterScrollProbe {
    /// 本帧 render 用于窗口计算的 scroll_top。
    render_top: Cell<f32>,
    /// 上一次自愈时的 `(render_top 桶, 偏差桶)`；相同则不再 notify。
    healed: Cell<(i32, i32)>,
    /// 上一次日志的桶，抑制重复行。
    logged: Cell<(i32, i32)>,
}

/// 自愈判定：偏差超过 1px 且 `(render_top 桶, 偏差桶)` 与上次自愈不同才给
/// 出新桶。桶宽 48px——同一数值的重复偏差不重复自愈，偏差演进时逐帧跟进。
fn poster_scroll_should_heal(
    render_top: f32,
    paint_top: f32,
    last_healed: (i32, i32),
) -> Option<(i32, i32)> {
    let bucket = |v: f32| (v / 48.0) as i32;
    let diff = paint_top - render_top;
    let key = (bucket(render_top), bucket(diff));
    (diff.abs() > 1.0 && key != last_healed).then_some(key)
}

pub struct ImmersiveView {
    focus_handle: FocusHandle,
    /// 镜像 `Models::immersive`；守每次观察者的 `notify`。
    active: bool,
    /// 沉浸模式下的舞台展示形态：Classic（经典双栏）、PosterWall（瀑布流海报墙）、KineticLyrics（随意跳动巨幕歌词）。
    stage_mode: ImmersiveStageMode,
    poster_wall_scroll: ScrollHandle,
    /// render↔paint 滚动偏移探针（见 [`PosterScrollProbe`]）。
    poster_scroll_probe: Rc<PosterScrollProbe>,
    /// 右侧中控卡片是否折叠收起。
    /// 鼠标是否靠近/悬停在右侧中控区（实现靠边自动显现、移开自动消失）
    console_hovered: bool,

    position: Entity<u64>,
    duration: Entity<u64>,
    sample_rate: Entity<u32>,
    spectrum_data: Option<Entity<SpectrumData>>,
    spectrum_viewers: Option<Arc<std::sync::atomic::AtomicUsize>>,
    /// 本视图当前是否持有一个频谱观察者席位（want 状态记账）：want =
    /// 激活且处于 KineticLyrics 舞台。SpectrumState.viewers 是与 EQ 视图
    /// 共享的全局原子，各视图独立差值加减——席位增减只能按翻转
    /// fetch_add/fetch_sub，绝不能绝对置位，翻转前的旧值靠本字段还原。
    spectrum_viewers_registered: bool,
    volume: Entity<f64>,
    /// 全屏底栏整秒标签缓存（`util::CachedTimeText`，与 controls.rs 的
    /// Scrubber 同一套机制）。
    stage_time_labels: CachedTimeText<(SharedString, SharedString)>,
    /// 音量百分比文本：只在整数百分比跨档时重建。
    cached_volume_pct: i64,
    cached_volume_text: SharedString,
    /// 频谱柱元素的复用缓冲：每帧 clear 后保留容量。
    spectrum_bar_buffer: Vec<AnyElement>,
    playback_state: Entity<PlaybackState>,
    /// 镜像 `*playback_state == Playing`，由 playback_state 观察者维护，
    /// 是暂停态扫色泵停转（needs_animation_frame 第三子句）的判据。初值
    /// false 靠 StateChanged 事后补正：启动次序下起播事件先经 playback
    /// bridge 的通道排队，主窗口（含本视图）构造完成后才 drain（app.rs
    /// PlaybackThread::start → start_broadcast → ensure_main_window 全在
    /// 同一同步流程里），观察者必能接上。
    playing: bool,
    current_track: Entity<Option<CurrentTrack>>,
    queue: Entity<Queue>,
    /// 侧栏的歌词模型——共享它，在线抓取才能复用。
    lyrics: Entity<Lyrics>,
    /// 上一次镜像进 `parsed` 的 `Lyrics::parsed_generation`。
    synced_lyrics_generation: u64,

    // 曲目呈现，在切歌/队列位置变化时重新解析。
    image_key: Option<ManagedImageKey>,
    image_element_key: u64,
    /// 当前渲染中的背景图键：解码完成时才晋升，与 `image_key`（封面/
    /// 取色的即时来源）解耦。晋升前旧背景一直绘制——没有黑底间隙。
    backdrop_key: Option<ManagedImageKey>,
    /// crossfade 双槽位：两个带显式 id 的固定图层（index 0/1）交替充当
    /// "当前背景"与"淡出层"。晋升时新图进入空闲槽，旧图原地不动——
    /// 其 ManagedImage 元素 id 与祖先路径全程不变，keyed state 不重建、
    /// 不重解码，淡出结束才清槽并把瓦片推进回收漏斗。第三元是该图解码时
    /// 的设备像素目标尺寸：图层按自己的尺寸绘制，不随 `backdrop_target`
    /// 漂移。
    backdrop_layers: [Option<BackdropLayer>; 2],
    /// 当前背景所在的槽位 index（0/1），每次晋升翻转。
    backdrop_active: usize,
    /// 背景元素 id 代数：仅在晋升时递增（新图进入槽位时取下一代号）。
    backdrop_gen: u64,
    /// 背景重采样目标 = 背景元素的设备像素尺寸（viewport × DPR，render
    /// 时更新）。纹素与屏幕逐像素对齐后 GPU 1:1 采样，消除非整数比例
    /// 双线性采样的半像素模糊（2026-09-29 取证定案的质量损失根源）。
    backdrop_target: Option<(u32, u32)>,
    /// `activate` 早于首个沉浸页 render：目标尺寸未知时先记下，render
    /// 拿到窗口尺寸后补一次布防。
    backdrop_pending_arm: bool,
    /// crossfade 起点；`None` 表示没有过渡在进行。
    backdrop_fade_started: Option<Instant>,
    /// 背景解码任务及其布防键（未 detach：切歌/退出即取消；从未显示的
    /// 背景不进回收漏斗，零队列成本）。键随任务原子绑定，供同键在途
    /// 守卫判别"这条管线是为哪首歌布防的"。
    backdrop_task: Option<(ManagedImageKey, Task<()>)>,
    /// 已预取的下一首封面记忆 `(键, 目标宽, 目标高)`：同键同尺寸只预取
    /// 一次，重复 resolve / activate 直接跳过。
    prefetched_art: Option<(ManagedImageKey, u32, u32)>,
    /// 在途预取任务句柄：退出沉浸页时 abort，防预取完成后把下一首的
    /// 背景大图写回刚收缩过的缓存（"退出界面即归还"契约的补口），也防
    /// 重进时与补预取产生同键双解码。不 detach 的语义与 `backdrop_task`
    /// 一致——置空即取消。
    prefetch_task: Option<tokio::task::JoinHandle<()>>,
    track_name: Option<SharedString>,
    artist_name: Option<SharedString>,
    meta_subscription: Option<Subscription>,
    /// 上一次解析的 `(队列位置, 当前曲目路径)`；每次切歌两个触发器都会响，
    /// 第二个必须是空操作。
    resolved_signature: Option<(usize, Option<PathBuf>)>,

    // 歌词，从共享的 `Lyrics` 实体镜像而来。
    parsed: Option<Arc<Vec<crate::ui::lyrics::lrc::LrcLine>>>,
    current_line: Option<usize>,

    frame_scheduled: bool,
}

impl Focusable for ImmersiveView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ImmersiveView {
    pub fn new(cx: &mut App, lyrics: Entity<Lyrics>) -> Entity<Self> {
        cx.new(|cx| {
            let info = cx.global::<PlaybackInfo>().clone();
            let immersive_flag = cx.global::<Models>().immersive.clone();
            let queue = cx.global::<Models>().queue.clone();

            // 全屏切换本身住在 `set_immersive`（它需要窗口句柄）；这个观察者
            // 只把旗标镜像进视图自己的状态。
            cx.observe(&immersive_flag, |this: &mut ImmersiveView, flag, cx| {
                this.set_active(*flag.read(cx), cx);
            })
            .detach();

            cx.observe(&info.position, |this, pos, cx| {
                if !this.active {
                    return;
                }
                let pos_ms = *pos.read(cx);
                let mut needs_repaint = this.track_lyric_line(pos_ms);
                // 卡拉OK逐字色随 tick 扫动——但只在 KineticLyrics 舞台且当前行
                // 最后一个字仍在途时。舞台门控必须保留：parse_lyrics 会为所有
                // LRC 行拟合逐字 words，任何带歌词曲目演唱期间该检查几乎恒真，
                // 而 PosterWall 的 NOW PLAYING 卡只消费行文本（行粒度更新由
                // track_lyric_line 覆盖），逐字 tick 不改变其任何像素，30Hz
                // notify 只会徒劳触发每帧全队列克隆的海报墙重建。进度条住在
                // 自己的实体里并自行观察位置。
                if !needs_repaint
                    && this.stage_mode == ImmersiveStageMode::KineticLyrics
                    && this.current_line.is_some_and(|i| {
                        this.parsed
                            .as_ref()
                            .and_then(|parsed| parsed.get(i))
                            .and_then(|line| line.words.last())
                            .is_some_and(|last| pos_ms < last.time_ms + last.duration_ms)
                    })
                {
                    needs_repaint = true;
                }
                // 暂停态兜底：本观察者只在位置真实变化（update_ts 对时间戳
                // 不变早退；PositionChanged 仅真实变化或 seek force）时触发，
                // 暂停下的 seek 恒得单帧重绘——泵停转后行内 seek 落在末字
                // 窗口之外（上面逐字判定为否）时再无其它重绘来源，不补就会
                // 留下旧扫色。播放态不进此分支，判定仍由上面各条款负责。
                if !needs_repaint && !this.playing {
                    needs_repaint = true;
                }
                if needs_repaint {
                    cx.notify();
                }
            })
            .detach();

            cx.observe(&info.volume, |this, _, cx| {
                if this.active {
                    cx.notify();
                }
            })
            .detach();

            cx.observe(&info.playback_state, |this, state, cx| {
                // 读值缓存无条件更新（视图隐藏期间也不能落伍，否则激活后
                // 泵判据是陈值）；notify 行为保持原样。
                this.playing = *state.read(cx) == PlaybackState::Playing;
                if this.active {
                    cx.notify();
                }
            })
            .detach();

            // 每次切歌的两个触发器都汇入同一个带守卫的 resolve：SongChanged
            // 落地时队列位置还指向上一个曲目，QueuePositionChanged 才补齐全貌。
            cx.observe(&info.current_track, |this, _, cx| {
                if this.active {
                    this.resolve_track_presentation(cx);
                }
            })
            .detach();
            cx.observe(&queue, |this, _, cx| {
                if this.active {
                    this.resolve_track_presentation(cx);
                }
            })
            .detach();

            // Lyrics 实体在任何加载（本地或在线抓取）落地时都会无条件 notify；
            // 代际守卫让重复 notify 不再重复克隆已解析的行。
            cx.observe(&lyrics, |this: &mut ImmersiveView, lyrics, cx| {
                if !this.active {
                    return;
                }
                let generation = lyrics.read(cx).parsed_generation();
                if generation != this.synced_lyrics_generation {
                    this.synced_lyrics_generation = generation;
                    this.sync_lyrics(cx);
                    cx.notify();
                }
            })
            .detach();

            ensure_analyzer(cx);
            let (spectrum_data, spectrum_viewers) =
                if let Some(state) = cx.try_global::<SpectrumState>() {
                    (Some(state.data.clone()), Some(state.viewers.clone()))
                } else {
                    (None, None)
                };
            if let Some(spectrum) = &spectrum_data {
                cx.observe(spectrum, |this, _, cx| {
                    if this.active && this.stage_mode == ImmersiveStageMode::KineticLyrics {
                        cx.notify();
                    }
                })
                .detach();
            }

            let mut this = Self {
                focus_handle: cx.focus_handle(),
                active: *immersive_flag.read(cx),
                stage_mode: ImmersiveStageMode::PosterWall,
                poster_wall_scroll: ScrollHandle::new(),
                poster_scroll_probe: Rc::new(PosterScrollProbe {
                    render_top: Cell::new(0.0),
                    healed: Cell::new((i32::MIN, i32::MIN)),
                    logged: Cell::new((i32::MIN, i32::MIN)),
                }),
                console_hovered: false,
                position: info.position.clone(),
                duration: info.duration.clone(),
                sample_rate: info.sample_rate.clone(),
                spectrum_data,
                spectrum_viewers,
                spectrum_viewers_registered: false,
                volume: info.volume.clone(),
                stage_time_labels: CachedTimeText::unseeded(),
                cached_volume_pct: i64::MIN,
                cached_volume_text: SharedString::default(),
                spectrum_bar_buffer: Vec::new(),
                playback_state: info.playback_state.clone(),
                playing: false,
                current_track: info.current_track.clone(),
                queue,
                lyrics,
                synced_lyrics_generation: 0,
                image_key: None,
                image_element_key: 0,
                track_name: None,
                artist_name: None,
                meta_subscription: None,
                resolved_signature: None,
                parsed: None,
                current_line: None,
                frame_scheduled: false,
                backdrop_key: None,
                backdrop_layers: [None, None],
                backdrop_active: 0,
                backdrop_gen: 0,
                backdrop_target: None,
                backdrop_pending_arm: false,
                backdrop_fade_started: None,
                backdrop_task: None,
                prefetched_art: None,
                prefetch_task: None,
            };
            if this.active {
                this.activate(cx);
            }
            this
        })
    }

    /// `Models::immersive` 在视图仍活着（住在 `MainWindow` 里）时被翻转为
    /// 开/关。
    fn set_active(&mut self, active: bool, cx: &mut Context<Self>) {
        if self.active == active {
            return;
        }
        self.active = active;
        if active {
            self.activate(cx);
        } else {
            self.deactivate();
        }
        cx.notify();
    }

    /// 把频谱观察者席位对齐到 want 状态（激活且处于 KineticLyrics 舞台），
    /// 按翻转对共享原子做差值加减。PosterWall 停留期间席位归零，分析器进
    /// 1s 驻车档（~94 次 FFT/s 后台 CPU 与 30 次/s 主线程实体更新全部归
    /// 零）；频谱 observer 的 notify 本就按舞台门控（见 new 里的观察者），
    /// 这里管的是分析器与数据产出本身。
    fn sync_spectrum_viewers(&mut self) {
        let want = self.active && self.stage_mode == ImmersiveStageMode::KineticLyrics;
        if want == self.spectrum_viewers_registered {
            return;
        }
        self.spectrum_viewers_registered = want;
        let Some(viewers) = &self.spectrum_viewers else {
            return;
        };
        if want {
            viewers.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        } else {
            viewers.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn activate(&mut self, cx: &mut Context<Self>) {
        // 进入重模式先让位：背景管线即将支付 ~105MB 瞬态（4096² 解码 +
        // 裁剪带，2026-10-02 硬上限重启复盘），普通界面的封面瓦片收缩到
        // 半个近期窗口，驱动侧提交经回收漏斗年龄门（5s/1s，2026-11-04
        // 决议）秒量级归还。保留的是 LRU 尾部——当前曲目封面大概率在
        // 内，沉浸页圆盘标签不受影响；退出后浏览库视图时被逐出的封面按
        // 需重解码（磁盘缓存命中）。
        render_cache_shrink(RENDER_CACHE_PRESSURE_KEEP);
        self.resolve_track_presentation(cx);
        // 重进沉浸页：resolve 可能因签名守卫早退（同一首歌），这里兜底
        // 重新布防背景 settle（退出时已失显/中止）。
        self.arm_backdrop(cx);
        self.sync_lyrics(cx);
        // 同首歌重进（resolve 早退）时 resolve 里的预取不会执行，这里补。
        self.prefetch_next_track_art(cx);
        self.sync_spectrum_viewers();
    }

    fn deactivate(&mut self) {
        self.sync_spectrum_viewers();
        // 退出沉浸页：取消解码任务并立即失显。已显示背景随元素 unmount
        // 推进回收漏斗，短年龄门（大图 1s，2026-11-04 决议）后像素与瓦片
        // 放行；背景缓存
        // 收缩到仅当前歌曲一套（重进沉浸页秒开），预取图与上一首经漏斗
        // 归还——普通模式下不为看不见的背景驻留 ~16MB（"退出界面即归
        // 还"纪律的执行点）。预取记忆一并清除：收缩后记忆指向的条目可
        // 能已被逐出，残留会让下次激活误信缓存命中而漏掉重预取。
        // 未 detach 的 Task 置空即取消。
        self.backdrop_task = None;
        // 在途预取一并取消：否则预取完成后 backdrop_cache_insert 会把下
        // 一首的大图写回刚被 shrink(1) 收缩的缓存（异步完成时序晚于本函数
        // 的同步收缩），"退出界面即归还"落空；重进 activate 补预取时还会
        // 与之同键双解码。
        if let Some(task) = self.prefetch_task.take() {
            task.abort();
        }
        self.backdrop_key = None;
        self.backdrop_layers = [None, None];
        self.backdrop_pending_arm = false;
        self.backdrop_fade_started = None;
        self.prefetched_art = None;
        // 歌词镜像一并归还：观察者在非激活态被短路，旧歌词 Arc 否则会挂到
        // 下次 activate 的 sync_lyrics（几十 KB 驻留）；重进时镜像无条件
        // 重建，清空零行为差。
        self.parsed = None;
        backdrop_cache_shrink(1);
        // 退出沉浸页时异步触发一次 mimalloc purge：大图卸载后的几十兆堆碎片立刻归还系统，
        // 杜绝退出全屏后内存数字迟迟不降。
        crate::RUNTIME.spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
            let (before, _) = crate::mimalloc_memory_mb();
            crate::mimalloc_force_collect_reclaim_mb(before);
        });
    }
    /// 针对此刻正在播放的曲目重新解析封面/名称/取色。由 `SongChanged` 与
    /// `QueuePositionChanged` 双双触发；签名守卫让每次切歌的第二次触发变成
    /// 空操作。
    fn resolve_track_presentation(&mut self, cx: &mut Context<Self>) {
        let (position, item) = {
            let queue = self.queue.read(cx);
            let position = queue.position;
            let item = queue.data.read_recovered().get(position).cloned();
            (position, item)
        };
        let track_path = self
            .current_track
            .read(cx)
            .as_ref()
            .map(|track| track.get_path().clone());
        let signature = (position, track_path);
        if self.resolved_signature.as_ref() == Some(&signature) {
            return;
        }
        let track_path = signature.1.clone();
        self.resolved_signature = Some(signature);

        self.image_element_key += 1;
        self.track_name = None;
        self.artist_name = None;
        drop(self.meta_subscription.take());

        let mut cover_key = None;
        // SongChanged 先于 QueuePositionChanged 落地时，队列槽位仍指向
        // 上一首：这次 resolve 解析出的封面键属于旧曲目，只为名字/标签
        // 服务，绝不能据此布防背景解码（否则每首歌先为马上作废的键付
        // 一次 2048 解码再被取消）。
        let resolved_slot_is_current = item.as_ref().is_none_or(|slot_item| {
            let slot_path = slot_item.get_path().clone();
            track_path.as_ref().is_some_and(|path| *path == slot_path)
        });
        if let Some(item) = &item {
            // 在线曲目的队列条目自带封面 URL；本地文件回退到内嵌封面 /
            // sidecar 文件。
            #[cfg(feature = "online_sources")]
            if let Some(url) = queue_metadata::queue_ui_data(cx, item)
                .read(cx)
                .clone()
                .and_then(|data| data.cover_url)
                .filter(|url| !url.is_empty())
            {
                // 大屏显示档：队列里带的是 256px 缩略图，铺满全屏背景会糊成一团。
                cover_key = Some(ManagedImageKey::HttpCoverLarge(url));
            }
            if cover_key.is_none() {
                cover_key = Some(ManagedImageKey::track_file(item.get_path()));
            }

            // 句柄来自元数据旁表（P2-B 第 4 步）：同一 uid 恒给出同一实体，
            // 所以下面的 `meta_subscription` 观察的正是数据所在的那个槽位。
            // `adopt_names` 不变——它消费实体的内容，只是订阅的来源换了。
            let data = queue_metadata::queue_ui_data(cx, item);
            let item_path = item.get_path().clone();
            let is_current_slot = track_path.as_ref().is_some_and(|path| *path == item_path);
            // 名称只取自真正持有播放中曲目的那个槽位——陈旧槽位
            // （SongChanged 早于 QueuePositionChanged）绝不能锁死，
            // 改由位置变化那次 resolve 纠正。
            if is_current_slot && let Some(ui_data) = data.read(cx).clone() {
                self.adopt_names(ui_data);
            }
            self.meta_subscription = Some(cx.observe(&data, move |this, data, cx| {
                if !this.active {
                    return;
                }
                let is_current = this
                    .current_track
                    .read(cx)
                    .as_ref()
                    .is_some_and(|track| track.get_path() == &item_path);
                if is_current && let Some(ui_data) = data.read(cx).clone() {
                    this.adopt_names(ui_data);
                    cx.notify();
                }
            }));
        }
        self.image_key = cover_key;
        // 完全停止（无 current_track）时沿用原行为照常布防；只有
        // "槽位与播放中曲目不一致" 的陈旧 resolve 才跳过。
        if resolved_slot_is_current || track_path.is_none() {
            self.arm_backdrop(cx);
        }

        // 沉浸模式下切歌先收拢通用封面缓存：为大图解码留出充裕堆空间，
        // 消除连续切歌瞬间的双轨显存尖刺（峰值从 ~800MB 降至安全线以内）。
        if self.active {
            render_cache_shrink(RENDER_CACHE_PRESSURE_KEEP);
        }
        // 当前曲目布防完成后，顺手把下一首的圆盘图与背景渲染进缓存：
        // 切歌瞬间整条链路零解码（"切换不丝滑"的收尾一环）。仅沉浸页
        // 激活时执行，关闭页面时不为看不见的背景烧 CPU/流量。
        if resolved_slot_is_current || track_path.is_none() {
            self.prefetch_next_track_art(cx);
        }
        cx.notify();
    }

    /// 下一首曲目的封面键：与 `resolve_track_presentation` 同一构造（在
    /// 线取 cover_url 的 Large 变体，本地回退 TrackFile），保证预取写入
    /// 的正是切歌时元素查找的缓存键。队列末尾返回 None。
    fn next_track_art_key(&self, cx: &mut Context<Self>) -> Option<ManagedImageKey> {
        let slot = {
            let queue = self.queue.read(cx);
            queue
                .data
                .read_recovered()
                .get(queue.position + 1)
                .cloned()?
        };
        #[cfg(feature = "online_sources")]
        if let Some(url) = queue_metadata::queue_ui_data(cx, &slot)
            .read(cx)
            .clone()
            .and_then(|data| data.cover_url)
            .filter(|url| !url.is_empty())
        {
            return Some(ManagedImageKey::HttpCoverLarge(url));
        }
        Some(ManagedImageKey::track_file(slot.get_path()))
    }

    /// 预取下一首的 512 圆盘缩略图（RENDER_CACHE）与整幅背景
    /// （Backdrop 缓存）：与真实切换的元素路径同键同模式，切歌时直接命
    /// 中——圆盘与背景同时零解码换上。解码信号量天然让预取排在其后，不
    /// 抢当前 swap 的道；`(键, 目标尺寸)` 记忆去重，队列末尾、下一首即
    /// 当前曲目时跳过。
    fn prefetch_next_track_art(&mut self, cx: &mut Context<Self>) {
        if !self.active {
            return;
        }
        // 内存压力下不预取：省掉一整条 4096 重管线 + 一个背景缓存槽。
        // 切歌照常即时解码（arm_backdrop 不受压力旗标影响），代价只是
        // 换歌瞬间多付 ~1s 解码——压力态下这是正确的取舍。不写预取记忆，
        // 压力清除后下次 resolve 自然恢复预取。
        if memory_pressure() {
            return;
        }
        let Some((target_w, target_h)) = self.backdrop_target else {
            return;
        };
        let Some(next_key) = self.next_track_art_key(cx) else {
            return;
        };
        if self.image_key.as_ref() == Some(&next_key)
            || self.prefetched_art.as_ref() == Some(&(next_key.clone(), target_w, target_h))
        {
            return;
        }
        if !backdrop_decode_allowed() {
            return; // 背压饱和：不写记忆，下次 resolve 重试
        }
        self.prefetched_art = Some((next_key.clone(), target_w, target_h));
        let pool = cx.global::<Pool>().0.clone();
        // 句柄留存在案：deactivate 据此 abort（若再切歌则被新预取覆盖，
        // 旧任务自然跑完，与既往"丢弃句柄"行为一致且写回的是当前曲目条
        // 目，无害）。
        self.prefetch_task = Some(crate::RUNTIME.spawn(async move {
            // 背景先行：它是切歌丝滑的关键路径。两条预取各付一次完整
            // 4096 解码，用户若在中间切歌，至少背景已就绪。
            let _ = next_key
                .retrieve(
                    pool.clone(),
                    0,
                    ImageCacheMode::Backdrop(Some((target_w, target_h))),
                )
                .await;
            let _ = next_key
                .retrieve(pool, LABEL_THUMB_PX, ImageCacheMode::RenderCache)
                .await;
        }));
    }

    /// 切歌即启动背景解码（切歌/退出时上一个任务随之取消——从未显示的
    /// 背景不进回收漏斗，零队列成本，§33/§34）；解码完成时若曲目仍是当
    /// 前这首才晋升交换。晋升即进入 400ms crossfade：旧背景降级为淡出
    /// 层继续绘制，新图在其上淡入，没有黑底间隙也没有硬切闪变。大图回
    /// 收队列未放行（>2 套在队）时按 [`BACKDROP_RETRY`] 退避重试，是
    /// "同时驻留大图套数"硬上界的执行点；大图 1s 短年龄门（2026-11-04
    /// 决议）让积压在秒级排空，普通切歌节奏下阀门不再成为"背景不跟歌"
    /// 的来源。
    /// 这张背景图是否仍是本页正在显示的那张：必须是激活态且键与当前一致。
    /// `arm_backdrop` 的每一跳（重试退避后、解码完成后）都问这同一句，此前
    /// 三处逐字重复整条表达式。
    fn is_showing(&self, key: &ManagedImageKey) -> bool {
        self.active && self.image_key.as_ref() == Some(key)
    }

    fn arm_backdrop(&mut self, cx: &mut Context<Self>) {
        // 设备像素目标尺寸由沉浸页 render 提供（activate 早于首个
        // render，先挂起，render 里补布防）。
        let Some((target_w, target_h)) = self.backdrop_target else {
            self.backdrop_pending_arm = true;
            return;
        };
        self.backdrop_pending_arm = false;
        let key = self.image_key.clone();
        if self.backdrop_key == key && key.is_some() {
            return; // 已在显示
        }
        // 同键管线在途：等它晋升，绝不重付一次解码。2026-11-10 实测从最
        // 大化窗口进入沉浸时视口 1020→1080 跳变，resolve 恰落在"解码完
        // 成→晋升"的窗口里，取消刚解码完的任务、再按新目标重付整条管线
        // （~25MB 瞬态 + ~1s CPU，各带一套图集纹理）。在途图晋升后按其
        // 自己的目标尺寸绘制，GPU 双线性拉伸几个百分点即可接受。
        if key.is_some()
            && self
                .backdrop_task
                .as_ref()
                .is_some_and(|(armed, _)| Some(armed) == key.as_ref())
        {
            return;
        }
        // 未 detach 的 Task 在替换/置空时即取消（equalizer 防抖同款语义）。
        self.backdrop_task = None;
        let Some(key) = key else {
            // 无封面曲目：立即失显（渲染以槽位图层为准，两槽都要清）。
            self.backdrop_key = None;
            self.backdrop_layers = [None, None];
            self.backdrop_fade_started = None;
            return;
        };
        let pool = cx.global::<Pool>().0.clone();
        let armed_key = key.clone();
        self.backdrop_task = Some((
            armed_key,
            cx.spawn(async move |this, cx| {
                let mut render_misses = 0u32;
                loop {
                    // 缓存命中（预取/回跳）不产生新解码，直接绕过背压阀门：
                    // 连跳时阀门按 1s/套排水（2026-11-04 决议），不能把已经渲
                    // 染好的下一首也拖住（"换了歌还是之前的图"的主因）。
                    let cached = backdrop_cache_contains(&key, target_w, target_h);
                    if !cached && !backdrop_decode_allowed() {
                        cx.background_executor().timer(BACKDROP_RETRY).await;
                        let still = this
                            .update(cx, |this, _| this.is_showing(&key))
                            .unwrap_or(false);
                        if !still {
                            return;
                        }
                        continue;
                    }
                    let decoded = crate::RUNTIME
                        .spawn({
                            let key = key.clone();
                            let pool = pool.clone();
                            // 解码 bound 给目标长边 25% 余量：KuGou 2048 源不被
                            // 压缩（2048 < 2400），超大本地图先 box 压到界内，
                            // 之后只做一次 Lanczos 裁剪重采样。
                            let bound = (target_w.max(target_h) * 5 / 4).clamp(2048, 3200);
                            async move {
                                key.retrieve(
                                    pool,
                                    bound,
                                    ImageCacheMode::Backdrop(Some((target_w, target_h))),
                                )
                                .await
                                .ok()
                                .flatten()
                            }
                        })
                        .await
                        .ok()
                        .flatten();
                    // 解码/渲染失败（网络抖动、临时离线）：有限次退避重试，
                    // 期间旧背景保持绘制；曲目已变则放弃（成果留缓存）。
                    // 不重试会让旧图一直挂到下一次手动操作。像素本身由背景
                    // 缓存持有，晋升时元素按同键取回，这里只做存在性检查。
                    let Some(_) = decoded else {
                        render_misses += 1;
                        if render_misses > 3 {
                            return;
                        }
                        cx.background_executor().timer(BACKDROP_RETRY).await;
                        let still = this
                            .update(cx, |this, _| this.is_showing(&key))
                            .unwrap_or(false);
                        if !still {
                            return;
                        }
                        continue;
                    };
                    // 解码完成：曲目已又变则放弃晋升（成果留在背景缓存里，
                    // 回跳零成本）。
                    let promote = this
                        .update(cx, |this, _| this.is_showing(&key))
                        .unwrap_or(false);
                    if !promote {
                        return;
                    }
                    this.update(cx, |this, cx| {
                        if this.image_key.as_ref() != Some(&key) {
                            return;
                        }
                        let reduced_motion = reduced_motion_enabled(cx);
                        // 新图进入空闲槽（槽位翻转），旧图原地留作淡出层——
                        // 元素 id 与祖先路径全程不变，keyed state 不重建、不
                        // 重解码。被覆盖的槽若有残留（上一次过渡被打断的图），
                        // 其元素就地卸载进回收漏斗。reduced_motion 硬切。
                        let incoming = 1 - this.backdrop_active;
                        this.backdrop_active = incoming;
                        if reduced_motion {
                            this.backdrop_layers[1 - incoming] = None;
                            this.backdrop_fade_started = None;
                        } else {
                            this.backdrop_fade_started = Some(Instant::now());
                        }
                        this.backdrop_gen += 1;
                        this.backdrop_layers[incoming] =
                            Some((key.clone(), this.backdrop_gen, (target_w, target_h)));
                        this.backdrop_key = Some(key);
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            }),
        ));
    }

    /// 渲染一个背景槽位图层：显式 id 让元素祖先路径跨帧稳定（槽位是否
    /// 有图、图是否更换都不影响另一个槽位的 keyed state）；opacity 由
    /// wrapper div 的 `opacity()` 设置，gpui 的 element_opacity 栈在绘制
    /// 时乘进子 `ManagedImage` 的精灵。纹理按该层晋升时的设备像素尺寸
    /// 精确重采样，绘制时 GPU 1:1 取样。
    fn backdrop_layer(&self, index: usize, opacity: f32) -> impl IntoElement {
        let layer = self.backdrop_layers[index].clone();
        div()
            .id(("immersive-bg-layer", index as u64))
            .absolute()
            .inset_0()
            .opacity(opacity)
            .when_some(layer, move |el, (key, gen_id, (w, h))| {
                // 用层自己的目标尺寸，而不是当前 `backdrop_target`：后者在
                // 进入 OS 全屏的头几帧会从窗口尺寸跳到全屏尺寸，元素的
                // 缓存键随之变化会重启一整条解码管线（2026-11-10 实测同一
                // 首歌连续两次 2048 源解码、各带一套图集纹理）。旧图由 GPU
                // 双线性拉伸几个百分点，代价远小于再来一条 ~25MB 瞬态管线。
                let image =
                    managed_image(("immersive-bg", gen_id), key).backdrop_cached_target(w, h);
                el.child(image.w_full().h_full().object_fit(ObjectFit::Cover))
            })
    }

    fn adopt_names(&mut self, data: crate::playback::queue::QueueItemUIData) {
        self.track_name = data.name.or(self.track_name.take());
        self.artist_name = data.artist_name.or(self.artist_name.take());
    }

    /// 镜像共享歌词实体的已解析行，并直接吸附到当前行，不滑动穿过整个列表。
    fn sync_lyrics(&mut self, cx: &mut Context<Self>) {
        self.synced_lyrics_generation = self.lyrics.read(cx).parsed_generation();
        // parsed_lines 返回的 Arc 与面板/缓存共享同一份行数据，镜像只付
        // 引用计数。
        self.parsed = self.lyrics.read(cx).parsed_lines();
        let pos_ms = *self.position.read(cx);
        self.current_line = None;
        if let Some(parsed) = &self.parsed {
            let idx = parsed.partition_point(|line| line.time_ms <= pos_ms);
            self.current_line = if idx == 0 { None } else { Some(idx - 1) };
        }
    }

    /// 返回活动行是否移动了（即歌词栏发生变化、需要重绘）。
    fn track_lyric_line(&mut self, pos_ms: u64) -> bool {
        let Some(parsed) = &self.parsed else {
            return false;
        };
        let idx = parsed.partition_point(|line| line.time_ms <= pos_ms);
        let new_line = if idx == 0 { None } else { Some(idx - 1) };
        if new_line == self.current_line {
            return false;
        }
        self.current_line = new_line;
        true
    }

    fn needs_animation_frame(&self) -> bool {
        self.backdrop_fade_started.is_some()
            // 暂停态停转扫色泵：KineticLyrics 的逐字扫色由 render 按
            // position_ms 现算（word_progress），无墙钟插值，暂停时位置
            // 不动，advance_animations 的自续 schedule+notify 只是白付整
            // 帧重绘。恢复播放经 playback_state 观察者 notify 自然重启。
            // 前一条款是自终止动画（走完即 None），不受影响。
            || (self.active && self.playing && self.stage_mode == ImmersiveStageMode::KineticLyrics)
    }

    fn schedule_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.frame_scheduled {
            return;
        }
        self.frame_scheduled = true;
        cx.on_next_frame(window, |this, window, cx| {
            this.frame_scheduled = false;
            this.advance_animations(window, cx);
        });
    }

    fn advance_animations(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // crossfade 走完：清空淡出槽，旧背景的瓦片随后经回收漏斗放行。
        if let Some(started) = self.backdrop_fade_started
            && started.elapsed() >= BACKDROP_FADE
        {
            self.backdrop_fade_started = None;
            self.backdrop_layers[1 - self.backdrop_active] = None;
        }
        if self.needs_animation_frame() {
            self.schedule_frame(window, cx);
        }
        cx.notify();
    }

    /// 海报墙只实体化滚动窗口内（含缓冲）的卡片。`ManagedImage` 的
    /// `request_layout` 会立刻发起解码；整队列渲染意味着队列里每张封面都会
    /// 被解码、上传图集并在元素状态里驻留（2026-11-10 日志实测：50 首队列
    /// 进入沉浸页 8 秒内 private 418→822MB 触发硬上限重启，退场时回收漏斗
    /// 积压 50MB）。卡片位置完全由高度序列决定，故用显式列高 + 绝对定位做
    /// 窗口化，滚动范围与瀑布流视觉不变。O(队列长度) 扫描是纯算术，比构建
    /// 元素便宜两个数量级。
    fn render_poster_wall(
        &self,
        viewport_height: f32,
        viewport_width_device: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (current_pos, queue_len) = {
            let q = self.queue.read(cx);
            (q.position, q.data.read_recovered().len())
        };

        if queue_len == 0 {
            let theme = cx.global::<Theme>();
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_size(px(16.0))
                        .text_color(theme.text_secondary)
                        .child("播放列表为空"),
                )
                .into_any_element();
        }

        // 循环里要经 `cx.listener` 拿可变借用，主题值先拷出来（Theme: Clone，
        // 字段都是小值），否则与全局的不可变借用冲突。
        let theme = cx.global::<Theme>().clone();
        let poster_scroll = self.poster_wall_scroll.clone();
        let current_lyric = self
            .current_line
            .and_then(|idx| self.parsed.as_ref().and_then(|p| p.get(idx)))
            .map(|l| l.text.clone())
            .unwrap_or_default();

        // 6 列全屏自适应流式瀑布流：宽度完全铺满屏幕，上下紧密咬合，绝无窟窿空隙
        let raw_offset_y = self.poster_wall_scroll.offset().y.as_f32();
        let content_height = poster_content_height(queue_len, current_pos);
        let scroll_top = poster_scroll_top(raw_offset_y, content_height, viewport_height);
        self.poster_scroll_probe.render_top.set(scroll_top);
        let window_top = scroll_top - POSTER_OVERSCAN;
        let window_bottom = scroll_top + viewport_height.max(1.0) + POSTER_OVERSCAN;
        let poster_thumb = poster_thumb_size(viewport_width_device);

        let mut column_cards: [Vec<AnyElement>; POSTER_COLUMNS] = Default::default();
        let mut column_heights = [0.0f32; POSTER_COLUMNS];
        let mut materialized = 0usize;

        for (col_idx, column) in column_cards.iter_mut().enumerate() {
            let (slots, column_height) =
                poster_column_layout(queue_len, col_idx, current_pos, window_top, window_bottom);
            column_heights[col_idx] = column_height;
            for (idx, y, h) in slots {
                materialized += 1;
                let is_playing_track = idx == current_pos;
                let item = { self.queue.read(cx).data.read_recovered().get(idx).cloned() };
                if let Some(item) = item {
                    let ui_data = queue_metadata::queue_ui_data(cx, &item).read(cx).clone();
                    let title = ui_data
                        .as_ref()
                        .and_then(|u| u.name.clone())
                        .unwrap_or_else(|| SharedString::from(format!("Track #{}", idx + 1)));
                    let artist = ui_data
                        .as_ref()
                        .and_then(|u| u.artist_name.clone())
                        .unwrap_or_default();
                    #[cfg(feature = "online_sources")]
                    let cover_key = ui_data
                        .and_then(|u| u.cover_url)
                        .filter(|u| !u.is_empty())
                        .map(ManagedImageKey::HttpCover)
                        .unwrap_or_else(|| ManagedImageKey::track_file(item.get_path()));
                    // 无在线音源时 `HttpCover` 变体不存在，封面只能按本地曲目路径取。
                    #[cfg(not(feature = "online_sources"))]
                    let cover_key = ManagedImageKey::track_file(item.get_path());

                    column.push(
                        div()
                            .id(("poster_tile", idx))
                            .absolute()
                            .left_0()
                            .right_0()
                            .top(px(y))
                            .h(px(h))
                            .rounded(px(16.0))
                            .shadow_lg()
                            .border_1()
                            .border_color(if is_playing_track {
                                theme.button_primary_border
                            } else {
                                Rgba::new(1.0, 1.0, 1.0, 0.12)
                            })
                            .bg(Rgba::new(0.08, 0.09, 0.12, 0.85))
                            .overflow_hidden()
                            // 这里绝不能再链 .relative()：两者都写 style.position，
                            // 后者覆盖前者——2026-11-11 实测卡片因此退回流式布局，
                            // top(y) 变成相对偏移，整墙下坠出黑带且尾部卡片滚不到。
                            // absolute 本身就是内部标签层（absolute bottom_0）的
                            // 定位锚，不需要 relative。
                            .cursor_pointer()
                            .hover(|s| s.border_color(theme.button_primary))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.global::<PlaybackInterface>().jump(idx);
                                this.stage_mode = ImmersiveStageMode::KineticLyrics;
                                this.sync_spectrum_viewers();
                                cx.notify();
                            }))
                            .child(
                                managed_image(("poster_img", idx), cover_key)
                                    .thumb_max(poster_thumb)
                                    .size_full()
                                    .object_fit(ObjectFit::Cover),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .left_0()
                                    .right_0()
                                    .bottom_0()
                                    .pt(px(28.0))
                                    .pb(px(10.0))
                                    .px(px(12.0))
                                    .bg(linear_gradient(
                                        0.0,
                                        linear_color_stop(Rgba::new(0.0, 0.0, 0.0, 0.90), 0.0),
                                        linear_color_stop(Rgba::new(0.0, 0.0, 0.0, 0.0), 1.0),
                                    ))
                                    .flex()
                                    .flex_col()
                                    .gap(px(2.0))
                                    .when(is_playing_track, |d| {
                                        d.child(
                                            div()
                                                .flex()
                                                .items_center()
                                                .gap(px(6.0))
                                                .child(
                                                    div()
                                                        .size(px(6.0))
                                                        .rounded_full()
                                                        .bg(theme.button_primary),
                                                )
                                                .child(
                                                    div()
                                                        .text_size(px(10.0))
                                                        .font_weight(FontWeight::BOLD)
                                                        .text_color(theme.button_primary)
                                                        .child("NOW PLAYING"),
                                                ),
                                        )
                                    })
                                    .child(
                                        div()
                                            .text_size(if is_playing_track {
                                                px(15.0)
                                            } else {
                                                px(12.0)
                                            })
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Rgba::new(1.0, 1.0, 1.0, 0.95))
                                            .truncate()
                                            .child(title),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.0))
                                            .text_color(Rgba::new(1.0, 1.0, 1.0, 0.60))
                                            .truncate()
                                            .child(artist),
                                    )
                                    .when(is_playing_track && !current_lyric.is_empty(), |d| {
                                        d.child(
                                            div()
                                                .mt(px(4.0))
                                                .text_size(px(11.0))
                                                .text_color(Rgba::new(1.0, 0.85, 0.42, 0.95))
                                                .truncate()
                                                .child(current_lyric.clone()),
                                        )
                                    }),
                            )
                            .into_any_element(),
                    );
                }
            }
        }

        let masonry_cols: Vec<AnyElement> = column_cards
            .into_iter()
            .zip(column_heights)
            .enumerate()
            .map(|(c_idx, (col_items, col_height))| {
                div()
                    .id(("masonry_col", c_idx))
                    .relative()
                    .flex_1()
                    .min_w(px(180.0))
                    .h(px(col_height))
                    .children(col_items)
                    .into_any_element()
            })
            .collect();

        // render↔paint 偏移探针（见 [`PosterScrollProbe`]）：canvas 的 prepaint
        // 在滚动容器钳位之后执行，读到的就是 paint 实际使用的偏移。
        let probe = self.poster_scroll_probe.clone();
        let scroll_handle = self.poster_wall_scroll.clone();
        let entity_id = cx.entity().entity_id();
        let (probe_raw, probe_content, probe_viewport, probe_queue, probe_pos, probe_cards) = (
            raw_offset_y,
            content_height,
            viewport_height,
            queue_len,
            current_pos,
            materialized,
        );
        let probe_canvas = canvas(
            move |_, _, cx| {
                let paint_top = (-scroll_handle.offset().y.as_f32()).max(0.0);
                let render_top = probe.render_top.get();
                let diff = paint_top - render_top;
                let bucket = |v: f32| (v / 48.0) as i32;
                let key = (bucket(render_top), bucket(diff));
                if key != probe.logged.get() {
                    probe.logged.set(key);
                    tracing::info!(
                        target: "poster",
                        "scroll render_top={render_top:.0} paint_top={paint_top:.0} \
                         diff={diff:.0} raw={probe_raw:.0} content={probe_content:.0} \
                         viewport={probe_viewport:.0} queue_len={probe_queue} \
                         pos={probe_pos} cards={probe_cards}"
                    );
                }
                if let Some(healed) = poster_scroll_should_heal(render_top, paint_top, probe.healed.get())
                {
                    probe.healed.set(healed);
                    tracing::warn!(
                        target: "poster",
                        "scroll divergence self-heal render_top={render_top:.0} paint_top={paint_top:.0}"
                    );
                    cx.notify(entity_id);
                }
            },
            |_, _, _, _| {},
        )
        .absolute();

        div()
            .id("stage-poster-wall")
            .size_full()
            .pt(px(20.0))
            .pb(px(24.0))
            .px(px(16.0))
            .overflow_y_scroll()
            .track_scroll(&poster_scroll)
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .gap(px(14.0))
                    .children(masonry_cols)
                    .child(probe_canvas),
            )
            .into_any_element()
    }
}

impl Render for ImmersiveView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _render = crate::tally_render(crate::RenderedView::Immersive);
        // 让键盘焦点落到 Esc 绑定上（AboutDialog 的模式；幂等）。
        self.focus_handle.focus(window, cx);

        let reduced_motion = reduced_motion_enabled(cx);
        if self.needs_animation_frame() {
            if reduced_motion {
                self.advance_animations(window, cx);
            } else {
                self.schedule_frame(window, cx);
            }
        }

        // 背景重采样目标 = 背景 inset_0 元素的设备像素尺寸（本机
        // 1920×1080@125% → 1920×1028）。长边超 2560 时等比缩界，把
        // 4K 屏的纹理驻留压回合理区间（放大交还 GPU，质量仍优于旧路径）。
        {
            let dpr = window.scale_factor();
            let vp = window.viewport_size();
            let (mut tw, mut th) = (
                (vp.width.as_f32() * dpr).round().max(1.0),
                (vp.height.as_f32() * dpr).round().max(1.0),
            );
            let long = tw.max(th);
            if long > 2560.0 {
                let k = 2560.0 / long;
                tw = (tw * k).round().max(1.0);
                th = (th * k).round().max(1.0);
            }
            self.backdrop_target = Some((tw as u32, th as u32));
            if self.backdrop_pending_arm {
                self.arm_backdrop(cx);
            }
        }

        let theme = cx.global::<Theme>();
        let text = theme.text;
        let text_secondary = theme.text_secondary;
        let image_gen = self.image_element_key;

        // PlaybackInfo 的位置用毫秒。底栏时间标签与悬浮进度条在这里读取。
        let position_ms = *self.position.read(cx);
        let duration_ms = *self.duration.read(cx);
        let sample_rate = *self.sample_rate.read(cx);
        let playing = *self.playback_state.read(cx) == PlaybackState::Playing;
        let volume = *self.volume.read(cx);

        // 全屏底栏的时间/音量标签：整秒与整数百分比没跨档时复用已格式化的
        // SharedString，30Hz 重绘里绝大多数帧的输出逐字节相同。
        let (current_time_text, total_time_text) = self.stage_time_labels.resolve(
            position_ms / 1000,
            duration_ms / 1000,
            |position_secs, duration_secs| {
                (
                    SharedString::from(format!("{}:{:02}", position_secs / 60, position_secs % 60)),
                    SharedString::from(format!("{}:{:02}", duration_secs / 60, duration_secs % 60)),
                )
            },
        );
        let volume_pct = (volume * 100.0).round() as i64;
        if self.cached_volume_pct != volume_pct {
            self.cached_volume_pct = volume_pct;
            self.cached_volume_text = SharedString::from(format!("{}%", volume_pct));
        }
        let volume_text = self.cached_volume_text.clone();

        // 背景：全清晰度解码、Cover 适配、绝不模糊——清晰度就是目的；下面的
        // 渐变划出阅读区。两个固定图层渲染的是 `backdrop_layers`（解码就绪的
        // 槽位）而不是 `image_key`：视图只有在拿到解码后的 Arc 时才晋升槽位，
        // 所以上一张图会一直绘制到交换为止——没有黑底间隙。交换本身是一次
        // 400ms crossfade：淡出层以 (1-p) 在其下继续绘制，进入层以 p 淡入
        // （div opacity → gpui 的 element_opacity → PolychromeSprite opacity）。
        let fade_progress = self.backdrop_fade_started.map_or(1.0, |started| {
            ease_out_cubic((started.elapsed().as_secs_f32() / BACKDROP_FADE.as_secs_f32()).min(1.0))
        });
        let backdrop = div()
            .absolute()
            .inset_0()
            .overflow_hidden()
            .child(self.backdrop_layer(
                1 - self.backdrop_active,
                (1.0 - fade_progress).clamp(0.0, 1.0),
            ))
            .child(self.backdrop_layer(self.backdrop_active, fade_progress.clamp(0.0, 1.0)));
        // 整体暗度：全局底板遮罩深度提升，确保大图背景不抢戏
        let vertical_shade = div().absolute().inset_0().bg(linear_gradient(
            0.0,
            linear_color_stop(Rgba::new(0.0, 0.0, 0.0, 0.65), 0.0),
            linear_color_stop(Rgba::new(0.0, 0.0, 0.0, 0.35), 1.0),
        ));
        // 右侧歌词阅读区专用渐变暗角（Vignette Scrim）：彻底保障白色与高光文字对比度
        let right_shade = div().absolute().inset_0().bg(linear_gradient(
            90.0,
            linear_color_stop(Rgba::new(0.0, 0.0, 0.0, 0.35), 0.0),
            linear_color_stop(Rgba::new(0.0, 0.0, 0.0, 0.82), 1.0),
        ));

        let exit_button = div()
            .id("immersive-exit")
            .absolute()
            .top(px(14.0))
            .right(px(14.0))
            .p(px(8.0))
            .rounded(px(10.0))
            .opacity(0.55)
            .hover(|el| el.opacity(1.0).bg(Rgba::new(1.0, 1.0, 1.0, 0.10)))
            .on_click(|_: &ClickEvent, _, cx| set_immersive(false, cx))
            .tooltip(build_tooltip(tr!("IMMERSIVE_EXIT", "Exit Immersive Mode")))
            .child(icon(MINIMIZE).size(px(18.0)).text_color(text));

        let stage_center_view = if self.stage_mode == ImmersiveStageMode::KineticLyrics {
            let gold_glow = Rgba::new(1.0, 0.88, 0.45, 1.0);
            let dimmed_color = lerp_color(text, text_secondary, 0.45);

            let active_seed = self
                .current_line
                .and_then(|idx| self.parsed.as_ref().and_then(|p| p.get(idx)))
                .map_or(0, |l| l.time_ms);

            // 自由空间位移（根据乐句种子产生平缓随风漂浮的梦幻位置）
            let (space_hop_x, space_hop_y) = match (active_seed / 1000) % 6 {
                0 => (-60.0f32, -28.0f32),
                1 => (70.0f32, 20.0f32),
                2 => (-20.0f32, -15.0f32),
                3 => (48.0f32, -32.0f32),
                4 => (-50.0f32, 24.0f32),
                _ => (15.0f32, 8.0f32),
            };

            let kinetic_lyric_elements: smallvec::SmallVec<[AnyElement; 4]> = if let Some(parsed) =
                &self.parsed
            {
                let active_idx = self.current_line.unwrap_or(0);
                if let Some(line) = parsed.get(active_idx) {
                    let line_time_ms = line.time_ms;
                    let words_elements: smallvec::SmallVec<[AnyElement; 16]> =
                        if !line.words.is_empty() {
                            line.words
                                .iter()
                                .map(|word| {
                                    let progress = word_progress(word, position_ms);
                                    let color = lerp_color(dimmed_color, gold_glow, progress);

                                    // 梦幻逐字弹跳与浮动动画 (Dreamy Kinetic Pop-up)
                                    let (pop_y, scale_pt, halo_opacity) =
                                        if progress > 0.0 && progress < 1.0 {
                                            // 正在唱：弹起并伴随柔和回落 (-16px 高度)
                                            let curve = (progress * std::f32::consts::PI).sin();
                                            (-curve * 16.0, 62.0f32, 0.70 * curve)
                                        } else if progress >= 1.0 {
                                            // 已唱完：稳定高光与柔和微悬
                                            (-2.0, 58.0f32, 0.25)
                                        } else {
                                            // 未唱到：静静等待
                                            (0.0, 56.0f32, 0.0)
                                        };

                                    let is_active = progress > 0.02;

                                    div()
                                        .relative()
                                        .mt(px(pop_y))
                                        .when(halo_opacity > 0.05, |el| {
                                            el.child(
                                                div()
                                                    .absolute()
                                                    .top(px(-3.0))
                                                    .left(px(-3.0))
                                                    .opacity(halo_opacity)
                                                    .text_size(px(scale_pt))
                                                    .text_color(Rgba::new(1.0, 0.85, 0.45, 0.85))
                                                    .child(word.text.clone()),
                                            )
                                        })
                                        .child(
                                            div()
                                                .child(word.text.clone())
                                                .text_size(px(scale_pt))
                                                .text_color(color)
                                                .when(is_active, |s| {
                                                    s.font_weight(FontWeight::EXTRA_BOLD)
                                                }),
                                        )
                                        .into_any_element()
                                })
                                .collect()
                        } else {
                            line.text
                                .chars()
                                .enumerate()
                                .map(|(c_idx, ch)| {
                                    let c_seed = (active_seed / 50) + c_idx as u64 * 31;
                                    let scatter_y = (c_seed as f32 * 0.67).cos() * 6.0;
                                    div()
                                        .relative()
                                        .mt(px(scatter_y))
                                        .text_size(px(58.0))
                                        .text_color(gold_glow)
                                        .child({
                                            // SmolStr 对 1–4 字节字符内联，免掉每帧每字的
                                            // String 堆分配；SharedString::from(&mut str) 拥有副本。
                                            let mut encoded = [0u8; 4];
                                            SharedString::from(ch.encode_utf8(&mut encoded))
                                        })
                                        .into_any_element()
                                })
                                .collect()
                        };

                    let mut elements = smallvec::SmallVec::<[AnyElement; 4]>::new();
                    // 上一句淡出行（伴随在上方，30% 不透明度）
                    if active_idx > 0
                        && let Some(prev) = parsed.get(active_idx - 1)
                    {
                        elements.push(
                            div()
                                .mb(px(20.0))
                                .text_left()
                                .text_size(px(24.0))
                                .font_weight(FontWeight::MEDIUM)
                                .opacity(0.32)
                                .text_color(dimmed_color)
                                .child(prev.text.clone())
                                .into_any_element(),
                        );
                    }

                    // 统一而灵动优雅的主歌词容器（自然横排流动，字级梦幻弹跳）
                    elements.push(
                        div()
                            .id("stage-kinetic-hero-line")
                            .w_full()
                            .cursor_pointer()
                            .on_click(move |_, _, cx| {
                                let state = *cx.global::<PlaybackInfo>().playback_state.read(cx);
                                if state != PlaybackState::Stopped {
                                    cx.global::<PlaybackInterface>()
                                        .seek(line_time_ms as f64 / 1000.0);
                                }
                            })
                            .child(
                                div()
                                    .id("stage-kinetic-hero-horizontal")
                                    .w_full()
                                    .line_height(relative(1.22))
                                    .font_weight(FontWeight::BOLD)
                                    .flex()
                                    .flex_wrap()
                                    .justify_start()
                                    .items_baseline()
                                    .gap(px(4.0))
                                    .children(words_elements),
                            )
                            .into_any_element(),
                    );

                    // 伴随优雅译文
                    if let Some(trans) = &line.translation {
                        elements.push(
                            div()
                                .mt(px(14.0))
                                .text_left()
                                .text_size(px(20.0))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(lerp_color(text_secondary, gold_glow, 0.4))
                                .child(trans.clone())
                                .into_any_element(),
                        );
                    }

                    // 下一句微弱预读伴随行（38% 透明度，静静浮在下方引导视觉）
                    if let Some(next) = parsed.get(active_idx + 1) {
                        elements.push(
                            div()
                                .mt(px(24.0))
                                .text_left()
                                .text_size(px(26.0))
                                .font_weight(FontWeight::MEDIUM)
                                .opacity(0.38)
                                .text_color(dimmed_color)
                                .child(next.text.clone())
                                .into_any_element(),
                        );
                    }

                    elements
                } else {
                    smallvec::SmallVec::new()
                }
            } else {
                smallvec::SmallVec::new()
            };

            // 电影级多重空间几何罗盘、悬浮微粒与星轨极光装饰层
            let compass_canvas = canvas(
                move |bounds, _, _| bounds,
                move |bounds, _, window, _cx| {
                    let w = f32::from(bounds.size.width);
                    let h = f32::from(bounds.size.height);
                    let center_x = w * 0.38;
                    let center_y = h * 0.50;

                    let t_sec = position_ms as f32 / 1000.0;

                    // 1. 悬浮几何多边形与旋转虚线框 (Floating Polygons & Geometric Shards)
                    // 完美还原 Folia 心象/云阶经典深空悬浮图形元素
                    for (i, (base_x, base_y, r, sides, rot_speed, col)) in [
                        (
                            w * 0.16,
                            h * 0.28,
                            48.0f32,
                            4,
                            0.08f32,
                            Rgba::new(0.35, 0.55, 0.90, 0.065),
                        ),
                        (
                            w * 0.54,
                            h * 0.22,
                            58.0f32,
                            3,
                            -0.06f32,
                            Rgba::new(0.40, 0.60, 0.95, 0.075),
                        ),
                        (
                            w * 0.12,
                            h * 0.72,
                            62.0f32,
                            4,
                            0.05f32,
                            Rgba::new(0.50, 0.65, 0.98, 0.070),
                        ),
                        (
                            w * 0.46,
                            h * 0.68,
                            72.0f32,
                            4,
                            -0.04f32,
                            Rgba::new(0.30, 0.50, 0.85, 0.060),
                        ),
                        (
                            w * 0.62,
                            h * 0.52,
                            40.0f32,
                            3,
                            0.07f32,
                            Rgba::new(0.45, 0.65, 1.0, 0.055),
                        ),
                    ]
                    .iter()
                    .enumerate()
                    {
                        // 柔和浮游呼吸
                        let float_x = base_x + (t_sec * 0.5 + i as f32 * 2.1).sin() * 14.0;
                        let float_y = base_y + (t_sec * 0.4 + i as f32 * 1.7).cos() * 12.0;
                        let rot = t_sec * rot_speed;

                        // 填充半透明色块
                        let mut poly_fill = PathBuilder::fill();
                        for step in 0..*sides {
                            let angle = rot + (step as f32 / *sides as f32) * std::f32::consts::TAU;
                            let pt =
                                point(px(float_x + r * angle.cos()), px(float_y + r * angle.sin()));
                            if step == 0 {
                                poly_fill.move_to(pt);
                            } else {
                                poly_fill.line_to(pt);
                            }
                        }
                        if let Ok(p) = poly_fill.build() {
                            window.paint_path(p, *col);
                        }

                        // 轮廓描边
                        let mut poly_stroke = PathBuilder::stroke(px(1.0));
                        for step in 0..=*sides {
                            let angle = rot + (step as f32 / *sides as f32) * std::f32::consts::TAU;
                            let pt =
                                point(px(float_x + r * angle.cos()), px(float_y + r * angle.sin()));
                            if step == 0 {
                                poly_stroke.move_to(pt);
                            } else {
                                poly_stroke.line_to(pt);
                            }
                        }
                        let stroke_col = Rgba {
                            alpha: (col.alpha * 1.8).min(1.0),
                            ..*col
                        };
                        if let Ok(p) = poly_stroke.build() {
                            window.paint_path(p, stroke_col);
                        }
                    }

                    // 2. 空间律动十字十字星与悬浮方块微粒 (Cross Shards & Ambient Dust)
                    for (i, (rel_x, rel_y, sz)) in [
                        (0.24, 0.44, 16.0f32),
                        (0.48, 0.32, 12.0f32),
                        (0.28, 0.78, 20.0f32),
                        (0.58, 0.82, 14.0f32),
                    ]
                    .iter()
                    .enumerate()
                    {
                        let cross_x = w * rel_x + (t_sec * 0.3 + i as f32).cos() * 8.0;
                        let cross_y = h * rel_y + (t_sec * 0.3 + i as f32).sin() * 8.0;
                        let mut cross = PathBuilder::stroke(px(sz * 0.28));
                        cross.move_to(point(px(cross_x - sz * 0.5), px(cross_y)));
                        cross.line_to(point(px(cross_x + sz * 0.5), px(cross_y)));
                        cross.move_to(point(px(cross_x), px(cross_y - sz * 0.5)));
                        cross.line_to(point(px(cross_x), px(cross_y + sz * 0.5)));
                        if let Ok(p) = cross.build() {
                            window.paint_path(p, Rgba::new(0.35, 0.50, 0.85, 0.08));
                        }
                    }

                    // 3. 动态极光流光弧 (Fluid Aurora Ribbons)
                    let phase = t_sec * 0.4;
                    for (ribbon_idx, (y_off, amp, freq, col)) in [
                        (
                            -60.0f32,
                            45.0f32,
                            0.0035f32,
                            Rgba::new(0.35, 0.50, 0.95, 0.065),
                        ),
                        (
                            40.0f32,
                            55.0f32,
                            0.0028f32,
                            Rgba::new(0.85, 0.40, 0.70, 0.055),
                        ),
                        (
                            140.0f32,
                            40.0f32,
                            0.0042f32,
                            Rgba::new(1.0, 0.75, 0.30, 0.045),
                        ),
                    ]
                    .iter()
                    .enumerate()
                    {
                        let mut ribbon = PathBuilder::stroke(px(2.5));
                        let steps = 40;
                        for i in 0..=steps {
                            let x = (i as f32 / steps as f32) * w;
                            let y = (center_y + y_off)
                                + (x * freq + phase + (ribbon_idx as f32) * 1.5).sin() * amp
                                + (x * (freq * 0.5) - phase * 0.7).cos() * (amp * 0.4);
                            let pt = point(px(x), px(y));
                            if i == 0 {
                                ribbon.move_to(pt);
                            } else {
                                ribbon.line_to(pt);
                            }
                        }
                        if let Ok(p) = ribbon.build() {
                            window.paint_path(p, *col);
                        }
                    }

                    // 4. 细腻同心星轨圆 (Concentric Celestial Circles)
                    let compass_color = Rgba::new(1.0, 1.0, 1.0, 0.045);
                    for r in [160.0f32, 280.0f32, 420.0f32] {
                        let mut circle = PathBuilder::stroke(px(1.0));
                        for i in 0..=48 {
                            let angle = (i as f32 / 48.0) * std::f32::consts::TAU;
                            let pt = point(
                                px(center_x + r * angle.cos()),
                                px(center_y + r * angle.sin()),
                            );
                            if i == 0 {
                                circle.move_to(pt);
                            } else {
                                circle.line_to(pt);
                            }
                        }
                        if let Ok(path) = circle.build() {
                            window.paint_path(path, compass_color);
                        }
                    }

                    // 5. 极轴十字刻度线与坐标尺微标
                    let mut axes = PathBuilder::stroke(px(1.0));
                    axes.move_to(point(px(center_x - 480.0), px(center_y)));
                    axes.line_to(point(px(center_x + 480.0), px(center_y)));
                    axes.move_to(point(px(center_x), px(center_y - 320.0)));
                    axes.line_to(point(px(center_x), px(center_y + 320.0)));
                    if let Ok(path) = axes.build() {
                        window.paint_path(path, compass_color);
                    }

                    // 6. 象限四角星芒与星辉 (Cross Stars)
                    for (sx, sy, star_color) in [
                        (
                            center_x + 280.0,
                            center_y - 180.0,
                            Rgba::new(1.0, 0.88, 0.65, 0.26),
                        ),
                        (
                            center_x - 320.0,
                            center_y + 190.0,
                            Rgba::new(0.65, 0.85, 1.0, 0.24),
                        ),
                        (
                            center_x + 380.0,
                            center_y + 240.0,
                            Rgba::new(1.0, 0.65, 0.85, 0.20),
                        ),
                        (
                            center_x - 240.0,
                            center_y - 220.0,
                            Rgba::new(0.85, 1.0, 0.85, 0.20),
                        ),
                    ] {
                        let mut star = PathBuilder::stroke(px(1.2));
                        star.move_to(point(px(sx - 8.0), px(sy)));
                        star.line_to(point(px(sx + 8.0), px(sy)));
                        star.move_to(point(px(sx), px(sy - 8.0)));
                        star.line_to(point(px(sx), px(sy + 8.0)));
                        if let Ok(p) = star.build() {
                            window.paint_path(p, star_color);
                        }
                    }
                },
            );

            // 右侧中控感应区与手机式精致控制卡片（靠近右侧自动显现，移出自动隐藏）
            let right_console = div()
                .id("stage-right-hover-zone")
                .h_full()
                .w(px(380.0))
                .flex()
                .items_center()
                .justify_end()
                .pr(px(32.0))
                .on_hover(cx.listener(|this, is_hovering: &bool, _, cx| {
                    if this.console_hovered != *is_hovering {
                        this.console_hovered = *is_hovering;
                        cx.notify();
                    }
                }))
                .when(self.console_hovered, |zone| {
                    zone.child(
                        div()
                            .id("stage-console-pod")
                            .w(px(310.0))
                            .h(px(520.0))
                            .rounded(px(22.0))
                            .shadow_lg()
                            .border_1()
                            .border_color(Rgba::new(1.0, 1.0, 1.0, 0.16))
                            .bg(Rgba::new(0.06, 0.07, 0.09, 0.88))
                            .backdrop_blur(px(24.0))
                            .p(px(20.0))
                            .flex()
                            .flex_col()
                            .justify_between()
                            .child(
                                div()
                                    .relative()
                                    .w_full()
                                    .h(px(270.0))
                                    .rounded(px(16.0))
                                    .shadow_md()
                                    .border_1()
                                    .border_color(Rgba::new(1.0, 1.0, 1.0, 0.12))
                                    .overflow_hidden()
                                    .when_some(self.image_key.clone(), |el, key| {
                                        el.child(
                                            managed_image(("stage-console-art", image_gen), key)
                                                .thumb_max(512)
                                                .size_full()
                                                .object_fit(ObjectFit::Cover),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(3.0))
                                    .child(
                                        div()
                                            .text_size(px(17.0))
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(text)
                                            .truncate()
                                            .child(
                                                self.track_name
                                                    .clone()
                                                    .unwrap_or_else(|| tr!("APP_NAME").into()),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(6.0))
                                            .w_full()
                                            .child(
                                                div()
                                                    .text_size(px(12.0))
                                                    .text_color(text_secondary)
                                                    .truncate()
                                                    .child(
                                                        self.artist_name
                                                            .clone()
                                                            .unwrap_or_default(),
                                                    ),
                                            )
                                            .when(sample_rate > 0, |container| {
                                                let is_hires = sample_rate >= 88_200;
                                                let rate_khz = sample_rate as f32 / 1000.0;
                                                let badge_text = if is_hires {
                                                    format!("{rate_khz:.1}k Hi-Res")
                                                } else if sample_rate.is_multiple_of(1000) {
                                                    format!("{}k", sample_rate / 1000)
                                                } else {
                                                    format!("{rate_khz:.1}k")
                                                };
                                                let (badge_bg, badge_border, badge_text_col) =
                                                    if is_hires {
                                                        (
                                                            Rgba::new(0.95, 0.75, 0.30, 0.16),
                                                            Rgba::new(0.95, 0.75, 0.30, 0.38),
                                                            Rgba::new(1.0, 0.88, 0.45, 1.0),
                                                        )
                                                    } else {
                                                        (
                                                            Rgba::new(1.0, 1.0, 1.0, 0.08),
                                                            Rgba::new(1.0, 1.0, 1.0, 0.16),
                                                            text_secondary,
                                                        )
                                                    };
                                                container.child(
                                                    div()
                                                        .flex_shrink_0()
                                                        .px(px(6.0))
                                                        .py(px(1.5))
                                                        .rounded(px(4.0))
                                                        .bg(badge_bg)
                                                        .border_1()
                                                        .border_color(badge_border)
                                                        .shadow_sm()
                                                        .text_size(px(10.0))
                                                        .font_weight(FontWeight::BOLD)
                                                        .text_color(badge_text_col)
                                                        .child(badge_text),
                                                )
                                            }),
                                    ),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap(px(10.0))
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(8.0))
                                            .child(
                                                div()
                                                    .id("console-prev-btn")
                                                    .cursor_pointer()
                                                    .size(px(36.0))
                                                    .rounded_full()
                                                    .bg(Rgba::new(1.0, 1.0, 1.0, 0.15))
                                                    .border_1()
                                                    .border_color(Rgba::new(1.0, 1.0, 1.0, 0.25))
                                                    .hover(|s| s.bg(Rgba::new(1.0, 1.0, 1.0, 0.30)))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .child(
                                                        icon(PREV_TRACK).size(px(18.0)).text_color(
                                                            Rgba::new(1.0, 1.0, 1.0, 0.95),
                                                        ),
                                                    )
                                                    .on_click(|_, _, cx| {
                                                        cx.global::<PlaybackInterface>().previous();
                                                    }),
                                            )
                                            .child(
                                                div()
                                                    .id("console-play-btn")
                                                    .cursor_pointer()
                                                    .size(px(46.0))
                                                    .rounded_full()
                                                    .bg(theme.button_primary)
                                                    .text_color(theme.button_primary_text)
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .shadow_md()
                                                    .hover(|s| s.bg(theme.button_primary_hover))
                                                    .child(
                                                        icon(if playing { PAUSE } else { PLAY })
                                                            .size(px(22.0))
                                                            .text_color(theme.button_primary_text),
                                                    )
                                                    .on_click(|_, _, cx| {
                                                        let state = *cx
                                                            .global::<PlaybackInfo>()
                                                            .playback_state
                                                            .read(cx);
                                                        let interface =
                                                            cx.global::<PlaybackInterface>();
                                                        if state == PlaybackState::Playing {
                                                            interface.pause();
                                                        } else {
                                                            interface.play();
                                                        }
                                                    }),
                                            )
                                            .child(
                                                div()
                                                    .id("console-next-btn")
                                                    .cursor_pointer()
                                                    .size(px(36.0))
                                                    .rounded_full()
                                                    .bg(Rgba::new(1.0, 1.0, 1.0, 0.15))
                                                    .border_1()
                                                    .border_color(Rgba::new(1.0, 1.0, 1.0, 0.25))
                                                    .hover(|s| s.bg(Rgba::new(1.0, 1.0, 1.0, 0.30)))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .child(
                                                        icon(NEXT_TRACK).size(px(18.0)).text_color(
                                                            Rgba::new(1.0, 1.0, 1.0, 0.95),
                                                        ),
                                                    )
                                                    .on_click(|_, _, cx| {
                                                        cx.global::<PlaybackInterface>().next();
                                                    }),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(8.0))
                                            .child(
                                                div()
                                                    .id("console-poster-wall-btn")
                                                    .cursor_pointer()
                                                    .size(px(36.0))
                                                    .rounded_full()
                                                    .bg(Rgba::new(1.0, 1.0, 1.0, 0.12))
                                                    .border_1()
                                                    .border_color(Rgba::new(1.0, 1.0, 1.0, 0.22))
                                                    .hover(|s| s.bg(theme.button_primary))
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .shadow_sm()
                                                    .child(
                                                        icon(GRID).size(px(16.0)).text_color(
                                                            Rgba::new(1.0, 1.0, 1.0, 0.95),
                                                        ),
                                                    )
                                                    .tooltip(build_tooltip("返回海报墙"))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.stage_mode =
                                                            ImmersiveStageMode::PosterWall;
                                                        this.sync_spectrum_viewers();
                                                        cx.notify();
                                                    })),
                                            )
                                            .child(
                                                div()
                                                    .id("console-heart-btn")
                                                    .cursor_pointer()
                                                    .px(px(14.0))
                                                    .py(px(8.0))
                                                    .rounded(px(theme.radius_md))
                                                    .bg(Rgba::new(0.72, 0.12, 0.22, 0.95))
                                                    .border_1()
                                                    .border_color(Rgba::new(1.0, 0.40, 0.50, 0.35))
                                                    .hover(|s| {
                                                        s.bg(Rgba::new(0.85, 0.14, 0.26, 1.0))
                                                            .border_color(Rgba::new(
                                                                1.0, 0.50, 0.60, 0.60,
                                                            ))
                                                    })
                                                    .active(|s| {
                                                        s.bg(Rgba::new(0.60, 0.08, 0.16, 1.0))
                                                    })
                                                    .shadow_md()
                                                    .child(
                                                        icon(HEART_FILLED)
                                                            .size(px(16.0))
                                                            .text_color(Rgba::new(
                                                                1.0, 0.92, 0.95, 1.0,
                                                            )),
                                                    ),
                                            ),
                                    ),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(icon(VOLUME).size(px(13.0)).text_color(text_secondary))
                                    .child(
                                        slider()
                                            .id("console-vol-slider")
                                            .w_full()
                                            .h(px(4.0))
                                            .rounded_full()
                                            .value(volume as f32)
                                            .on_change(move |v, _, cx| {
                                                cx.global::<PlaybackInterface>()
                                                    .set_volume(v as f64);
                                            }),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.0))
                                            .text_color(text_secondary)
                                            .child(volume_text.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .text_xs()
                                    .text_color(text_secondary)
                                    .child(current_time_text.clone()),
                            ),
                    )
                });

            let lyrics_stream = div()
                .flex_1()
                .h_full()
                .flex()
                .flex_col()
                .justify_center()
                .pl(px((80.0f32 + space_hop_x).max(20.0)))
                .mt(px(space_hop_y))
                .pr(px(40.0))
                .child(
                    div()
                        .w_full()
                        .max_w(px(920.0))
                        .flex()
                        .flex_col()
                        .gap(px(18.0))
                        .children(kinetic_lyric_elements),
                );

            // 动态音乐律动频谱声浪 (Dancing Audio Spectrum Waves - Expanded Stage)
            let spectrum_bars = if let Some(spectrum) = &self.spectrum_data {
                let post = spectrum.read(cx).post.clone();
                let num_bars = 40;
                // 复用上一帧的 Vec 容量：稳态每帧不再分配柱元素缓冲。
                let mut bars = std::mem::take(&mut self.spectrum_bar_buffer);
                bars.clear();
                for b in 0..num_bars {
                    let frac = b as f32 / (num_bars - 1) as f32;
                    // 对数感知采样跨度：兼顾低音鼓点与高音细节
                    let sample_idx = ((frac.powf(1.25) * (post.len() as f32 - 1.0)).round()
                        as usize)
                        .min(post.len().saturating_sub(1));
                    let val_db = post.get(sample_idx).copied().unwrap_or(-60.0);

                    // 高频感度补偿（高频物理能量弱，加入 18dB 渐进斜率补偿让高频也能充分舞动）
                    let hf_boost = frac * 18.0;
                    let compensated_db = val_db + hf_boost;

                    // 高灵敏度动态窗口（-36dB ~ 0dB 黄金可听区间）
                    let norm = ((compensated_db + 36.0) / 36.0).clamp(0.0, 1.0);
                    // 平方根开方动力学提拉：显著提升中低能量时的起伏表现（告别小圆点），强拍直接爆发
                    let dynamic = norm.sqrt();
                    let bar_h = if playing {
                        (dynamic * 42.0).max(3.0)
                    } else {
                        2.5
                    };

                    let col = if b < 12 {
                        Rgba::new(1.0, 0.85, 0.40, 0.90) // 低频重低音：燃金色
                    } else if b < 28 {
                        Rgba::new(0.40, 0.85, 1.0, 0.85) // 中频人声旋律：荧光冰蓝
                    } else {
                        Rgba::new(0.85, 0.55, 1.0, 0.80) // 高频乐器泛音：梦幻紫
                    };

                    bars.push(
                        div()
                            .w(px(4.0))
                            .h(px(bar_h))
                            .rounded_full()
                            .bg(col)
                            .when(bar_h > 10.0, |el| el.shadow_sm())
                            .into_any_element(),
                    );
                }
                let element = div()
                    .flex()
                    .items_end()
                    .justify_center()
                    .gap(px(7.0))
                    .h(px(48.0))
                    .mb(px(10.0))
                    .children(bars.drain(..));
                self.spectrum_bar_buffer = bars;
                Some(element)
            } else {
                None
            };

            // 底部电影级极简悬浮光轨 (Minimal Floating Aurora Progress Bar - With Drag-seeking Slider)
            let floating_progress = div()
                .id("stage-floating-progress")
                .absolute()
                .bottom(px(26.0))
                .left_0()
                .right_0()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(8.0))
                .children(spectrum_bars)
                .child(
                    div().w(px(480.0)).child(
                        slider()
                            .id("stage-floating-slider")
                            .w_full()
                            .h(px(5.0))
                            .rounded_full()
                            .time_preview(duration_ms)
                            .change_interval(SLIDER_CHANGE_INTERVAL)
                            .value(if duration_ms > 0 {
                                (position_ms as f32 / duration_ms as f32).clamp(0.0, 1.0)
                            } else {
                                0.0
                            })
                            .on_change(move |v, _, cx| {
                                seek_to_fraction(v as f64, duration_ms, cx);
                            }),
                    ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(12.0))
                        .text_size(px(12.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(Rgba::new(1.0, 1.0, 1.0, 0.45))
                        .child(current_time_text)
                        .child("•")
                        .child(total_time_text),
                );

            Some(
                div()
                    .absolute()
                    .inset_0()
                    .child(compass_canvas)
                    .child(
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_between()
                            .pr(px(52.0))
                            .child(lyrics_stream)
                            .child(right_console),
                    )
                    .child(floating_progress),
            )
        } else {
            None
        };

        let mut root = div()
            .id("immersive-view")
            .key_context("Immersive")
            .track_focus(&self.focus_handle)
            .role(gpui::Role::Region)
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(Rgba::new(0.02, 0.02, 0.03, 1.0))
            .child(backdrop)
            .child(vertical_shade)
            .child(right_shade);

        match self.stage_mode {
            ImmersiveStageMode::PosterWall => {
                let viewport = window.viewport_size();
                let viewport_height = viewport.height.as_f32();
                let viewport_width_device = viewport.width.as_f32() * window.scale_factor();
                root =
                    root.child(self.render_poster_wall(viewport_height, viewport_width_device, cx));
            }
            _ => {
                if let Some(stage_view) = stage_center_view {
                    root = root.child(stage_view);
                }
            }
        }

        root.child(exit_button)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 卡片解码尺寸跟随实际设备像素宽度：1080p 用 320（而不是旧的固定 512），
    /// 4K 仍封顶 512，低分辨率不低于缩略图下限 256。
    #[test]
    fn poster_thumb_tracks_card_device_width() {
        assert_eq!(poster_thumb_size(1920.0), 320);
        assert_eq!(poster_thumb_size(1280.0), 256);
        assert_eq!(poster_thumb_size(3840.0), 512);
    }

    /// 越界偏移必须被钳到内容底部，且钳位后视口内不出现"没被实体化的空白带"……
    /// 这正是 2026-11-10 截图里顶部约 225px 空洞的成因（句柄偏移越界 ~865px，
    /// render 按越界值算窗口，而 gpui 到 layout 才钳、实际绘制在钳位后的位置）。
    #[test]
    fn poster_scroll_clamp_keeps_viewport_covered() {
        const LEN: usize = 60;
        const VIEWPORT: f32 = 1080.0;
        let content = poster_content_height(LEN, 0);
        let max_scroll = content - VIEWPORT;
        assert!(max_scroll > 0.0);

        // 范围内原样通过；越界一律钳到 [0, max_scroll]。
        assert_eq!(poster_scroll_top(-300.0, content, VIEWPORT), 300.0);
        assert_eq!(poster_scroll_top(0.0, content, VIEWPORT), 0.0);
        assert_eq!(
            poster_scroll_top(-(content + 5000.0), content, VIEWPORT),
            max_scroll
        );
        assert_eq!(
            poster_scroll_top(-100.0, VIEWPORT - 10.0, VIEWPORT),
            0.0,
            "内容比视口矮时只能停在顶部"
        );

        // 钳位后的窗口必须覆盖视口内每一张卡；用越界偏移直接算窗口则会漏卡。
        let covering = |top: f32| -> Vec<(usize, usize)> {
            (0..POSTER_COLUMNS)
                .flat_map(|col| {
                    poster_column_layout(
                        LEN,
                        col,
                        0,
                        top - POSTER_OVERSCAN,
                        top + VIEWPORT + POSTER_OVERSCAN,
                    )
                    .0
                    .into_iter()
                    .map(move |(idx, _, _)| (col, idx))
                })
                .collect()
        };
        let clamped = covering(poster_scroll_top(-(content + 5000.0), content, VIEWPORT));
        let raw = covering(content + 5000.0);
        let mut missing = 0;
        for col in 0..POSTER_COLUMNS {
            for (idx, y, h) in poster_column_layout(LEN, col, 0, f32::NEG_INFINITY, f32::INFINITY).0
            {
                if y + h >= max_scroll && y <= max_scroll + VIEWPORT {
                    assert!(
                        clamped.contains(&(col, idx)),
                        "col={col} idx={idx} 未被实体化"
                    );
                    if !raw.contains(&(col, idx)) {
                        missing += 1;
                    }
                }
            }
        }
        assert!(missing > 0, "越界偏移本应漏掉视口内的卡片，测试失去意义");
    }

    /// 自愈判定：首次大偏差触发一次；同一数值重复出现不再触发（防死循环）；
    /// 收敛（|diff|≤1px）永不触发；偏差演进后再次触发。
    #[test]
    fn poster_scroll_probe_heals_once_per_divergence() {
        let none = (i32::MIN, i32::MIN);
        let first = poster_scroll_should_heal(1000.0, 500.0, none).unwrap();
        assert_eq!(poster_scroll_should_heal(1000.0, 500.0, first), None);
        assert_eq!(poster_scroll_should_heal(500.0, 500.0, first), None);
        assert!(poster_scroll_should_heal(2000.0, 500.0, first).is_some());
    }

    /// 海报墙窗口化的三条不变量：任意滚动位置只实体化近邻卡片（内存上界）、
    /// 每列总高与整队列渲染一致（滚动范围不变）、所有下标都能在某个滚动位置
    /// 进入窗口（不会永久隐形）。
    #[test]
    fn poster_wall_window_materializes_bounded_slots() {
        const LEN: usize = 600;
        const VIEWPORT: f32 = 1080.0;
        let column_total =
            |col: usize| poster_column_layout(LEN, col, 0, f32::NEG_INFINITY, f32::INFINITY).1;
        let full_total = (0..POSTER_COLUMNS).map(column_total).fold(0.0f32, f32::max);
        assert!(full_total > 0.0);

        let mut seen = vec![false; LEN];
        let mut scroll_top = 0.0f32;
        while scroll_top <= full_total {
            let top = scroll_top - POSTER_OVERSCAN;
            let bottom = scroll_top + VIEWPORT + POSTER_OVERSCAN;
            let mut materialized = 0usize;
            for col in 0..POSTER_COLUMNS {
                let (slots, total) = poster_column_layout(LEN, col, 0, top, bottom);
                assert!((total - column_total(col)).abs() < 0.01);
                for (idx, y, h) in slots {
                    assert_eq!(idx % POSTER_COLUMNS, col);
                    assert!(y + h >= top && y <= bottom);
                    seen[idx] = true;
                    materialized += 1;
                }
            }
            assert!(
                materialized > 0 && materialized < LEN / 2,
                "scroll_top={scroll_top} materialized={materialized} must be bounded"
            );
            scroll_top += 512.0;
        }
        assert!(
            seen.iter().all(|&s| s),
            "every queue slot must enter the window"
        );
    }

    #[test]
    fn kinetic_char_text_paired_allocation_bench() {
        use crate::test_support::alloc_guard::count_allocations;

        const ROUNDS: usize = 1000;
        let ch = '晴';
        let (_, legacy_allocations) = count_allocations(|| {
            for _ in 0..ROUNDS {
                std::hint::black_box(ch.to_string());
            }
        });
        let (_, inline_allocations) = count_allocations(|| {
            for _ in 0..ROUNDS {
                let mut encoded = [0u8; 4];
                std::hint::black_box(SharedString::from(ch.encode_utf8(&mut encoded)));
            }
        });
        eprintln!(
            "kinetic char text allocations/{ROUNDS}: String={legacy_allocations} SharedString={inline_allocations}"
        );
        assert!(legacy_allocations >= ROUNDS as u64);
        assert_eq!(inline_allocations, 0);
    }

    #[test]
    fn stage_labels_and_spectrum_buffer_paired_allocation_bench() {
        use crate::test_support::alloc_guard::count_allocations;

        const ROUNDS: usize = 1000;
        let position_ms: u64 = 125_000;
        let duration_ms: u64 = 240_000;
        let volume = 0.63f64;

        // 旧形状：每帧 4 次 format!（控制台时间、音量、底栏当前/总时长）
        // + 一次 40 柱频谱 Vec 分配。
        let (_, legacy_allocations) = count_allocations(|| {
            for _ in 0..ROUNDS {
                let console_time = format!(
                    "{}:{:02}",
                    position_ms / 60000,
                    (position_ms % 60000) / 1000
                );
                let volume_text = format!("{}%", (volume * 100.0).round() as i64);
                let current = format!(
                    "{}:{:02}",
                    position_ms / 60000,
                    (position_ms % 60000) / 1000
                );
                let total = format!(
                    "{}:{:02}",
                    duration_ms / 60000,
                    (duration_ms % 60000) / 1000
                );
                let bars: Vec<u8> = Vec::with_capacity(40);
                std::hint::black_box((console_time, volume_text, current, total, bars));
            }
        });

        // 新形状：整秒/整数百分比缓存 + 频谱 Vec 容量复用。先预热到稳态。
        let mut labels: CachedTimeText<(SharedString, SharedString)> = CachedTimeText::unseeded();
        let mut cached_pct = (volume * 100.0).round() as i64;
        let mut cached_volume = SharedString::from(format!("{}%", cached_pct));
        let mut bar_buffer: Vec<u8> = Vec::with_capacity(40);
        labels.resolve(position_ms / 1000, duration_ms / 1000, |pos, dur| {
            (
                SharedString::from(format!("{}:{:02}", pos / 60, pos % 60)),
                SharedString::from(format!("{}:{:02}", dur / 60, dur % 60)),
            )
        });
        bar_buffer.extend(0..40_u8);

        let (_, reuse_allocations) = count_allocations(|| {
            for _ in 0..ROUNDS {
                let (current, total) =
                    labels.resolve(position_ms / 1000, duration_ms / 1000, |pos, dur| {
                        (
                            SharedString::from(format!("{}:{:02}", pos / 60, pos % 60)),
                            SharedString::from(format!("{}:{:02}", dur / 60, dur % 60)),
                        )
                    });
                let pct = (volume * 100.0).round() as i64;
                if cached_pct != pct {
                    cached_pct = pct;
                    cached_volume = SharedString::from(format!("{}%", pct));
                }
                let volume_text = cached_volume.clone();
                bar_buffer.clear();
                for bar in 0..40_u8 {
                    bar_buffer.push(bar);
                }
                std::hint::black_box((current, total, volume_text, &bar_buffer));
            }
        });
        eprintln!(
            "stage labels+spectrum allocations/{ROUNDS} frames: legacy={legacy_allocations} reuse={reuse_allocations}"
        );
        assert!(legacy_allocations >= 5 * ROUNDS as u64);
        assert_eq!(reuse_allocations, 0);
    }

    #[test]
    fn kinetic_element_vectors_paired_allocation_bench() {
        use crate::test_support::alloc_guard::count_allocations;

        const ROUNDS: usize = 1000;
        // 只量化容器这一层：AnyElement 自身的 Box 分配在生产路径里每个元素
        // 仍然存在，这里对比的是 Vec 头/堆缓冲的每帧开销。
        let (_, legacy_allocations) = count_allocations(|| {
            for _ in 0..ROUNDS {
                let words: Vec<u8> = (0..12_u8).collect();
                let lines: Vec<u8> = (0..3_u8).collect();
                std::hint::black_box((words, lines));
            }
        });
        let (_, inline_allocations) = count_allocations(|| {
            for _ in 0..ROUNDS {
                let words: smallvec::SmallVec<[u8; 16]> = (0..12_u8).collect();
                let lines: smallvec::SmallVec<[u8; 4]> = (0..3_u8).collect();
                std::hint::black_box((words, lines));
            }
        });
        eprintln!(
            "kinetic element containers allocations/{ROUNDS} frames: Vec={legacy_allocations} SmallVec={inline_allocations}"
        );
        assert_eq!(legacy_allocations, 2 * ROUNDS as u64);
        assert_eq!(inline_allocations, 0);
    }
}
