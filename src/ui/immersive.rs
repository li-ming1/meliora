//! Full-screen immersive listening view, laid out like a hi-fi lyrics
//! presentation: the artwork fills the screen as a sharp cover-band backdrop
//! (see `managed_image::backdrop_sharp_fill` — center-cropped to the window
//! aspect, median-denoised, Lanczos-resampled to the exact device-pixel size
//! and unsharp-masked; the KuGou ladder now leads with 4096 so the resample
//! averages noise like the disc label does; one vertical + one horizontal
//! gradient carve out the reading zones), a static disc-sized circular
//! cover (the "vinyl" no longer spins — see the animation notes below) sits
//! on the left above the track names and a frosted-glass player card
//! (seekable progress, transport, volume), and the right half is the lyrics
//! column with karaoke word timing, wheel browsing and click-to-seek.
//!
//! Lyrics are mirrored from the sidebar's [`Lyrics`] entity instead of being
//! loaded a second time: that entity owns the sidecar/DB/online fetch
//! pipeline (KuGou/NetEase included), its bounded cache and the scan-reset
//! logic. The mirror is generation-guarded so a re-notify never re-clones.
//!
//! Track presentation (cover, names, accent) is keyed off the queue position
//! — the single source of truth for "what is playing", as in InfoSection —
//! and re-resolved on both `SongChanged` and `QueuePositionChanged`, because
//! the former lands while the position still points at the previous track.
//!
//! Animation discipline (see `GPUI_HARDCORE_PERFORMANCE.md` and the
//! 2026-09-27 glyph-atlas lesson in `scroll_follow.rs`):
//! - every frame is data-driven (position broadcasts) or transition-driven
//!   (line glide), with the frame loop stopped the moment nothing animates
//!   any more;
//! - lyric glyph geometry never interpolates: font sizes come from a
//!   discrete set and line offsets snap to whole pixels, only colors lerp
//!   (karaoke word colors included);
//! - the "vinyl" is now just the circular cover art at disc size: the dark
//!   platter, groove rings and sheen went away across 2026-09-28 feedback
//!   (a rotating sheen arc rendered displaced/oversized on DirectX via
//!   `Svg::with_transformation`, and the exposed platter ring read as a
//!   pointless border around the cover);
//! - the backdrop is its own decode, rendered as a sharp cover-band to the
//!   exact window device-pixel size (held by the backdrop LRU, recycled
//!   through the orphan-tile funnel on track switch; the swap is a 400ms
//!   crossfade — the outgoing art keeps painting underneath until the fade
//!   ends). Sharp won the 2026-09-29 four-round bake-off with the user:
//!   blur-fill (three strength grades, with and without luma dimming) was
//!   rejected round by round — the directive is backdrop sharpness on par
//!   with the disc label, so the pipeline is center-crop → median denoise
//!   (kills JPEG block edges that 1:1-ish resampling would paint) → Lanczos
//!   → thresholded unsharp, and the KuGou ladder leads with 4096 so covers
//!   that have it resample ~0.47× (disc-grade averaging). Online covers walk
//!   the ladder (4096 → 2048 → 1280 → 480 → original) so a missing largest
//!   variant degrades gracefully instead of silently painting a 256px
//!   thumbnail across the screen.

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use cntp_i18n::tr;
use gpui::prelude::FluentBuilder;
use gpui::{
    App, AppContext, ClickEvent, Context, Div, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement, IntoElement, ObjectFit, ParentElement, Render, Rgba, ScrollWheelEvent,
    SharedString, Stateful, StatefulInteractiveElement, Styled, Subscription, Task, Window, div,
    linear_color_stop, linear_gradient, px, relative,
};
use tracing::warn;

use crate::{
    playback::{interface::PlaybackInterface, thread::PlaybackState},
    settings::SettingsGlobal,
    ui::{
        app::Pool,
        components::{
            icons::{MINIMIZE, NEXT_TRACK, PAUSE, PLAY, PREV_TRACK, VOLUME, VOLUME_OFF, icon},
            managed_image::{
                ImageCacheMode, ManagedImageKey, backdrop_cache_contains, backdrop_cache_shrink,
                backdrop_decode_allowed, managed_image,
            },
            slider::slider,
            tooltip::build_tooltip,
        },
        lyrics::{Lyrics, lerp_color, word_progress},
        models::{CurrentTrack, Models, PlaybackInfo, Queue},
        scroll_follow::ease_out_cubic,
        theme::Theme,
        util::{extract_accent, format_duration},
    },
};

/// Vertical distance between lyric lines, in px.
const LINE_PITCH_PX: f32 = 44.0;
/// Lyric font sizes — discrete glyph-atlas-safe levels only.
const CURRENT_LINE_SIZE: f32 = 22.0;
const NEIGHBOR_LINE_SIZE: f32 = 18.0;
const TRANSLATION_SIZE: f32 = 13.0;
/// Lyric lines rendered on either side of the active one (13-line window).
const LINE_WINDOW: i64 = 6;
/// Height of the lyric viewport: thirteen pitches.
const LYRICS_VIEWPORT_HEIGHT: f32 = LINE_PITCH_PX * 13.0;
/// Per-line opacity by distance from the active line (index = distance).
const LINE_OPACITY: [f32; 7] = [1.0, 0.82, 0.64, 0.47, 0.33, 0.22, 0.14];
const LINE_ANIMATION: Duration = Duration::from_millis(320);
/// Vinyl disc as a fraction of the left column height.
const VINYL_SIZE_FRACTION: f32 = 0.34;
/// Cover as a fraction of the disc diameter — 1.0: the cover IS the disc.
/// (The dark platter used to peek out around a smaller label; 2026-09-28
/// feedback: with the grooves gone that ring read as a pointless border.)
const VINYL_LABEL_FRACTION: f32 = 1.0;
/// Cover decode for the vinyl label — small, render-cached.
const LABEL_THUMB_PX: u32 = 512;
/// Backdrop decode cap for the fixed-size fallback path (`Backdrop(None)`,
/// see `backdrop_layer`): taken only when the device-pixel target is unknown.
const BACKDROP_THUMB_PX: u32 = 2048;
/// 大图回收队列未放行时的退避重试间隔：重试只是一次廉价的队列计数检查，
/// 期间旧背景保持绘制，不出现黑底。
const BACKDROP_RETRY: Duration = Duration::from_millis(500);
/// 背景切换 crossfade 时长。必须严格小于 managed_image 的
/// RECLAIM_DELAY_LARGE（3s）：淡出期间旧背景元素仍在绘制，其图集瓦片
/// 靠那条短年龄门兜底（门到时淡出早已结束、旧元素已卸载）。
const BACKDROP_FADE: Duration = Duration::from_millis(400);
/// Wheel browsing: after this long without scrolling the view glides back
/// to the active line (the sidebar's lyrics panel uses the same pattern).
const SCROLL_RETURN_AFTER: Duration = Duration::from_secs(2);

/// Enters or leaves immersive mode: flips [`Models::immersive`] and toggles
/// the main window's OS fullscreen state in step. Every entry point (keybind,
/// sidebar toggle, info-section double-click, exit button) funnels through
/// here.
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

pub struct ImmersiveView {
    focus_handle: FocusHandle,
    /// Mirrors `Models::immersive`; gates every observer's `notify`.
    active: bool,

    position: Entity<u64>,
    volume: Entity<f64>,
    prev_volume: Entity<f64>,
    playback_state: Entity<PlaybackState>,
    current_track: Entity<Option<CurrentTrack>>,
    queue: Entity<Queue>,
    /// The sidebar's lyrics model — shared so online fetches are reused.
    lyrics: Entity<Lyrics>,
    /// `Lyrics::parsed_generation` last mirrored into `parsed`.
    synced_lyrics_generation: u64,

    // Track presentation, re-resolved on track/queue-position changes.
    image_key: Option<ManagedImageKey>,
    image_element_key: u64,
    /// 当前渲染中的背景图键：解码完成时才晋升，与 `image_key`（封面/
    /// 取色的即时来源）解耦。晋升前旧背景一直绘制——没有黑底间隙。
    backdrop_key: Option<ManagedImageKey>,
    /// crossfade 双槽位：两个带显式 id 的固定图层（index 0/1）交替充当
    /// "当前背景"与"淡出层"。晋升时新图进入空闲槽，旧图原地不动——
    /// 其 ManagedImage 元素 id 与祖先路径全程不变，keyed state 不重建、
    /// 不重解码，淡出结束才清槽并把瓦片推进回收漏斗。
    backdrop_layers: [Option<(ManagedImageKey, u64)>; 2],
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
    /// 背景解码任务（未 detach：切歌/退出即取消；从未显示的背景不进
    /// 回收漏斗，零队列成本）。
    backdrop_task: Option<Task<()>>,
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
    accent: Option<Rgba>,
    /// Which cover the pending/existing accent was extracted from.
    accent_for: Option<ManagedImageKey>,
    /// `(queue position, current track path)` last resolved; both triggers
    /// fire per switch, the second one must be a no-op.
    resolved_signature: Option<(usize, Option<PathBuf>)>,

    // Lyrics, mirrored from the shared `Lyrics` entity.
    parsed: Option<Arc<Vec<crate::ui::lyrics::lrc::LrcLine>>>,
    current_line: Option<usize>,
    /// Wheel-browsing offset from the active line, in lines. Returns to 0
    /// after [`SCROLL_RETURN_AFTER`] of wheel idle (position ticks drive the
    /// check, so paused playback never resets the browse position).
    browse_offset: i64,
    /// Sub-line wheel accumulator (fractional lines between snaps).
    scroll_accum: f32,
    /// Last wheel interaction; `None` once the browse offset has returned.
    last_scroll: Option<Instant>,
    /// Animated display center = active line + browse offset, glided
    /// whenever either moves. Whole-pixel snapped per line at render time.
    visual_line: f32,
    line_anim: Option<(f32, f32, Instant)>,

    frame_scheduled: bool,
    /// Self-contained seek bar; observes position/duration itself so 30 Hz
    /// ticks repaint only this subtree instead of the fullscreen view.
    progress: Entity<ImmersiveProgress>,
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

            // Fullscreen toggling itself lives in `set_immersive` (it needs
            // the window handle); this observer only mirrors the flag into
            // the view's own state.
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
                // Wheel-browsing returns to the active line after the idle
                // window; position ticks drive the check so paused playback
                // never resets the browse position.
                if this.browse_offset != 0
                    && this
                        .last_scroll
                        .is_some_and(|at| at.elapsed() >= SCROLL_RETURN_AFTER)
                {
                    this.browse_offset = 0;
                    this.last_scroll = None;
                    this.sync_display_target(true);
                    needs_repaint = true;
                }
                // Karaoke word colors sweep per tick — but only while the
                // current line's last word is still in flight. Line-based
                // lyrics repaint only when the line moved (track_lyric_line).
                // The progress bar lives in its own entity and observes
                // position itself, so nothing else needs this 30 Hz path.
                if !needs_repaint
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

            cx.observe(&info.playback_state, |this, _, cx| {
                if this.active {
                    cx.notify();
                }
            })
            .detach();

            // Both per-switch triggers funnel into one guarded resolve:
            // SongChanged lands while the queue position still points at the
            // previous track, QueuePositionChanged completes the picture.
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

            // The Lyrics entity notifies unconditionally when a load (local
            // or online fetch) lands; the generation guard keeps re-notifies
            // from re-cloning the parsed lines.
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

            let progress = cx.new(|cx| {
                // The seek bar repaints on every position tick while the
                // fullscreen view around it stays put (see the position
                // observer above).
                cx.observe(&info.position, |_, _, cx| cx.notify()).detach();
                cx.observe(&info.duration, |_, _, cx| cx.notify()).detach();
                ImmersiveProgress {
                    position: info.position.clone(),
                    duration: info.duration.clone(),
                    // u64::MAX 永不匹配真实秒数：强制首建
                    time_labels: ImmersiveTimeLabels {
                        position_secs: u64::MAX,
                        duration_secs: u64::MAX,
                        elapsed_text: SharedString::default(),
                        total_text: SharedString::default(),
                    },
                }
            });

            let mut this = Self {
                focus_handle: cx.focus_handle(),
                active: *immersive_flag.read(cx),
                position: info.position.clone(),
                volume: info.volume.clone(),
                prev_volume: info.prev_volume.clone(),
                playback_state: info.playback_state.clone(),
                current_track: info.current_track.clone(),
                queue,
                lyrics,
                synced_lyrics_generation: 0,
                image_key: None,
                image_element_key: 0,
                track_name: None,
                artist_name: None,
                meta_subscription: None,
                accent: None,
                accent_for: None,
                resolved_signature: None,
                parsed: None,
                current_line: None,
                browse_offset: 0,
                scroll_accum: 0.0,
                last_scroll: None,
                visual_line: -1.0,
                line_anim: None,
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
                progress,
            };
            if this.active {
                this.activate(cx);
            }
            this
        })
    }

    /// `Models::immersive` flipped on/off while the view stays alive inside
    /// `MainWindow`.
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

    fn activate(&mut self, cx: &mut Context<Self>) {
        self.resolve_track_presentation(cx);
        // 重进沉浸页：resolve 可能因签名守卫早退（同一首歌），这里兜底
        // 重新布防背景 settle（退出时已失显/中止）。
        self.arm_backdrop(cx);
        self.sync_lyrics(cx);
        // 同首歌重进（resolve 早退）时 resolve 里的预取不会执行，这里补。
        self.prefetch_next_track_art(cx);
    }

    fn deactivate(&mut self) {
        self.line_anim = None;
        // 退出沉浸页：取消解码任务并立即失显。已显示背景随元素 unmount
        // 推进回收漏斗，短年龄门（大图 3s）后像素与瓦片放行；背景缓存
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
        backdrop_cache_shrink(1);
    }
    /// Re-resolves cover/names/accent for whatever is playing now. Fired by
    /// both `SongChanged` and `QueuePositionChanged`; the signature guard
    /// makes the second fire per switch a no-op.
    fn resolve_track_presentation(&mut self, cx: &mut Context<Self>) {
        let (position, item) = {
            let queue = self.queue.read(cx);
            let position = queue.position;
            let item = queue
                .data
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(position)
                .cloned();
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
        // A fresh track ends any wheel-browsing session.
        self.browse_offset = 0;
        self.last_scroll = None;
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
            // Online tracks carry a cover URL on their queue item; local
            // files fall back to embedded art / sidecar files.
            #[cfg(feature = "online_sources")]
            if let Some(url) = item
                .get_data(cx)
                .read(cx)
                .clone()
                .and_then(|data| data.cover_url)
                .filter(|url| !url.is_empty())
            {
                // Large-display variant: the queue carries 256px thumbnails,
                // which turn to mush across a fullscreen backdrop.
                cover_key = Some(ManagedImageKey::HttpCoverLarge(url));
            }
            if cover_key.is_none() {
                cover_key = Some(ManagedImageKey::TrackFile(item.get_path().clone()));
            }

            let data = item.get_data(cx);
            let item_path = item.get_path().clone();
            let is_current_slot = track_path.as_ref().is_some_and(|path| *path == item_path);
            // Names only from the slot that actually holds the playing track
            // — a stale slot (SongChanged before QueuePositionChanged) must
            // not latch, the position-change resolve corrects it instead.
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

        // Accent color: extracted from the label's small decode (render
        // cached), analyzed off-thread.
        if let Some(key) = self.image_key.clone()
            && self.accent_for.as_ref() != Some(&key)
        {
            self.accent = None;
            self.accent_for = Some(key.clone());
            let pool = cx.global::<Pool>().0.clone();
            let key_for_guard = key.clone();
            cx.spawn(async move |this, cx| {
                let accent = crate::RUNTIME
                    .spawn(async move {
                        key.retrieve(pool, LABEL_THUMB_PX, ImageCacheMode::RenderCache)
                            .await
                            .ok()
                            .flatten()
                            .and_then(|image| extract_accent(&image))
                    })
                    .await
                    .unwrap_or_else(|error| {
                        warn!(%error, "immersive: accent task failed");
                        None
                    });
                this.update(cx, |this, cx| {
                    if this.active && this.accent_for.as_ref() == Some(&key_for_guard) {
                        this.accent = accent;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
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
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(queue.position + 1)
                .cloned()?
        };
        #[cfg(feature = "online_sources")]
        if let Some(url) = slot
            .get_data(cx)
            .read(cx)
            .clone()
            .and_then(|data| data.cover_url)
            .filter(|url| !url.is_empty())
        {
            return Some(ManagedImageKey::HttpCoverLarge(url));
        }
        Some(ManagedImageKey::TrackFile(slot.get_path().clone()))
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
    /// 收队列未放行（>3 套在队）时按 [`BACKDROP_RETRY`] 退避重试，是
    /// "同时驻留大图套数"硬上界的执行点；大图 3s 短年龄门让积压在数秒
    /// 内排空，普通切歌节奏下阀门不再成为"背景不跟歌"的来源。
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
        self.backdrop_task = Some(cx.spawn(async move |this, cx| {
            let mut render_misses = 0u32;
            loop {
                // 缓存命中（预取/回跳）不产生新解码，直接绕过背压阀门：
                // 连跳时阀门按 3s/套排水，不能把已经渲染好的下一首也拖住
                // （"换了歌还是之前的图"的主因）。
                let cached = backdrop_cache_contains(&key, target_w, target_h);
                if !cached && !backdrop_decode_allowed() {
                    cx.background_executor().timer(BACKDROP_RETRY).await;
                    let still = this
                        .update(cx, |this, _| {
                            this.active && this.image_key.as_ref() == Some(&key)
                        })
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
                        .update(cx, |this, _| {
                            this.active && this.image_key.as_ref() == Some(&key)
                        })
                        .unwrap_or(false);
                    if !still {
                        return;
                    }
                    continue;
                };
                // 解码完成：曲目已又变则放弃晋升（成果留在背景缓存里，
                // 回跳零成本）。
                let promote = this
                    .update(cx, |this, _| {
                        this.active && this.image_key.as_ref() == Some(&key)
                    })
                    .unwrap_or(false);
                if !promote {
                    return;
                }
                this.update(cx, |this, cx| {
                    if this.image_key.as_ref() != Some(&key) {
                        return;
                    }
                    let reduced_motion = cx
                        .global::<SettingsGlobal>()
                        .model
                        .read(cx)
                        .interface
                        .reduced_motion;
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
                    this.backdrop_layers[incoming] = Some((key.clone(), this.backdrop_gen));
                    this.backdrop_key = Some(key);
                    cx.notify();
                })
                .ok();
                return;
            }
        }));
    }

    /// 渲染一个背景槽位图层：显式 id 让元素祖先路径跨帧稳定（槽位是否
    /// 有图、图是否更换都不影响另一个槽位的 keyed state）；opacity 由
    /// wrapper div 的 `opacity()` 设置，gpui 的 element_opacity 栈在绘制
    /// 时乘进子 `ManagedImage` 的精灵。纹理按 `backdrop_target` 设备尺寸
    /// 精确重采样，绘制时 GPU 1:1 取样。
    fn backdrop_layer(&self, index: usize, opacity: f32) -> impl IntoElement {
        let layer = self.backdrop_layers[index].clone();
        let target = self.backdrop_target;
        div()
            .id(("immersive-bg-layer", index as u64))
            .absolute()
            .inset_0()
            .opacity(opacity)
            .when_some(layer, move |el, (key, gen_id)| {
                let image = match target {
                    Some((w, h)) if w > 0 && h > 0 => {
                        managed_image(("immersive-bg", gen_id), key).backdrop_cached_target(w, h)
                    }
                    // 定长兜底（Backdrop(None)）：目标尺寸未知/为零时退回
                    // 2048 上限解码（render 总会先写入非零目标，正常不走）。
                    _ => managed_image(("immersive-bg", gen_id), key)
                        .thumb_max(BACKDROP_THUMB_PX)
                        .backdrop_cached(),
                };
                el.child(image.w_full().h_full().object_fit(ObjectFit::Cover))
            })
    }

    fn adopt_names(&mut self, data: crate::playback::queue::QueueItemUIData) {
        self.track_name = data.name.or(self.track_name.take());
        self.artist_name = data.artist_name.or(self.artist_name.take());
    }

    /// Mirrors the shared lyrics entity's parsed lines and snaps to the
    /// current line without gliding across the whole list.
    fn sync_lyrics(&mut self, cx: &mut Context<Self>) {
        self.synced_lyrics_generation = self.lyrics.read(cx).parsed_generation();
        // parsed_lines 返回的 Arc 与面板/缓存共享同一份行数据，镜像只付
        // 引用计数。
        self.parsed = self.lyrics.read(cx).parsed_lines();
        let pos_ms = *self.position.read(cx);
        self.current_line = None;
        self.line_anim = None;
        if let Some(parsed) = &self.parsed {
            let idx = parsed.partition_point(|line| line.time_ms <= pos_ms);
            self.current_line = if idx == 0 { None } else { Some(idx - 1) };
        }
        self.sync_display_target(false);
    }

    /// Returns whether the active line moved (i.e. the lyrics pane changed
    /// and a repaint is needed).
    fn track_lyric_line(&mut self, pos_ms: u64) -> bool {
        let Some(parsed) = &self.parsed else {
            return false;
        };
        let idx = parsed.partition_point(|line| line.time_ms <= pos_ms);
        let new_line = if idx == 0 { None } else { Some(idx - 1) };
        if new_line == self.current_line {
            return false;
        }
        // A jump of more than one line is a seek (click-to-seek, scrubber,
        // prev/next): the wheel-browse session ends so the window recenters
        // on the line the user jumped to. Natural playback steps by one.
        if let (Some(new), Some(old)) = (new_line, self.current_line)
            && new.abs_diff(old) > 1
        {
            self.browse_offset = 0;
            self.last_scroll = None;
        }
        self.current_line = new_line;
        self.sync_display_target(true);
        true
    }

    /// Recenters the lyric window on the active line plus the wheel-browse
    /// offset, gliding when the target moved.
    fn sync_display_target(&mut self, animate: bool) {
        let len = self.parsed.as_ref().map_or(0, |parsed| parsed.len() as i64);
        let base = self.current_line.map_or(-1, |line| line as i64);
        let min = -LINE_WINDOW - base;
        let max = len - 1 + LINE_WINDOW - base;
        self.browse_offset = self.browse_offset.clamp(min, max);
        let target = (base + self.browse_offset) as f32;
        if !animate || (target - self.visual_line).abs() < 0.01 {
            // 已在目标上，或调用方要求直接落位：不滑动并清掉进行中的动画。
            self.visual_line = target;
            self.line_anim = None;
        } else {
            self.line_anim = Some((self.visual_line, target, Instant::now()));
        }
    }

    fn needs_animation_frame(&self) -> bool {
        self.line_anim.is_some() || self.backdrop_fade_started.is_some()
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
        let reduced_motion = cx
            .global::<SettingsGlobal>()
            .model
            .read(cx)
            .interface
            .reduced_motion;
        if let Some((from, target, started_at)) = self.line_anim {
            if reduced_motion {
                self.visual_line = target;
                self.line_anim = None;
            } else {
                let progress = (started_at.elapsed().as_secs_f32() / LINE_ANIMATION.as_secs_f32())
                    .clamp(0.0, 1.0);
                self.visual_line = from + (target - from) * ease_out_cubic(progress);
                if progress >= 1.0 {
                    self.visual_line = target;
                    self.line_anim = None;
                }
            }
        }
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
}

/// 按 (整秒 position, 整秒 duration) 键缓存两个 "m:ss" 标签：文本每秒才变
/// 一次，而播放中 render 以 ~30Hz 运行（同 controls.rs Scrubber 的
/// TimeLabels 模式）。pad_minutes 保持 false（沉浸页 11px 紧凑风格）。
struct ImmersiveTimeLabels {
    position_secs: u64,
    duration_secs: u64,
    elapsed_text: SharedString,
    total_text: SharedString,
}

/// Self-contained seek bar for the immersive player card. It owns the 30 Hz
/// position observation so progress ticks repaint only this small subtree —
/// the fullscreen view's own position observer is gated on actual lyric-pane
/// changes (line moves, wheel-browse return, karaoke sweep in flight).
struct ImmersiveProgress {
    position: Entity<u64>,
    duration: Entity<u64>,
    time_labels: ImmersiveTimeLabels,
}

impl Render for ImmersiveProgress {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // PlaybackInfo positions and durations are both in milliseconds.
        let position_ms = *self.position.read(cx);
        let duration_ms = *self.duration.read(cx);
        let position_secs = position_ms / 1000;
        let duration_secs = duration_ms / 1000;
        let progress = if duration_ms > 0 {
            (position_ms as f32 / duration_ms as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let text_secondary = cx.global::<Theme>().text_secondary;

        // 整秒键未变即复用已格式化标签：30Hz 重渲染里绝大多数输出逐字节相同
        let (elapsed_text, total_text) = if self.time_labels.position_secs == position_secs
            && self.time_labels.duration_secs == duration_secs
        {
            (
                self.time_labels.elapsed_text.clone(),
                self.time_labels.total_text.clone(),
            )
        } else {
            let elapsed_text = SharedString::from(format_duration(position_secs as i64, false));
            let total_text =
                SharedString::from(format_duration((duration_secs as i64).max(0), false));
            self.time_labels = ImmersiveTimeLabels {
                position_secs,
                duration_secs,
                elapsed_text: elapsed_text.clone(),
                total_text: total_text.clone(),
            };
            (elapsed_text, total_text)
        };

        div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .w_full()
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(text_secondary)
                    .child(elapsed_text),
            )
            .child(
                slider()
                    .id("immersive-progress")
                    .flex_1()
                    .h(px(6.0))
                    .rounded_full()
                    .value(progress)
                    .on_change(move |v, _, cx| {
                        if duration_ms > 0 {
                            let state = *cx.global::<PlaybackInfo>().playback_state.read(cx);
                            if state != PlaybackState::Stopped {
                                cx.global::<PlaybackInterface>()
                                    .seek(v as f64 * duration_ms as f64 / 1000.0);
                            }
                        }
                    }),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(text_secondary)
                    .child(total_text),
            )
    }
}

/// The lyric lines inside the window around the animated display center:
/// `(line index, whole-pixel top offset, is_current)`. Pure so the clamp and
/// snap math is unit-testable. The highlighted line is the one actually
/// playing, even while the user browses away with the wheel.
fn lyric_window(
    visual_line: f32,
    parsed_len: usize,
    current_line: Option<usize>,
) -> smallvec::SmallVec<[(usize, f32, bool); 16]> {
    let center = visual_line.round();
    let len = parsed_len as i64;
    (-LINE_WINDOW..=LINE_WINDOW)
        .filter_map(move |offset| {
            let offset = offset as f32;
            let index = center + offset;
            if index < 0.0 || index >= len as f32 {
                return None;
            }
            let index = index as usize;
            let top = ((index as f32 - visual_line) + LINE_WINDOW as f32) * LINE_PITCH_PX;
            Some((index, top.round(), Some(index) == current_line))
        })
        .collect()
}

impl Render for ImmersiveView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Keyboard focus for the Esc binding (AboutDialog pattern; idempotent).
        self.focus_handle.focus(window, cx);

        let reduced_motion = cx
            .global::<SettingsGlobal>()
            .model
            .read(cx)
            .interface
            .reduced_motion;
        if self.needs_animation_frame() {
            if reduced_motion {
                self.advance_animations(window, cx);
            } else {
                self.schedule_frame(window, cx);
            }
        }

        let theme = cx.global::<Theme>();
        let text = theme.text;
        let text_secondary = theme.text_secondary;
        let accent = self.accent.unwrap_or(theme.text);
        let image_gen = self.image_element_key;

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

        // PlaybackInfo positions are in milliseconds. The progress bar reads
        // position/duration in its own entity (ImmersiveProgress).
        let position_ms = *self.position.read(cx);
        let playing = *self.playback_state.read(cx) == PlaybackState::Playing;
        let volume = *self.volume.read(cx);
        let prev_volume = *self.prev_volume.read(cx);

        // Backdrop: full-sharpness decode, Cover-fit, NO blur — sharpness is
        // the point; the gradients below carve the reading zones. The two
        // fixed layers render `backdrop_layers` (decode-ready slots), not
        // `image_key`: the view promotes a slot only once the decoded Arc is
        // in hand, so the previous art keeps painting until the swap — no
        // dark gap. The swap itself is a 400ms crossfade: the outgoing layer
        // keeps painting underneath at (1-p) while the incoming one fades in
        // at p (div opacity → gpui's element_opacity → PolychromeSprite
        // opacity).
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
        // Global weight: heavier at the bottom (card zone), light at the top
        // (0deg = to top: the 0% stop sits at the bottom edge).
        let vertical_shade = div().absolute().inset_0().bg(linear_gradient(
            0.0,
            linear_color_stop(Rgba::new(0.0, 0.0, 0.0, 0.42), 0.0),
            linear_color_stop(Rgba::new(0.0, 0.0, 0.0, 0.12), 1.0),
        ));
        // Right lyrics zone: transparent over the artwork, dark toward the
        // right edge (90deg = to right).
        let right_shade = div().absolute().inset_0().bg(linear_gradient(
            90.0,
            linear_color_stop(Rgba::new(0.0, 0.0, 0.0, 0.0), 0.30),
            linear_color_stop(Rgba::new(0.0, 0.0, 0.0, 0.52), 1.0),
        ));

        // ── Left column: circular cover, names, frosted player card ──────
        // The cover fills the whole disc circle (label fraction 1.0); the
        // dark layer behind it only shows through while the cover decodes or
        // when a track has no art at all. See module docs for why nothing
        // rotates and the grooves/sheen are gone.
        let vinyl_label = div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .when_some(self.image_key.clone(), |el, key| {
                el.child(
                    managed_image(("immersive-label", image_gen), key)
                        .thumb_max(LABEL_THUMB_PX)
                        .h(relative(VINYL_LABEL_FRACTION))
                        .aspect_square()
                        .rounded_full()
                        .object_fit(ObjectFit::Cover),
                )
            })
            .when_none(&self.image_key, |el| {
                el.child(
                    div()
                        .h(relative(VINYL_LABEL_FRACTION))
                        .aspect_square()
                        .rounded_full()
                        .bg(Rgba::new(1.0, 1.0, 1.0, 0.06)),
                )
            });
        let vinyl_disc = div()
            .absolute()
            .inset_0()
            .rounded_full()
            .bg(Rgba::new(0.045, 0.045, 0.058, 1.0));
        let vinyl = div()
            .relative()
            .h(relative(VINYL_SIZE_FRACTION))
            .aspect_square()
            .child(vinyl_disc)
            .child(vinyl_label);

        let track_info = div()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(5.0))
            .children(self.track_name.clone().map(|name| {
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_size(px(26.0))
                    .line_height(relative(1.15))
                    .text_color(text)
                    .text_center()
                    .child(name)
            }))
            .children(self.artist_name.clone().map(|artist| {
                div()
                    .text_size(px(15.0))
                    .text_color(text_secondary)
                    .text_center()
                    .child(artist)
            }));

        let transport_button =
            |id: &'static str, icon_path: &'static str, size: f32| -> Stateful<Div> {
                div()
                    .id(id)
                    .p(px(7.0))
                    .rounded(px(9.0))
                    .hover(|el| el.bg(Rgba::new(1.0, 1.0, 1.0, 0.10)))
                    .active(|el| el.bg(Rgba::new(1.0, 1.0, 1.0, 0.16)))
                    .child(icon(icon_path).size(px(size)).text_color(text))
            };
        let transport_row = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(
                transport_button("immersive-prev", PREV_TRACK, 18.0).on_click(|_, _, cx| {
                    cx.global::<PlaybackInterface>().previous();
                }),
            )
            .child(
                transport_button("immersive-play", if playing { PAUSE } else { PLAY }, 22.0)
                    .p(px(10.0))
                    .rounded_full()
                    .on_click(|_, _, cx| {
                        let state = *cx.global::<PlaybackInfo>().playback_state.read(cx);
                        let interface = cx.global::<PlaybackInterface>();
                        if state == PlaybackState::Playing {
                            interface.pause();
                        } else {
                            interface.play();
                        }
                    }),
            )
            .child(
                transport_button("immersive-next", NEXT_TRACK, 18.0).on_click(|_, _, cx| {
                    cx.global::<PlaybackInterface>().next();
                }),
            );
        let volume_group =
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    div()
                        .id("immersive-volume-btn")
                        .p(px(4.0))
                        .rounded(px(8.0))
                        .hover(|el| el.bg(Rgba::new(1.0, 1.0, 1.0, 0.10)))
                        .on_click(move |_, _, cx| {
                            let interface = cx.global::<PlaybackInterface>();
                            if volume <= 0.0 {
                                interface.set_volume(prev_volume);
                            } else {
                                interface.set_volume(0.0);
                            }
                        })
                        .child(
                            icon(if volume <= 0.0 { VOLUME_OFF } else { VOLUME })
                                .size(px(16.0))
                                .text_color(text_secondary),
                        ),
                )
                .child(
                    slider()
                        .id("immersive-volume")
                        .w(px(90.0))
                        .h(px(6.0))
                        .rounded_full()
                        .value(volume as f32)
                        .on_change(move |v, _, cx| {
                            cx.global::<PlaybackInterface>().set_volume(v as f64);
                        }),
                )
                .child(div().text_size(px(11.0)).text_color(text_secondary).child(
                    SharedString::from(format!("{}%", (volume * 100.0).round() as i64)),
                ));
        let progress_row = self.progress.clone();
        let player_card = div()
            .w(relative(0.90))
            .max_w(px(460.0))
            .rounded(px(16.0))
            .bg(Rgba::new(0.05, 0.05, 0.07, 0.62))
            .backdrop_blur(px(14.0))
            .border_1()
            .border_color(Rgba::new(1.0, 1.0, 1.0, 0.10))
            .p(px(18.0))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(progress_row)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(transport_row)
                    .child(volume_group),
            );

        let left_column = div()
            .absolute()
            .left_0()
            .top_0()
            .bottom_0()
            .w(relative(0.44))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(22.0))
            .px(px(40.0))
            .child(vinyl)
            .child(track_info)
            .child(player_card);

        // ── Right column: lyrics ─────────────────────────────────────────
        // Every line is click-to-seek; the wheel browses away from the
        // active line (returning after the idle window); the active line
        // renders word-by-word karaoke when the track carries per-word
        // timing. Colors lerp only — glyph geometry never interpolates.
        let lyric_children = self.parsed.as_ref().map(|parsed| {
            let visual_line = self.visual_line;
            let current_line = self.current_line;
            lyric_window(visual_line, parsed.len(), current_line)
                .into_iter()
                .map(move |(index, top, is_current)| {
                    let line = &parsed[index];
                    let distance = (index as f32 - visual_line).abs().round() as i64;
                    let opacity = LINE_OPACITY
                        .get(distance as usize)
                        .copied()
                        .unwrap_or(*LINE_OPACITY.last().unwrap());
                    let color = if is_current {
                        accent
                    } else {
                        lerp_color(text, text_secondary, 0.4)
                    };
                    let line_time_ms = line.time_ms;
                    // Karaoke: un-sung words sit at the dimmed neighbor tone
                    // and sweep to the accent as each word's window passes
                    // (the current line's own color is already the accent, so
                    // it cannot be the sweep start). Word colors change per
                    // tick — glyph geometry stays fixed.
                    let text_child: Div = if is_current && !line.words.is_empty() {
                        let unsung = lerp_color(text, text_secondary, 0.4);
                        let mut row = div().flex().flex_wrap().items_baseline();
                        for word in &line.words {
                            let progress = word_progress(word, position_ms);
                            row = row.child(
                                div()
                                    .child(word.text.clone())
                                    .text_color(lerp_color(unsung, accent, progress)),
                            );
                        }
                        row
                    } else {
                        div().child(line.text.clone())
                    };
                    div()
                        .id(index)
                        .cursor_pointer()
                        .hover(move |el| el.opacity(opacity.max(0.9)))
                        .on_click(move |_, _, cx| {
                            // Direct-closure handler (sidebar lyric rows use
                            // the same shape): the click only seeks — the
                            // wheel-browse session unwinds through the
                            // position-tick idle return.
                            let state = *cx.global::<PlaybackInfo>().playback_state.read(cx);
                            if state != PlaybackState::Stopped {
                                cx.global::<PlaybackInterface>()
                                    .seek(line_time_ms as f64 / 1000.0);
                            }
                        })
                        .absolute()
                        .left_0()
                        .right_0()
                        .top(px(top))
                        .h(px(LINE_PITCH_PX))
                        .flex()
                        .flex_col()
                        .items_start()
                        .justify_center()
                        .opacity(opacity)
                        .text_color(color)
                        .child(
                            text_child
                                .text_size(px(if is_current {
                                    CURRENT_LINE_SIZE
                                } else {
                                    NEIGHBOR_LINE_SIZE
                                }))
                                .line_height(relative(1.05))
                                .font_weight(if is_current {
                                    FontWeight::BOLD
                                } else {
                                    FontWeight::MEDIUM
                                })
                                .text_left(),
                        )
                        .when(is_current && line.translation.is_some(), |el| {
                            el.child(
                                div()
                                    .text_size(px(TRANSLATION_SIZE))
                                    .line_height(relative(1.05))
                                    .text_color(text_secondary)
                                    .child(line.translation.clone().unwrap()),
                            )
                        })
                })
        });
        let lyrics_viewport = div()
            .relative()
            .w_full()
            .h(px(LYRICS_VIEWPORT_HEIGHT))
            .overflow_hidden()
            .on_scroll_wheel(cx.listener(|this, ev: &ScrollWheelEvent, _, cx| {
                if !this.active {
                    return;
                }
                let dy = f32::from(ev.delta.pixel_delta(px(LINE_PITCH_PX)).y);
                // gpui 原样透传 Windows 滚轮符号：向下滚为负。歌词语义是
                // "向下滚 = 前进到更晚的行"（browse_offset 增大），故取反。
                this.scroll_accum -= dy / LINE_PITCH_PX;
                let whole = this.scroll_accum as i64;
                if whole != 0 {
                    this.scroll_accum -= whole as f32;
                    this.browse_offset += whole;
                    this.last_scroll = Some(Instant::now());
                    this.sync_display_target(true);
                    cx.notify();
                }
            }))
            .children(lyric_children.into_iter().flatten());
        let right_column = div()
            .absolute()
            .top_0()
            .bottom_0()
            .left(relative(0.46))
            .right_0()
            .flex()
            .flex_col()
            .justify_center()
            .pl(px(24.0))
            .pr(px(52.0))
            .child(lyrics_viewport);

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

        div()
            .key_context("Immersive")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(Rgba::new(0.02, 0.02, 0.03, 1.0))
            .child(backdrop)
            .child(vertical_shade)
            .child(right_shade)
            .child(left_column)
            .child(right_column)
            .child(exit_button)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lyric_window_centers_the_active_line() {
        let lines = lyric_window(10.0, 30, Some(10));
        assert_eq!(
            lines.iter().map(|(index, _, _)| *index).collect::<Vec<_>>(),
            (4..=16).collect::<Vec<_>>()
        );
        // The active line sits in the middle slot (index 6 of 13).
        let (_, top, is_current) = lines[6];
        assert!(is_current);
        assert_eq!(top, LINE_WINDOW as f32 * LINE_PITCH_PX);
    }

    #[test]
    fn lyric_window_clamps_to_parsed_range() {
        let lines = lyric_window(1.0, 3, Some(1));
        assert_eq!(
            lines.iter().map(|(index, _, _)| *index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        // Before the first line the window still starts at the clamp.
        let lines = lyric_window(-1.0, 3, None);
        assert_eq!(
            lines.iter().map(|(index, _, _)| *index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        // Browsing away: the highlight stays on the playing line even when
        // the window centers elsewhere.
        let lines = lyric_window(4.0, 30, Some(1));
        assert!(
            lines
                .iter()
                .any(|(index, _, current)| *index == 1 && *current)
        );
        assert!(
            !lines
                .iter()
                .any(|(index, _, current)| *index == 4 && *current)
        );
    }

    #[test]
    fn lyric_window_offsets_snap_to_whole_pixels() {
        for line in lyric_window(3.37, 30, Some(3)) {
            let (_, top, _) = line;
            assert_eq!(top, top.round());
        }
    }
}
