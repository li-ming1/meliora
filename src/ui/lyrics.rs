pub(crate) mod krc;
pub(crate) mod lrc;
pub(crate) mod yrc;

use lrc::{LrcLine, LrcWord, parse_lrc};

#[cfg(feature = "kugou")]
use crate::ui::kugou::{OnlineLyric, fetch_online_lyric};
#[cfg(feature = "netease")]
use crate::ui::netease::fetch_online_lyric as fetch_netease_lyric;

use crate::ui::design::ICON_MD;
use crate::{
    library::scan::ScanEvent,
    playback::{interface::PlaybackInterface, thread::PlaybackState},
    settings::SettingsGlobal,
    ui::{
        components::{
            icons::{MICROPHONE, icon},
            managed_image::{ImageCacheMode, ManagedImageKey},
            scrollbar::{ScrollableHandle, floating_scrollbar},
        },
        models::{Models, PlaybackInfo},
        scroll_follow::{SmoothScrollFollow, ease_out_cubic},
        theme::Theme,
        util::extract_accent,
    },
};
use cntp_i18n::tr;
use gpui::prelude::FluentBuilder;
use gpui::*;
use rustc_hash::FxHashMap;
#[cfg(any(feature = "kugou", feature = "netease"))]
use std::path::Path;
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

const LYRICS_FOLLOW_ANIMATION_DURATION: Duration = Duration::from_millis(180);
const LYRICS_ACTIVE_LINE_ANIMATION_DURATION: Duration = Duration::from_millis(180);
const LYRICS_USER_INTERACTION_TIMEOUT: Duration = Duration::from_secs(2);
const LYRICS_BASE_TEXT_SIZE: f32 = 22.0;
const LYRICS_ACTIVE_TEXT_SIZE: f32 = 25.0;
const LYRICS_BASE_VERTICAL_PADDING: f32 = 7.0;
const LYRICS_ACTIVE_VERTICAL_PADDING: f32 = 9.0;
const LYRICS_BASE_LINE_HEIGHT: f32 = 1.5;
const LYRICS_ACTIVE_LINE_HEIGHT: f32 = 1.65;
/// 上下渐隐遮罩高度：掩盖滚动边缘的硬裁切，越靠面板边缘越透明。
const LYRICS_FADE_MASK_HEIGHT: f32 = 56.0;
/// 按曲目歌词缓存容量（FIFO 淘汰）：覆盖来回切歌重访的最近曲目。
const LYRIC_CACHE_CAP: usize = 16;
/// 封面取色解码尺寸：256px 对饱和度加权主色已绰绰有余（约 2.1 万个采样
/// 点）。取色图从不绘制——`use_cache=false` 解码即弃，零 RENDER_CACHE
/// 条目、零图集瓦片，像素在提取任务结束时就地释放。
const ACCENT_THUMB_PX: u32 = 256;

pub struct Lyrics {
    content: Option<String>,
    /// Arc 共享：面板、`lyric_cache` 与沉浸页镜像引用同一份行数据，
    /// 传递全程只付引用计数（LrcLine 含逐字 Vec，深拷贝代价可观）。
    parsed: Option<Arc<Vec<LrcLine>>>,
    /// Bumped on every applied load (local, cached or online fetch): lets
    /// other consumers (the immersive view) mirror `parsed` without
    /// re-cloning it on unrelated notifies.
    parsed_generation: u64,
    last_active_line: Option<usize>,
    /// Bumped per track change; a background load only lands if its generation
    /// still matches, so fast switches never apply stale lyrics.
    load_generation: usize,
    /// Bounded FIFO cache of recently loaded lyrics keyed by track path: a
    /// back-and-forth track switch re-serves the cached lyric instead of
    /// re-reading the sidecar files + DB queries (or re-issuing the 1-3
    /// online requests). Cleared when a library scan completes (the only
    /// writer of DB lyrics). Online entries are keyed by stream URL, so a
    /// refreshed URL for the same song fetches once more.
    lyric_cache: FxHashMap<PathBuf, (Option<String>, Option<Arc<Vec<LrcLine>>>)>,
    /// Insertion order for `lyric_cache` FIFO eviction.
    lyric_cache_order: Vec<PathBuf>,
    /// Latest playback position snapshot (ms), refreshed by the position
    /// observer; drives per-word karaoke progress.
    position_ms: u64,
    scroll_handle: ScrollHandle,
    /// Virtualized variable-height list state for the LRC branch: only
    /// visible+overdraw lines are built and measured per frame (doctrine §10
    /// — off-screen lyrics must not pay render cost). Reset per track in
    /// `apply_loaded_lyrics`. The plain-text fallback branch keeps
    /// `scroll_handle` instead.
    list_state: ListState,
    follow_pending: bool,
    follow_frame_scheduled: bool,
    scroll_follow: SmoothScrollFollow,
    last_user_interaction_at: Option<Instant>,
    line_emphasis_start_values: Vec<f32>,
    line_emphasis_target_values: Vec<f32>,
    line_emphasis_started_at: Option<Instant>,
    playback_state: Entity<PlaybackState>,
    /// 每首歌主题色（与沉浸页同一提取器，取自当前封面）：活动行高亮与逐字
    /// 扫色的终点色。提取中或无封面时为 None，回退主题文本色。
    accent: Option<Rgba>,
    /// 当前 `accent` 对应的封面键；键未变化时跳过重复提取。
    accent_for: Option<ManagedImageKey>,
}

impl Lyrics {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let playback_info = cx.global::<PlaybackInfo>().clone();
            let current_track = playback_info.current_track.clone();
            let position = playback_info.position.clone();

            let initial_track = current_track.read(cx).clone();
            let initial_track_path = initial_track.as_ref().map(|t| t.get_path().clone());
            let pool = cx.global::<crate::ui::app::Pool>().0.clone();

            // Startup lyrics load mirrors the track-change path below: the
            // sidecar file read plus the two DB queries must not run on the
            // main thread while the window is being constructed. The view
            // starts in the empty state and the result lands via notify, with
            // the same generation guard so a fast track switch discards a
            // stale startup load.
            cx.spawn(async move |this, cx| {
                let loaded = Self::load_lyrics_via_runtime(pool, initial_track_path).await;

                this.update(cx, |this: &mut Self, cx| {
                    if this.load_generation != 0 {
                        return;
                    }
                    this.apply_loaded_lyrics(loaded.0, loaded.1, cx);
                })
                .ok();
            })
            .detach();

            cx.observe(&current_track, |this: &mut Lyrics, ct, cx| {
                let track = ct.read(cx).clone();
                // 每首歌主题色跟随曲目切换；放在所有提前 return 的分支之前，
                // 在线歌词路径同样生效。
                this.resolve_accent(cx);

                #[cfg(any(feature = "kugou", feature = "netease"))]
                if track
                    .as_ref()
                    .is_some_and(|t| crate::ui::availability::is_online_path(t.get_path()))
                {
                    // Invalidate in-flight local (sidecar/DB) loads: the
                    // online fetch below is guarded by the current-path
                    // check, but the local loads are guarded by this
                    // generation.
                    this.load_generation += 1;

                    // online stream: dispatch to the source that owns it. With
                    // both sources compiled in, each registry is consulted in
                    // turn, so a NetEase stream is never swallowed by the
                    // KuGou branch (and vice versa). An online path that
                    // matches neither registry falls through to the local
                    // lyrics handling below.
                    let path = track.as_ref().map(|t| t.get_path().as_path());
                    #[cfg(feature = "kugou")]
                    if path
                        .and_then(crate::ui::kugou::online_track_matching_path)
                        .is_some()
                    {
                        this.reset_track_state();
                        this.fetch_online_lyrics(path, cx);
                        return;
                    }
                    #[cfg(feature = "netease")]
                    if path
                        .and_then(crate::ui::netease::online_track_matching_path)
                        .is_some()
                    {
                        this.reset_track_state();
                        this.fetch_netease_online_lyrics(path, cx);
                        return;
                    }
                }

                // Clear synchronously so a fast switch never shows the previous
                // track's lines, then resolve the sidecar/DB off the main
                // thread - the sync path did a file read plus two DB queries
                // on the UI thread per track change.
                this.reset_track_state();
                this.last_user_interaction_at = None;
                this.load_generation += 1;
                let generation = this.load_generation;

                // Bounded per-track cache: a back-and-forth switch to a
                // recently played track re-serves the loaded lyric instead of
                // re-reading the sidecar files + DB queries.
                if let Some((content, parsed)) = track
                    .as_ref()
                    .and_then(|t| this.lyric_cache.get(t.get_path()).cloned())
                {
                    this.apply_loaded_lyrics(content, parsed, cx);
                    return;
                }

                let pool = cx.global::<crate::ui::app::Pool>().0.clone();
                let track_path = track.as_ref().map(|t| t.get_path().clone());
                let cache_key = track_path.clone();

                cx.spawn(async move |this, cx| {
                    let loaded = Self::load_lyrics_via_runtime(pool, track_path).await;

                    this.update(cx, |this, cx| {
                        if this.load_generation != generation {
                            return;
                        }
                        if let Some(path) = cache_key {
                            this.cache_lyrics(path, loaded.clone());
                        }
                        this.apply_loaded_lyrics(loaded.0, loaded.1, cx);
                    })
                    .ok();
                })
                .detach();
            })
            .detach();

            // 队列改写（在线曲目的 cover_url 晚到、position 变化）也重解析
            // 主题色；封面键未变化时 resolve_accent 直接返回，重复触发无成本。
            let queue = cx.global::<Models>().queue.clone();
            cx.observe(&queue, |this: &mut Lyrics, _, cx| {
                this.resolve_accent(cx);
            })
            .detach();

            cx.observe(&position, |this: &mut Lyrics, pos, cx| {
                let pos_ms = *pos.read(cx);
                this.position_ms = pos_ms;
                // 面板隐藏时实体仍存活（RightSidebar 持有），但 notify 只会
                // 白白调度一次整窗 draw（所有视图走缓存重放）。状态照常更新，
                // 重新显示时由下一次 render 的动画调度接管。
                let visible = *cx.global::<Models>().show_lyrics.read(cx);
                if let Some(parsed) = &this.parsed {
                    let idx = parsed.partition_point(|l| l.time_ms <= pos_ms);
                    let new_line = if idx == 0 { None } else { Some(idx - 1) };
                    if new_line != this.last_active_line {
                        // The deactivated line keeps its last measured height
                        // in the list's SumTree once it scrolls out of the
                        // overdraw band; remeasure it so the total height (and
                        // the scrollbar) doesn't drift by one emphasis delta.
                        if let Some(prev) = this.last_active_line {
                            this.list_state.remeasure_items(prev..prev + 1);
                        }
                        let reduced_motion = cx
                            .global::<SettingsGlobal>()
                            .model
                            .read(cx)
                            .interface
                            .reduced_motion;
                        this.start_line_emphasis_animation(new_line, reduced_motion);
                        this.last_active_line = new_line;
                        this.follow_pending = new_line.is_some();

                        if new_line.is_none() {
                            this.scroll_follow.cancel();
                        }

                        if visible {
                            cx.notify();
                        }
                    } else if visible
                        && this.last_active_line.is_some_and(|line| {
                            parsed.get(line).is_some_and(|l| !l.words.is_empty())
                        })
                    {
                        // 激活行带逐字歌词：position 每 ~33ms 广播一次，
                        // 需要按字进度持续重绘。
                        cx.notify();
                    }
                }
            })
            .detach();

            let playback_state = cx.global::<PlaybackInfo>().playback_state.clone();

            cx.observe(&playback_state, |this, state, cx| {
                if *state.read(cx) == PlaybackState::Playing {
                    this.register_user_interaction();
                }

                if *cx.global::<Models>().show_lyrics.read(cx) {
                    cx.notify();
                }
            })
            .detach();

            // A library scan is the only writer of DB lyrics: drop the
            // per-track lyric cache when one completes so re-scanned lyrics
            // are re-read instead of served stale from the cache (mirrors
            // the album-cache reset in `build_models`).
            let scan_state = cx.global::<Models>().scan_state.clone();
            cx.observe(&scan_state, |this, scan_event, cx| {
                if matches!(
                    scan_event.read(cx),
                    ScanEvent::ScanCompleteIdle
                        | ScanEvent::ScanCompleteWatching
                        | ScanEvent::TargetedRescanComplete
                ) {
                    this.lyric_cache.clear();
                    this.lyric_cache_order.clear();
                }
            })
            .detach();

            // Content starts in the empty state (same rendering as the
            // track-switch in-flight state); lyrics land off-thread above.
            let mut this = Self {
                content: None,
                parsed: None,
                load_generation: 0,
                parsed_generation: 0,
                lyric_cache: FxHashMap::default(),
                lyric_cache_order: Vec::new(),
                last_active_line: None,
                position_ms: *position.read(cx),
                scroll_handle: ScrollHandle::new(),
                list_state: ListState::new(0, ListAlignment::Top, px(400.0)),
                follow_pending: false,
                follow_frame_scheduled: false,
                scroll_follow: SmoothScrollFollow::new(LYRICS_FOLLOW_ANIMATION_DURATION),
                last_user_interaction_at: None,
                line_emphasis_start_values: Vec::new(),
                line_emphasis_target_values: Vec::new(),
                line_emphasis_started_at: None,
                playback_state,
                accent: None,
                accent_for: None,
            };
            this.resolve_accent(cx);
            this
        })
    }

    /// Parsed lyric lines of the current track, for consumers that render
    /// lyrics themselves (the immersive view mirrors this instead of loading
    /// a second copy — online fetches included). Arc-shared with `self.parsed`
    /// and the per-track cache: cloning is a refcount bump, no line data is
    /// copied. Pair with [`Self::parsed_generation`] to detect changes
    /// cheaply.
    pub(crate) fn parsed_lines(&self) -> Option<Arc<Vec<LrcLine>>> {
        self.parsed.clone()
    }

    /// Generation counter bumped on every applied load; see
    /// [`Self::parsed_lines`].
    pub(crate) fn parsed_generation(&self) -> u64 {
        self.parsed_generation
    }

    /// Loads lyrics for `path` off the main thread (startup and track-switch
    /// path): sidecar read via tokio fs, DB via the pool directly, no
    /// `block_on` on the main thread. Shared with the immersive view.
    pub(crate) async fn load_lyrics_off_thread(
        pool: &sqlx::SqlitePool,
        path: std::path::PathBuf,
    ) -> (Option<String>, Option<Arc<Vec<LrcLine>>>) {
        if let Some(stem) = path.file_stem() {
            let sidecar = path.with_file_name(format!("{}.krc", stem.to_string_lossy()));
            if let Ok(krc) = tokio::fs::read_to_string(&sidecar).await
                && let Some(parsed) = krc::parse_krc(&krc)
            {
                return (Some(krc), Some(Arc::new(parsed)));
            }
            let sidecar = path.with_file_name(format!("{}.yrc", stem.to_string_lossy()));
            if let Ok(yrc) = tokio::fs::read_to_string(&sidecar).await
                && let Some(parsed) = yrc::parse_yrc(&yrc)
            {
                return (Some(yrc), Some(Arc::new(parsed)));
            }
        }

        let content = match crate::library::db::get_track_by_path(pool, &path)
            .await
            .ok()
            .flatten()
        {
            Some(track) => crate::library::db::lyrics_for_track(pool, track.id)
                .await
                .ok()
                .flatten(),
            None => None,
        };
        let parsed = content.as_ref().and_then(|c| parse_lyrics(c)).map(Arc::new);
        (content, parsed)
    }

    /// Runs [`Self::load_lyrics_off_thread`] on the shared runtime and joins
    /// the result (shared by the startup and track-change paths). A `None`
    /// path (no current track) short-circuits to the empty pair, and so does
    /// a panicked or failed join — the UI then stays in the empty state.
    async fn load_lyrics_via_runtime(
        pool: sqlx::SqlitePool,
        path: Option<PathBuf>,
    ) -> (Option<String>, Option<Arc<Vec<LrcLine>>>) {
        crate::RUNTIME
            .spawn(async move {
                match path {
                    Some(path) => Self::load_lyrics_off_thread(&pool, path).await,
                    None => (None, None),
                }
            })
            .await
            .unwrap_or((None, None))
    }

    /// Resets lyric display state for a newly selected track.
    #[cfg_attr(not(any(feature = "kugou", feature = "netease")), allow(dead_code))]
    fn reset_track_state(&mut self) {
        self.last_active_line = None;
        self.position_ms = 0;
        self.follow_pending = false;
        self.scroll_follow.cancel();
        self.last_user_interaction_at = None;
        self.line_emphasis_started_at = None;
        self.line_emphasis_start_values.clear();
        self.line_emphasis_target_values.clear();
        self.scroll_handle.set_offset(gpui::Point {
            x: px(0.0),
            y: px(0.0),
        });
    }

    /// Applies a fully loaded lyric (cache hit or background load result) as
    /// the current track's lyric state and notifies the view.
    fn apply_loaded_lyrics(
        &mut self,
        content: Option<String>,
        parsed: Option<Arc<Vec<LrcLine>>>,
        cx: &mut Context<Self>,
    ) {
        self.content = content;
        self.parsed = parsed;
        self.parsed_generation += 1;
        let line_count = self.parsed.as_ref().map_or(0, |parsed| parsed.len());
        self.last_active_line = None;
        self.follow_pending = false;
        self.scroll_follow.cancel();
        self.line_emphasis_start_values = vec![0.0; line_count];
        self.line_emphasis_target_values = vec![0.0; line_count];
        // A track change is a fresh list: rebuilt with a uniform height hint
        // so the scrollbar is approximately right before the first paint
        // converges it. `reset` also returns the view to the top, which is
        // the pre-existing per-track scroll behavior.
        self.list_state
            .reset_with_uniform_height(line_count, px(60.0));
        if self.parsed.is_none() {
            // plain-text fallback branch still scrolls via `scroll_handle`
            self.scroll_handle.set_offset(gpui::Point {
                x: px(0.0),
                y: px(0.0),
            });
        }
        cx.notify();
    }

    /// Inserts a loaded lyric into the bounded FIFO cache, evicting the
    /// oldest entry past [`LYRIC_CACHE_CAP`].
    fn cache_lyrics(&mut self, path: PathBuf, loaded: (Option<String>, Option<Arc<Vec<LrcLine>>>)) {
        // 在线 URL 暂未进注册表时的未命中不缓存负结果：同 URL 稍后可能
        // 进注册表并取到歌词，负缓存会一直给出过期的"无歌词"。
        if loaded.0.is_none()
            && loaded.1.is_none()
            && crate::ui::availability::is_online_path(&path)
        {
            return;
        }
        if self.lyric_cache.insert(path.clone(), loaded).is_none() {
            self.lyric_cache_order.push(path);
            if self.lyric_cache_order.len() > LYRIC_CACHE_CAP {
                let oldest = self.lyric_cache_order.remove(0);
                self.lyric_cache.remove(&oldest);
            }
        }
    }

    /// Path string of the track the playback mirror says is playing now: the
    /// online fetch paths compare it against the path they were dispatched
    /// with, so a stale response for a track the user already left behind is
    /// dropped instead of overwriting the current lyric.
    #[cfg_attr(not(any(feature = "kugou", feature = "netease")), allow(dead_code))]
    fn current_track_path(cx: &App) -> Option<String> {
        cx.global::<PlaybackInfo>()
            .current_track
            .read(cx)
            .as_ref()
            .map(|t| t.get_path().to_string_lossy().into_owned())
    }

    /// 解析当前曲目的封面键并在封面变化时重提取每首歌主题色（与沉浸页
    /// 同一提取器）。取色图从不绘制：`use_cache=false` 解码即弃——零
    /// RENDER_CACHE 条目、零图集瓦片，256px 像素缓冲在提取任务结束时
    /// 就地释放；键未变化时直接返回，重复触发无成本。
    fn resolve_accent(&mut self, cx: &mut Context<Self>) {
        let queue = cx.global::<Models>().queue.clone();
        let item = {
            let queue = queue.read(cx);
            let position = queue.position;
            queue
                .data
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(position)
                .cloned()
        };
        let mut cover_key = None;
        if let Some(item) = &item {
            // 取色只需要小图：在线曲目用缩略图变体（而非沉浸页背景的
            // 大图变体），本地曲目读内嵌/sidecar 封面。
            #[cfg(feature = "online_sources")]
            if let Some(url) = item
                .get_data(cx)
                .read(cx)
                .clone()
                .and_then(|data| data.cover_url)
                .filter(|url| !url.is_empty())
            {
                cover_key = Some(ManagedImageKey::HttpCover(url));
            }
            if cover_key.is_none() {
                cover_key = Some(ManagedImageKey::TrackFile(item.get_path().clone()));
            }
        }
        if self.accent_for.as_ref() == cover_key.as_ref() {
            return;
        }
        // 换封面的瞬间先回到主题回退色，新主题色异步落地。
        self.accent = None;
        self.accent_for = cover_key.clone();
        let Some(key) = cover_key else {
            return;
        };

        let pool = cx.global::<crate::ui::app::Pool>().0.clone();
        let key_for_guard = key.clone();
        cx.spawn(async move |this, cx| {
            let accent = crate::RUNTIME
                .spawn(async move {
                    key.retrieve(pool, ACCENT_THUMB_PX, ImageCacheMode::None)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|image| extract_accent(&image))
                })
                .await
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "lyrics: accent task failed");
                    None
                });
            this.update(cx, |this, cx| {
                if this.accent_for.as_ref() == Some(&key_for_guard) {
                    this.accent = accent;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Applies a fetched online lyric to the current track and files it in
    /// the per-track cache. The caller has already confirmed via
    /// [`Self::current_track_path`] that the user hasn't switched tracks;
    /// `expected` is the path the fetch was started for.
    #[cfg_attr(not(any(feature = "kugou", feature = "netease")), allow(dead_code))]
    fn apply_online_lyrics(
        &mut self,
        expected: Option<&str>,
        loaded: (Option<String>, Option<Arc<Vec<LrcLine>>>),
        cx: &mut Context<Self>,
    ) {
        self.reset_track_state();
        self.apply_loaded_lyrics(loaded.0.clone(), loaded.1.clone(), cx);
        if let Some(key) = expected.map(PathBuf::from) {
            self.cache_lyrics(key, loaded);
        }
    }

    /// Fetches lyrics for the online (KuGou) track currently playing at `path`
    /// and swaps them in. Guarded so a stale response for a previous track
    /// doesn't overwrite the current one.
    #[cfg(feature = "kugou")]
    fn fetch_online_lyrics(&mut self, path: Option<&Path>, cx: &mut Context<Self>) {
        // Bounded per-track cache: a back-and-forth switch to a recently
        // played track skips the search + fetch network round-trips (the
        // stream-map lookup below only feeds that fetch).
        if let Some((content, parsed)) = path.and_then(|key| self.lyric_cache.get(key).cloned()) {
            self.apply_loaded_lyrics(content, parsed, cx);
            return;
        }

        let Some(track) = path.and_then(crate::ui::kugou::online_track_matching_path) else {
            return;
        };
        let track = track.clone();
        let expected = path.map(|p| p.to_string_lossy().into_owned());

        cx.spawn(async move |this, cx| {
            let lyric = fetch_online_lyric(&track).await.ok().flatten();

            // Parse off the UI thread: KRC is two full-text passes plus JSON
            // block deserialization, and the fetch above already runs on the
            // runtime so this just stays there instead of dropping back to
            // the main thread for the parse.
            let (parsed, content) = crate::RUNTIME
                .spawn(async move {
                    let parsed = match &lyric {
                        Some(OnlineLyric::Lrc(lrc)) => parse_lrc(lrc),
                        Some(OnlineLyric::Krc(krc)) => krc::parse_krc(krc),
                        None => None,
                    }
                    .map(Arc::new);
                    let content = lyric.as_ref().map(|lyric| lyric.describe());
                    (parsed, content)
                })
                .await
                .unwrap_or_default();

            this.update(cx, |this, cx| {
                // ignore the result if the user has already switched tracks
                if Self::current_track_path(cx) != expected {
                    return;
                }

                this.apply_online_lyrics(expected.as_deref(), (content, parsed), cx);
            })
            .ok();
        })
        .detach();
    }

    /// Fetches lyrics for the online (NetEase) track currently playing at
    /// `path` and swaps them in. Guarded so a stale response for a previous
    /// track doesn't overwrite the current one.
    #[cfg(feature = "netease")]
    fn fetch_netease_online_lyrics(&mut self, path: Option<&Path>, cx: &mut Context<Self>) {
        // Bounded per-track cache: a back-and-forth switch to a recently
        // played track skips the network request (the stream-map lookup
        // below only feeds that fetch).
        if let Some((content, parsed)) = path.and_then(|key| self.lyric_cache.get(key).cloned()) {
            self.apply_loaded_lyrics(content, parsed, cx);
            return;
        }

        let Some(track) = path.and_then(crate::ui::netease::online_track_matching_path) else {
            return;
        };
        let track = track.clone();
        let expected = path.map(|p| p.to_string_lossy().into_owned());

        cx.spawn(async move |this, cx| {
            let lyric = fetch_netease_lyric(&track).await.ok().flatten();

            this.update(cx, |this, cx| {
                // ignore the result if the user has already switched tracks
                if Self::current_track_path(cx) != expected {
                    return;
                }

                let parsed = lyric.as_ref().map(|lyric| Arc::new(lyric.lines.clone()));
                let content = lyric.as_ref().map(|lyric| lyric.content.clone());
                this.apply_online_lyrics(expected.as_deref(), (content, parsed), cx);
            })
            .ok();
        })
        .detach();
    }
}

impl Render for Lyrics {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();
        let queue = cx.global::<Models>().queue_width.read(cx).as_f32();
        let playback_state = *self.playback_state.read(cx);
        let reduced_motion = cx
            .global::<SettingsGlobal>()
            .model
            .read(cx)
            .interface
            .reduced_motion;

        let muted = theme.text_secondary;
        let normal = theme.text;

        // 遮罩必须与面板底色（window_chrome 根背景）一致才能无缝渐隐。
        let panel_bg = theme.background_primary;
        let panel_bg_transparent = Rgba {
            alpha: 0.0,
            ..panel_bg
        };
        let fade_masks = || {
            (
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .w_full()
                    .h(px(LYRICS_FADE_MASK_HEIGHT))
                    .bg(linear_gradient(
                        180.0,
                        linear_color_stop(panel_bg, 0.0),
                        linear_color_stop(panel_bg_transparent, 1.0),
                    )),
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .w_full()
                    .h(px(LYRICS_FADE_MASK_HEIGHT))
                    .bg(linear_gradient(
                        0.0,
                        linear_color_stop(panel_bg, 0.0),
                        linear_color_stop(panel_bg_transparent, 1.0),
                    )),
            )
        };

        if reduced_motion {
            if self.follow_pending || self.scroll_follow.is_active() || self.needs_animation_frame()
            {
                self.advance_animations(window, cx, true);
            }
        } else if self.needs_animation_frame() {
            self.schedule_follow_frame(window, cx);
        }

        let inner: AnyElement = if self.content.is_none() {
            div()
                .h_full()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .items_center()
                        .text_color(muted)
                        .child(icon(MICROPHONE).size(ICON_MD))
                        .child(tr!("NO_LYRICS", "No lyrics")),
                )
                .into_any_element()
        // LRC
        } else if self.parsed.is_some() {
            let lyrics = cx.entity().downgrade();
            let this_entity = cx.entity();
            let (fade_top, fade_bottom) = fade_masks();

            // Virtualized variable-height list: `render_item` runs only for
            // the visible + overdraw lines, so a full-track lyric no longer
            // rebuilds every row element at karaoke repaint rate (doctrine
            // §10). View state is read through the entity handle - by the
            // time the list lays items out (paint phase) the render lease on
            // this view is already released.
            let lyrics_list = list(self.list_state.clone(), move |idx, _window, cx| {
                let this = this_entity.read(cx);
                let Some(line) = this.parsed.as_ref().and_then(|parsed| parsed.get(idx)) else {
                    return div().into_any_element();
                };
                let time_ms = line.time_ms;
                if line.text.is_empty() {
                    // blank interlude lines are clickable too, so seeking to
                    // the start of an instrumental section works
                    div()
                        .id(("lyric", idx))
                        .on_click(move |_, _, cx| {
                            let interface = cx.global::<PlaybackInterface>();
                            interface.seek(time_ms as f64 / 1000_f64 + 0.1);
                        })
                        .cursor_pointer()
                        .h(px(16.0))
                        .w_full()
                        .into_any_element()
                } else {
                    let emphasis = this.line_emphasis_for(idx);
                    let is_active = emphasis > 0.0 || Some(idx) == this.last_active_line;
                    let highlight = this.accent.unwrap_or(normal);
                    let text_color = lerp_color(muted, highlight, emphasis);
                    // Geometry (font size, line height, padding) must move on
                    // a discrete grid: glyph atlas tiles are keyed by the
                    // exact f32 font size AND the glyph origin's fractional
                    // subpixel phase (RenderGlyphParams), and glyph tiles are
                    // never evicted. A continuous emphasis interpolation used
                    // to rasterize every intermediate size of every visible
                    // line onto fresh 4 MB subpixel pages that stayed resident
                    // forever — the 6.5 h session leak of 2026-09-27 (non_heap
                    // +2.5 MB/min while the lyrics panel was active, all of it
                    // glyph pages; same-day probe: 510 → 12 213 sub tiles in
                    // minutes). Quantized geometry repeats the same glyph
                    // origins, so the tile set stays bounded by the charset ×
                    // grid levels. Color, scroll timing and opacity still
                    // animate continuously.
                    let snapped_emphasis = (emphasis * 3.0).round() / 3.0;
                    let font_size = lerp(
                        LYRICS_BASE_TEXT_SIZE,
                        LYRICS_ACTIVE_TEXT_SIZE,
                        snapped_emphasis,
                    )
                    .round();
                    let width = (font_size / LYRICS_ACTIVE_TEXT_SIZE) * queue;

                    div()
                        .id(("lyric", idx))
                        .on_click(move |_, _, cx| {
                            let interface = cx.global::<PlaybackInterface>();
                            // add a small offset to make sure it goes to the next frame
                            interface.seek(time_ms as f64 / 1000_f64 + 0.1);
                        })
                        .cursor_pointer()
                        .max_w(px(width))
                        .overflow_x_hidden()
                        .px(px(20.0))
                        .py(px(lerp(
                            LYRICS_BASE_VERTICAL_PADDING,
                            LYRICS_ACTIVE_VERTICAL_PADDING,
                            snapped_emphasis,
                        )))
                        .text_size(px(font_size))
                        .line_height(rems(lerp(
                            LYRICS_BASE_LINE_HEIGHT,
                            LYRICS_ACTIVE_LINE_HEIGHT,
                            snapped_emphasis,
                        )))
                        .font_weight(if is_active {
                            FontWeight::EXTRA_BOLD
                        } else {
                            FontWeight::MEDIUM
                        })
                        .text_color(text_color)
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .child(if is_active && !line.words.is_empty() {
                                    // KRC 逐字卡拉OK：当前播放位置对应的字按
                                    // 完成进度从暗淡渐变到高亮，完成字全亮。
                                    div()
                                        .flex()
                                        .flex_wrap()
                                        .children(line.words.iter().map(|word| {
                                            let color = lerp_color(
                                                muted,
                                                highlight,
                                                word_progress(word, this.position_ms),
                                            );
                                            div().text_color(color).child(word.text.clone())
                                        }))
                                        .into_any_element()
                                } else {
                                    line.text.clone().into_any_element()
                                })
                                .when_some(line.translation.as_ref(), |d, translation| {
                                    d.child(
                                        div()
                                            .mt(px(2.0))
                                            .text_sm()
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(muted)
                                            .child(translation.clone()),
                                    )
                                }),
                        )
                        .into_any_element()
                }
            });

            div()
                .h_full()
                .w_full()
                .id("lyrics-scroll-container")
                .relative()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        if playback_state == PlaybackState::Playing {
                            this.register_user_interaction();
                        }
                        cx.notify();
                    }),
                )
                .on_scroll_wheel(cx.listener(move |this, _, _, cx| {
                    if playback_state == PlaybackState::Playing {
                        this.register_user_interaction();
                    }
                    cx.notify();
                }))
                .child(lyrics_list.flex().flex_col().w_full().h_full().py(px(9.0)))
                .child(fade_top)
                .child(fade_bottom)
                .child(
                    floating_scrollbar(
                        "lyrics-scrollbar",
                        ScrollableHandle::List {
                            state: self.list_state.clone(),
                        },
                    )
                    .right(px(4.0))
                    .on_interaction(move |_, cx| {
                        if let Some(lyrics) = lyrics.upgrade() {
                            lyrics.update(cx, |this, cx| {
                                this.register_user_interaction();
                                cx.notify();
                            });
                        }
                    }),
                )
                .into_any_element()
        } else {
            let text = self.content.clone().unwrap();
            let scroll_handle = self.scroll_handle.clone();
            let (fade_top, fade_bottom) = fade_masks();

            div()
                .h_full()
                .w_full()
                .relative()
                .child(
                    div()
                        .id("lyrics-plain-text")
                        .h_full()
                        .w_full()
                        .overflow_y_scroll()
                        .track_scroll(&scroll_handle)
                        .px(px(16.0))
                        .py(px(14.0))
                        .text_size(px(20.0))
                        .line_height(rems(1.6))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(normal)
                        .child(SharedString::from(text)),
                )
                .child(fade_top)
                .child(fade_bottom)
                .child(
                    floating_scrollbar(
                        "lyrics-plain-scrollbar",
                        ScrollableHandle::Regular(scroll_handle),
                    )
                    .right(px(4.0)),
                )
                .into_any_element()
        };

        div().h_full().w_full().flex().flex_col().child(inner)
    }
}

impl Lyrics {
    fn schedule_follow_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.follow_frame_scheduled {
            return;
        }

        self.follow_frame_scheduled = true;
        cx.on_next_frame(window, |this, window, cx| {
            this.follow_frame_scheduled = false;
            let reduced_motion = cx
                .global::<SettingsGlobal>()
                .model
                .read(cx)
                .interface
                .reduced_motion;
            this.advance_animations(window, cx, reduced_motion);
        });
    }

    fn advance_animations(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        reduced_motion: bool,
    ) {
        let mut changed = false;

        if self.has_recent_user_interaction() {
            self.scroll_follow.cancel();
        } else {
            changed |= self.advance_follow_animation(window, cx, reduced_motion);
        }

        changed |= self.advance_line_emphasis_animation(reduced_motion);

        if !reduced_motion && self.needs_animation_frame() {
            self.schedule_follow_frame(window, cx);
        }

        if changed {
            cx.notify();
        }
    }

    fn advance_follow_animation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        reduced_motion: bool,
    ) -> bool {
        if self.follow_pending {
            match self.compute_follow_target() {
                FollowTarget::PendingLayout => {
                    self.schedule_follow_frame(window, cx);
                    return false;
                }
                FollowTarget::NoScrollNeeded => {
                    self.follow_pending = false;
                    return false;
                }
                FollowTarget::Target(target_scroll_top) => {
                    let scroll_handle: ScrollableHandle = self.list_state.clone().into();
                    if reduced_motion {
                        self.scroll_follow
                            .jump_to(&scroll_handle, target_scroll_top);
                    } else {
                        self.scroll_follow
                            .animate_to(&scroll_handle, target_scroll_top);
                    }
                    self.follow_pending = false;
                }
            }
        }

        let scroll_handle: ScrollableHandle = self.list_state.clone().into();
        if reduced_motion {
            return self.scroll_follow.snap(&scroll_handle);
        }

        self.scroll_follow.advance(&scroll_handle)
    }

    fn compute_follow_target(&self) -> FollowTarget {
        let Some(active_line) = self.last_active_line else {
            return FollowTarget::NoScrollNeeded;
        };

        let viewport = self.list_state.viewport_bounds();
        if viewport.size.height <= px(0.0) {
            return FollowTarget::PendingLayout;
        }

        // `bounds_for_item` is None for lines above the scroll top, so a
        // newly active line the user has scrolled past would pend forever:
        // reveal it immediately instead of animating in from off-screen.
        let (Some(above), Some(below)) = (
            self.list_state.item_is_above_viewport(active_line),
            self.list_state.item_is_below_viewport(active_line),
        ) else {
            return FollowTarget::PendingLayout;
        };
        if above || below {
            // Jump most of the distance (reveal aligns the line to the
            // viewport edge), then keep following: once the line is inside
            // the viewport and measured, the next frame takes the smooth
            // path below and settles it exactly at the center. Settling for
            // `NoScrollNeeded` here would leave the active line pinned to
            // the edge forever - the next line would then be wholly outside
            // the viewport again and reveal right back.
            self.list_state.scroll_to_reveal_item(active_line);
            return FollowTarget::PendingLayout;
        }

        let Some(item_bounds) = self.list_state.bounds_for_item(active_line) else {
            return FollowTarget::PendingLayout;
        };

        let max_scroll_top = self.list_state.max_offset_for_scrollbar().y.max(px(0.0));
        let current_scroll_top = -self.list_state.scroll_px_offset_for_scrollbar().y;
        // ListState reports window coordinates, so the item's on-screen
        // position is already shifted by the current scroll: convert it back
        // to content space before aiming for the viewport center. (The old
        // ScrollHandle layout reported unscrolled content coordinates, which
        // is why the pre-virtualization formula had no current-scroll term.)
        let item_content_y = current_scroll_top + (item_bounds.origin.y - viewport.origin.y);
        let target_scroll_top = (item_content_y + item_bounds.size.height / 2.0
            - viewport.size.height / 2.0)
            .max(px(0.0))
            .min(max_scroll_top);

        if (target_scroll_top - current_scroll_top).abs() <= px(0.1) {
            FollowTarget::NoScrollNeeded
        } else {
            FollowTarget::Target(target_scroll_top)
        }
    }

    fn start_line_emphasis_animation(&mut self, active_line: Option<usize>, reduced_motion: bool) {
        let line_count = self.parsed.as_ref().map_or(0, |parsed| parsed.len());
        if self.line_emphasis_target_values.len() != line_count {
            self.line_emphasis_target_values = vec![0.0; line_count];
        }

        self.line_emphasis_start_values = (0..line_count)
            .map(|idx| self.line_emphasis_for(idx))
            .collect();

        self.line_emphasis_target_values.fill(0.0);
        if let Some(active_line) = active_line
            && active_line < self.line_emphasis_target_values.len()
        {
            self.line_emphasis_target_values[active_line] = 1.0;
        }

        let has_change = self.line_emphasis_has_change();

        if reduced_motion {
            self.line_emphasis_start_values = self.line_emphasis_target_values.clone();
            self.line_emphasis_started_at = None;
        } else {
            self.line_emphasis_started_at = has_change.then(Instant::now);
        }
    }

    fn advance_line_emphasis_animation(&mut self, reduced_motion: bool) -> bool {
        if reduced_motion {
            let changed =
                self.line_emphasis_has_change() || self.line_emphasis_started_at.is_some();
            self.line_emphasis_start_values = self.line_emphasis_target_values.clone();
            self.line_emphasis_started_at = None;
            return changed;
        }

        let Some(started_at) = self.line_emphasis_started_at else {
            return false;
        };

        if started_at.elapsed() < LYRICS_ACTIVE_LINE_ANIMATION_DURATION {
            return true;
        }

        self.line_emphasis_start_values = self.line_emphasis_target_values.clone();
        self.line_emphasis_started_at = None;
        true
    }

    /// Whether any line's emphasis start value still differs from its
    /// target: decides whether the frame loop must keep repainting.
    fn line_emphasis_has_change(&self) -> bool {
        self.line_emphasis_start_values
            .iter()
            .zip(self.line_emphasis_target_values.iter())
            .any(|(start, target)| (start - target).abs() > f32::EPSILON)
    }

    fn line_emphasis_for(&self, idx: usize) -> f32 {
        let target = self
            .line_emphasis_target_values
            .get(idx)
            .copied()
            .unwrap_or(0.0);
        let start = self
            .line_emphasis_start_values
            .get(idx)
            .copied()
            .unwrap_or(target);

        let Some(started_at) = self.line_emphasis_started_at else {
            return target;
        };

        let progress = (started_at.elapsed().as_secs_f32()
            / LYRICS_ACTIVE_LINE_ANIMATION_DURATION.as_secs_f32())
        .clamp(0.0, 1.0);
        let eased_progress = ease_out_cubic(progress);
        lerp(start, target, eased_progress)
    }

    fn register_user_interaction(&mut self) {
        self.last_user_interaction_at = Some(Instant::now());
        self.scroll_follow.cancel();
        self.follow_pending = self.last_active_line.is_some();
    }

    fn has_recent_user_interaction(&self) -> bool {
        self.last_user_interaction_at
            .is_some_and(|at| at.elapsed() < LYRICS_USER_INTERACTION_TIMEOUT)
    }

    // During the interaction window the wheel/click events drive the
    // repaints themselves and `scroll_follow` is cancelled, so a pending
    // re-follow must not keep the frame loop alive: it is consumed by the
    // first frame after the window expires (the next line change or
    // karaoke position tick), and counting it unconditionally here would
    // idle-spin the loop for the whole 2s window after every interaction.
    fn needs_animation_frame(&self) -> bool {
        self.line_emphasis_started_at.is_some()
            || (self.follow_pending && !self.has_recent_user_interaction())
            || self.scroll_follow.is_active()
    }
}

enum FollowTarget {
    PendingLayout,
    NoScrollNeeded,
    Target(Pixels),
}

/// 统一歌词解析：先按 LRC，再按 KRC / YRC 明文；无时间行返回 `None`。
pub(crate) fn parse_lyrics(content: &str) -> Option<Vec<LrcLine>> {
    parse_lrc(content)
        .or_else(|| krc::parse_krc(content))
        .or_else(|| yrc::parse_yrc(content))
}

fn lerp(start: f32, end: f32, progress: f32) -> f32 {
    start + (end - start) * progress
}

/// 单个字的卡拉OK 完成进度 0..1（按播放位置与字时间窗求交）。
pub(crate) fn word_progress(word: &LrcWord, pos_ms: u64) -> f32 {
    if pos_ms <= word.time_ms {
        0.0
    } else if pos_ms >= word.time_ms + word.duration_ms {
        1.0
    } else {
        (pos_ms - word.time_ms) as f32 / word.duration_ms.max(1) as f32
    }
}

pub(crate) fn lerp_color(start: Rgba, end: Rgba, progress: f32) -> Rgba {
    Rgba::new(
        lerp(start.red, end.red, progress),
        lerp(start.green, end.green, progress),
        lerp(start.blue, end.blue, progress),
        lerp(start.alpha, end.alpha, progress),
    )
}
