use std::{rc::Rc, sync::Arc, time::Duration};

use crate::ui::design::ICON_MD;
use cntp_i18n::tr;
use gpui::*;
use prelude::FluentBuilder;

use crate::{
    library::{
        db,
        types::{
            Album, DATE_PRECISION_FULL_DATE, DATE_PRECISION_YEAR, DATE_PRECISION_YEAR_MONTH,
            DBString, Track,
        },
    },
    playback::thread::PlaybackState,
    ui::{
        app::Pool,
        caching::meliora_cache,
        components::{
            button::{ButtonSize, button},
            icons::{DOTS_VERTICAL, STAR, STAR_FILLED, icon},
            playback_controls::playback_controls,
            popover::{PopoverPosition, popover},
            scrollbar::{ScrollableHandle, floating_scrollbar},
            table::table_data::capped_table_width,
            tooltip::build_tooltip,
        },
        library::{
            collection_summary::format_collection_summary,
            context_menus::{
                AlbumContextMenuContext, add_album_to_playlist_state, album::AlbumContextMenu,
                navigate_to_album_artists, queue_items_from_tracks,
            },
            detail_close_button,
            track_item::{ArtistNameVisibility, TrackItem, TrackItemLeftField, TrackItemOptions},
        },
        models::{
            LIKED_SONGS_PLAYLIST_ID, Models, PlaybackInfo, is_track_available_snapshot,
            toggle_album_like,
        },
        scroll_follow::SmoothScrollFollow,
        theme::Theme,
        util::reduced_motion_enabled,
    },
};

const RELEASE_SCROLL_ANIMATION_DURATION: Duration = Duration::from_millis(250);

/// 发行页头部里专辑封面盒子及其图像的固定尺寸。
const COVER_SIZE_PX: f32 = 160.0;

/// 后台加载落地前显示的占位：此刻只知道 id，所以头部渲染为空、曲目列表保持空白，
/// 其余字段等 `reload` 完成时填入。在我们脚下被删除的专辑（例如清理扫描）同样永远
/// 拿不到真实数据——它持续渲染这张空发行页，而不是把 UI 线程 panic 掉。
fn placeholder_album(album_id: i64) -> Album {
    Album {
        id: album_id,
        title: DBString::from(""),
        artist_display_override: None,
        release_date: None,
        date_precision: None,
        label: None,
        catalog_number: None,
        isrc: None,
        vinyl_numbering: false,
    }
}

fn compute_all_liked(cx: &App, tracks: &[Track]) -> bool {
    if tracks.is_empty() {
        return false;
    }
    // 缓存集合是异步落地的、本视图观察它，所以点赞/取消赞不再花一次 UI 线程上的
    // `block_on` 查询。
    let Some(liked) = cx.global::<Models>().liked_ids.read(cx).clone() else {
        return false;
    };
    tracks.iter().all(|track| liked.contains(&track.id))
}

/// 发行视图预先构造好的曲目行。
struct TrackListing {
    tracks: Arc<Vec<Entity<TrackItem>>>,
    original_tracks: Arc<Vec<Track>>,
}

impl TrackListing {
    fn new(
        cx: &mut App,
        tracks: Arc<Vec<Track>>,
        artist_name_visibility: ArtistNameVisibility,
        vinyl_numbering: bool,
        show_go_to_album: bool,
        show_go_to_artist: bool,
    ) -> Self {
        // 找出最大的曲目号并交给 track item 用于测量宽度
        let max_track_num_str = tracks
            .iter()
            .filter_map(|t| t.track_number)
            .max()
            .map(|n| format!("{}", n).into());

        Self {
            tracks: Arc::new({
                let tracks_for_closure = tracks.clone();
                tracks
                    .iter()
                    .enumerate()
                    .map(move |(index, track)| {
                        TrackItem::new(
                            cx,
                            track.clone(),
                            artist_name_visibility.clone(),
                            TrackItemLeftField::TrackNum,
                            None,
                            max_track_num_str.clone(),
                            None,
                            TrackItemOptions {
                                is_start: index == 0
                                    || track.track_number == Some(1)
                                    || tracks_for_closure
                                        .get(index - 1)
                                        .is_some_and(|t| t.disc_number != track.disc_number),
                                vinyl_numbering,
                                show_go_to_album,
                                show_go_to_artist,
                            },
                        )
                    })
                    .collect()
            }),
            original_tracks: tracks,
        }
    }

    fn tracks(&self) -> &Arc<Vec<Track>> {
        &self.original_tracks
    }

    /// 把曲目行作为惰性迭代器返回：调用方的 `.children(...)` 直接消费它，
    /// 避免每帧一次 `Vec<AnyElement>` 分配。
    fn track_elements(&self) -> impl Iterator<Item = AnyElement> + '_ {
        self.tracks
            .iter()
            .cloned()
            .map(|track| track.into_any_element())
    }
}

pub struct ReleaseView {
    album: Arc<Album>,
    artist_name: Option<DBString>,
    tracks: Arc<Vec<Track>>,
    /// 逐曲目的可用性，加载时算一次，而不是每帧 N 次 stat。
    tracks_available: Arc<Vec<bool>>,
    track_listing: TrackListing,
    /// 每次加载都自增，使晚于新一轮完成的慢加载不能覆盖它（与 `Table::reload_rows`
    /// 同一个守卫）。
    load_generation: u64,
    collection_summary: SharedString,
    release_info: Option<SharedString>,
    img_path: SharedString,
    scroll_handle: ScrollHandle,
    pending_scroll: Option<usize>,
    scroll_follow: SmoothScrollFollow,
    scroll_frame_scheduled: bool,
    all_liked: bool,
    /// 加载时算好并缓存：`tracks_available` 一旦建就不可变，于是头部不再每次渲染
    /// 都重扫一遍。
    has_available_tracks: bool,
    /// 当前播放的曲目是否属于本专辑。由 `PlaybackInfo::current_track` 推导；
    /// 在曲目变更时（下面的观察者）与加载时重算——绝不每帧重算。
    current_track_in_album: bool,
    menu_open: bool,
    track_ids: std::rc::Rc<[i64]>,
    /// 只解析一次；否则页脚每帧都要重新解析它。
    release_date_utc: Option<chrono::DateTime<chrono::Utc>>,
}

/// [`ReleaseView::render_header`] 的头部渲染标志位，打包成一个结构体，
/// 好让该方法不必收一堆位置参数 bool。
struct HeaderState {
    has_available_tracks: bool,
    current_track_in_album: bool,
    is_playing: bool,
    show_close_button: bool,
}

impl ReleaseView {
    pub(super) fn new(cx: &mut App, album_id: i64, target_track_id: Option<i64>) -> Entity<Self> {
        cx.new(|cx| {
            // 观察的是集合本身而不是歌单追踪器：追踪器在异步 reload 落地前就触发，
            // 那时读缓存会是陈旧的。
            let liked_ids = cx.global::<Models>().liked_ids.clone();
            cx.observe(&liked_ids, |this: &mut Self, _, cx| {
                let new_all_liked = compute_all_liked(cx, &this.tracks);
                if new_all_liked != this.all_liked {
                    this.all_liked = new_all_liked;
                    cx.notify();
                }
            })
            .detach();

            // 头部在当前曲目属于本专辑时高亮它。就在这里于曲目变更时重算（带 `!=` 守卫），
            // 于是 render 从不扫描曲目列表——也让高亮不再因曲目变更而失真，
            // 而这一件事本视图其他任何地方都没观察过。
            let current_track = cx.global::<PlaybackInfo>().current_track.clone();
            cx.observe(&current_track, |this: &mut Self, _, cx| {
                let in_album = this.compute_current_track_in_album(cx);
                if in_album != this.current_track_in_album {
                    this.current_track_in_album = in_album;
                    cx.notify();
                }
            })
            .detach();

            cx.on_release(|this: &mut Self, cx: &mut App| {
                ImageSource::Resource(Resource::Embedded(this.img_path.clone())).remove_asset(cx);
            })
            .detach();

            let placeholder = Arc::new(placeholder_album(album_id));
            let collection_summary = format_collection_summary(0, 0);

            let mut this = Self {
                album: placeholder,
                artist_name: None,
                tracks: Arc::new(Vec::new()),
                tracks_available: Arc::new(Vec::new()),
                track_listing: TrackListing::new(
                    cx,
                    Arc::new(Vec::new()),
                    ArtistNameVisibility::OnlyIfDifferent(None),
                    false,
                    false,
                    true,
                ),
                collection_summary,
                release_info: None,
                img_path: SharedString::from(format!("!db://album/{album_id}/full")),
                scroll_handle: ScrollHandle::new(),
                pending_scroll: None,
                scroll_follow: SmoothScrollFollow::new(RELEASE_SCROLL_ANIMATION_DURATION),
                scroll_frame_scheduled: false,
                all_liked: false,
                has_available_tracks: false,
                current_track_in_album: false,
                menu_open: false,
                track_ids: std::rc::Rc::from(Vec::new()),
                release_date_utc: None,
                load_generation: 0,
            };
            // 打开视图时在运行时上加载专辑头部与其曲目列表（见 reload）；
            // 落地前视图显示占位头部与空列表。
            this.reload(cx, target_track_id);
            this
        })
    }

    /// 在后台运行时上加载专辑头部及其曲目列表：打开视图过去会在「专辑行 + 完整曲目列表
    /// 查询（外加已点赞专辑归属检查）」上阻塞 UI 线程。加载落地前视图继续显示占位；
    /// 世代守卫丢弃被更新一轮加载取代的结果。
    fn reload(&mut self, cx: &mut Context<Self>, target_track_id: Option<i64>) {
        self.load_generation = self.load_generation.wrapping_add(1);
        let generation = self.load_generation;
        let album_id = self.album.id;
        let pool = cx.global::<Pool>().0.clone();

        cx.spawn(async move |this, cx| {
            let loaded = crate::RUNTIME
                .spawn(async move {
                    let album = db::get_album_by_id(&pool, album_id).await.ok();
                    let tracks = db::list_tracks_in_album(&pool, album_id)
                        .await
                        .unwrap_or_else(|_| Arc::new(Vec::new()));
                    // 与 `compute_all_liked` 完全相同的已点赞专辑检查（含它的空列表
                    // 提前返回），但挪出 UI 线程：这些 id 只在曲目列表加载完后才存在，
                    // 所以检查就搭在这里跑。
                    let all_liked = if tracks.is_empty() {
                        false
                    } else {
                        let ids: Vec<i64> = tracks.iter().map(|track| track.id).collect();
                        db::playlist_contains_all_tracks(&pool, LIKED_SONGS_PLAYLIST_ID, &ids)
                            .await
                            .unwrap_or(false)
                    };
                    (album, tracks, all_liked)
                })
                .await
                .unwrap_or((None, Arc::new(Vec::new()), false));

            let _ = this.update(cx, |this, cx| {
                // 更新的加载已取代这一轮：本结果陈旧。
                if this.load_generation != generation {
                    return;
                }
                let (album, tracks, all_liked) = loaded;
                this.apply_loaded(album, tracks, all_liked, target_track_id, cx);
            });
        })
        .detach();
    }

    /// 用一次后台加载的结果填掉占位。派生状态（行、可用性、滚动目标）在这里的
    /// UI 线程上重建——只有查询本身在线程外跑。
    fn apply_loaded(
        &mut self,
        album: Option<Arc<Album>>,
        tracks: Arc<Vec<Track>>,
        all_liked: bool,
        target_track_id: Option<i64>,
        cx: &mut Context<Self>,
    ) {
        // 专辑取回失败时保留占位：在我们脚下被删除的专辑（例如清理扫描）会渲染一张
        // 空发行页，而不是把 UI 线程 panic 掉。
        if let Some(album) = album {
            self.artist_name = album.artist_display_override.clone();
            self.release_info = match (&album.label, &album.catalog_number) {
                (Some(label), Some(catalog_number)) => Some(format!(
                    "{} • {}",
                    label.0.as_str(),
                    catalog_number.0.as_str()
                )),
                (Some(label), None) => Some(label.0.to_string()),
                (None, Some(catalog_number)) => Some(catalog_number.0.to_string()),
                (None, None) => None,
            }
            // 结果为空（例如厂牌为空白）时不渲染页脚行
            .filter(|info| !info.is_empty())
            .map(SharedString::from);
            self.release_date_utc = album.release_date.as_ref().and_then(|date| {
                chrono::NaiveDate::parse_from_str(date.0.as_str(), "%Y-%m-%d")
                    .ok()
                    .map(|nd| {
                        chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
                            nd.and_hms_opt(0, 0, 0).expect("date has zero time"),
                            chrono::Utc,
                        )
                    })
            });
            self.album = album;
        }
        self.tracks = tracks.clone();
        self.track_ids = tracks.iter().map(|track| track.id).collect();
        self.tracks_available = Arc::new(
            tracks
                .iter()
                .map(|track| is_track_available_snapshot(cx, track))
                .collect(),
        );
        self.track_listing = TrackListing::new(
            cx,
            tracks.clone(),
            ArtistNameVisibility::OnlyIfDifferent(self.artist_name.clone()),
            self.album.vinyl_numbering,
            false,
            true,
        );
        self.collection_summary = format_collection_summary(
            tracks.len() as i64,
            tracks.iter().map(|track| track.duration).sum(),
        );
        self.pending_scroll = target_track_id.and_then(|track_id| {
            tracks
                .iter()
                .position(|track| track.id == track_id && is_track_available_snapshot(cx, track))
        });
        self.all_liked = all_liked;
        // 派生的头部状态，每次加载算一次而不是每帧算。
        self.has_available_tracks = self.tracks_available.iter().any(|&a| a);
        self.current_track_in_album = self.compute_current_track_in_album(cx);

        cx.notify();
    }

    /// 当前播放曲目是否属于本专辑可用曲目之一：借用当前路径（不 clone），
    /// 并把每首曲目与它预先算好的可用性配对——没有每帧 stat，而且这次扫描只在
    /// 曲目变更 / 加载时跑，不在每次渲染跑。
    fn compute_current_track_in_album(&self, cx: &App) -> bool {
        cx.global::<PlaybackInfo>()
            .current_track
            .read(cx)
            .as_ref()
            .map(|current| current.get_path().as_path())
            .is_some_and(|current| {
                self.tracks
                    .iter()
                    .zip(self.tracks_available.iter())
                    .any(|(track, &available)| available && current == track.location.as_path())
            })
    }

    fn render_header(
        &self,
        theme: &Theme,
        state: HeaderState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let HeaderState {
            has_available_tracks,
            current_track_in_album,
            is_playing,
            show_close_button,
        } = state;

        div()
            .pt(px(52.0))
            .flex_shrink(1.0)
            .flex()
            .overflow_x_hidden()
            .px(px(18.0))
            .w_full()
            .relative()
            .when(show_close_button, |this| {
                this.child(detail_close_button("release_close"))
            })
            .child(
                div()
                    .rounded(px(theme.radius_lg))
                    .bg(theme.album_art_background)
                    .shadow_sm()
                    .w(px(COVER_SIZE_PX))
                    .h(px(COVER_SIZE_PX))
                    .flex_shrink_0()
                    .overflow_hidden()
                    .child(
                        img(self.img_path.clone())
                            .min_w(px(COVER_SIZE_PX))
                            .min_h(px(COVER_SIZE_PX))
                            .max_w(px(COVER_SIZE_PX))
                            .max_h(px(COVER_SIZE_PX))
                            .overflow_hidden()
                            .flex()
                            // TODO: 理想情况这里应是 ObjectFit::Cover，但那样会破坏圆角
                            // FIXME: 这是一个 GPUI bug
                            .object_fit(ObjectFit::Fill)
                            .rounded(px(theme.radius_lg)),
                    ),
            )
            .child(
                div()
                    .ml(px(18.0))
                    .mt_auto()
                    .flex_shrink(1.0)
                    .flex()
                    .flex_col()
                    .w_full()
                    .overflow_x_hidden()
                    .child(
                        div()
                            .id(("release_view_artist", self.album.id as usize))
                            .text_ellipsis()
                            .overflow_x_hidden()
                            .cursor_pointer()
                            .on_click({
                                let album_id = self.album.id;
                                move |ev, _, cx| {
                                    navigate_to_album_artists(cx, album_id, ev.position());
                                }
                            })
                            .when_some(self.artist_name.clone(), |this, artist| this.child(artist)),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::EXTRA_BOLD)
                            .text_size(rems(2.5))
                            .line_height(rems(2.75))
                            .overflow_x_hidden()
                            .pb(px(10.0))
                            .w_full()
                            .text_ellipsis()
                            .child(self.album.title.clone()),
                    )
                    .child(
                        div()
                            .pb(px(10.0))
                            .text_sm()
                            .text_color(theme.text_secondary)
                            .child(self.collection_summary.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.0))
                            .child(
                                playback_controls(
                                    "release",
                                    has_available_tracks,
                                    current_track_in_album,
                                    is_playing,
                                    {
                                        let tracks = self.track_listing.tracks().clone();
                                        move |_cx| queue_items_from_tracks(&tracks)
                                    },
                                )
                                .show_add_to_queue(false)
                                .trailing(self.render_menu_button(window, cx)),
                            )
                            .child(self.render_like_button(theme)),
                    ),
            )
    }

    fn render_like_button(&self, theme: &Theme) -> impl IntoElement {
        let all_liked = self.all_liked;
        let has_tracks = !self.tracks.is_empty();
        let track_ids = self.track_ids.clone();

        div()
            .id("release-like")
            .rounded_sm()
            .p(px(8.0))
            .when(has_tracks, |this| {
                this.cursor_pointer()
                    .hover(|this| this.bg(theme.button_secondary_hover))
                    .active(|this| this.bg(theme.button_secondary_active))
                    .tooltip(build_tooltip(if all_liked {
                        tr!("UNLIKE_ALBUM", "Unlike Album")
                    } else {
                        tr!("LIKE_ALBUM", "Like Album")
                    }))
                    .on_click(move |_, _, cx| {
                        toggle_album_like(track_ids.to_vec(), all_liked, cx);
                    })
            })
            .when(!has_tracks, |this| this.opacity(0.5))
            .child(
                icon(if all_liked { STAR_FILLED } else { STAR })
                    .size(ICON_MD)
                    .text_color(if all_liked {
                        theme.liked_song
                    } else {
                        theme.text_secondary
                    }),
            )
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        self.menu_open = false;
        cx.notify();
    }

    fn render_menu_button(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let menu_open = self.menu_open;

        let (show_add_to, add_to) =
            add_album_to_playlist_state("album-menu-state", self.album.id, window, cx);
        let menu_btn = div()
            .relative()
            .flex()
            .child(
                button()
                    .id("release-menu-button")
                    .size(ButtonSize::Large)
                    .flex_none()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();

                            this.menu_open = !menu_open;
                            cx.notify();
                        }),
                    )
                    .child(icon(DOTS_VERTICAL).size(ICON_MD).my_auto()),
            )
            .when(menu_open, |this| {
                let album = Rc::new((*self.album).clone());
                let weak_self = cx.entity().downgrade();
                let close = {
                    move |cx: &mut App| {
                        weak_self.update(cx, |this, cx| this.close_menu(cx)).ok();
                    }
                };
                let close_for_dismiss = close.clone();
                let close_for_out = close.clone();
                this.child(
                    popover()
                        .position(PopoverPosition::BottomRight)
                        .edge_offset(px(4.0))
                        .p(px(0.0))
                        .on_dismiss(move |_, cx| close_for_dismiss(cx))
                        .on_mouse_down_out(move |_, _, cx| close_for_out(cx))
                        .child(
                            div()
                                .id("release-menu-container")
                                .on_click(move |_, _, cx| close(cx))
                                .child(AlbumContextMenu::new(
                                    album,
                                    show_add_to,
                                    AlbumContextMenuContext::default(),
                                )),
                        ),
                )
            });

        div().child(menu_btn).child(add_to).into_any_element()
    }

    fn render_footer(&self, theme: &Theme) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .text_sm()
            .ml(px(18.0))
            .pt(px(12.0))
            .pb(px(12.0))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(theme.text_secondary)
            .when_some(self.release_info.clone(), |this, release_info| {
                this.child(div().child(release_info))
            })
            .when_some(
                self.album.date_precision,
                |this, precision| match precision {
                    DATE_PRECISION_FULL_DATE | DATE_PRECISION_YEAR_MONTH => {
                        if let Some(utc) = self.release_date_utc {
                            this.child(if precision == DATE_PRECISION_FULL_DATE {
                                tr!(
                                    "RELEASED_DATE",
                                    "Released {{date}}",
                                    date:date("YMD", length="long")=utc
                                )
                            } else {
                                tr!(
                                    "RELEASED_DATE",
                                    date:date("YM", length="long")=utc
                                )
                            })
                        } else {
                            this
                        }
                    }
                    DATE_PRECISION_YEAR => {
                        // release_date 缺失或短于 4 字符年份前缀（元数据畸形）时：
                        // 跳过发行年这一行，而不是让渲染过程 panic
                        let year = self
                            .album
                            .release_date
                            .as_ref()
                            .and_then(|date| date.0.get(..4));
                        match year {
                            Some(year) => {
                                this.child(tr!("RELEASED_YEAR", "Released {{year}}", year = year))
                            }
                            None => this,
                        }
                    }
                    _ => this,
                },
            )
            .when_some(self.album.isrc.as_ref(), |this, isrc| {
                this.child(div().child(isrc.clone()))
            })
    }

    fn schedule_scroll_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.scroll_frame_scheduled {
            return;
        }

        self.scroll_frame_scheduled = true;
        cx.on_next_frame(window, |this, window, cx| {
            this.scroll_frame_scheduled = false;
            let reduced_motion = reduced_motion_enabled(cx);
            this.advance_scroll_animation(window, cx, reduced_motion);
        });
    }

    fn advance_scroll_animation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        reduced_motion: bool,
    ) {
        if let Some(pending_scroll) = self.pending_scroll {
            match self.compute_follow_target(pending_scroll) {
                FollowTarget::PendingLayout => {
                    self.schedule_scroll_frame(window, cx);
                    return;
                }
                FollowTarget::NoScrollNeeded => {
                    self.pending_scroll = None;
                }
                FollowTarget::Target(target_scroll_top) => {
                    let scroll_handle: ScrollableHandle = self.scroll_handle.clone().into();
                    self.scroll_follow
                        .follow_to(&scroll_handle, target_scroll_top, reduced_motion);
                    self.pending_scroll = None;
                }
            }
        }

        let scroll_handle: ScrollableHandle = self.scroll_handle.clone().into();
        let changed = self
            .scroll_follow
            .advance_or_snap(&scroll_handle, reduced_motion);

        if self.scroll_follow.is_active() {
            self.schedule_scroll_frame(window, cx);
        }

        if changed {
            cx.notify();
        }
    }

    fn compute_follow_target(&self, track_index: usize) -> FollowTarget {
        let viewport = self.scroll_handle.bounds();
        if viewport.size.height <= px(0.0) {
            return FollowTarget::PendingLayout;
        }

        let Some(item_bounds) = self.scroll_handle.bounds_for_item(track_index + 1) else {
            return FollowTarget::PendingLayout;
        };

        let max_scroll_top = self.scroll_handle.max_offset().y.max(px(0.0));
        let raw_offset_y = viewport.origin.y - item_bounds.origin.y;
        let target_scroll_top = (-raw_offset_y).max(px(0.0)).min(max_scroll_top);
        let current_scroll_top = -self.scroll_handle.offset().y;

        if (target_scroll_top - current_scroll_top).abs() <= px(0.1) {
            FollowTarget::NoScrollNeeded
        } else {
            FollowTarget::Target(target_scroll_top)
        }
    }
}

#[derive(Clone, Copy)]
enum FollowTarget {
    PendingLayout,
    NoScrollNeeded,
    Target(Pixels),
}

impl Render for ReleaseView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = cx
            .global::<crate::settings::SettingsGlobal>()
            .model
            .read(cx);
        let reduced_motion = settings.interface.reduced_motion;
        let full_width = settings.interface.effective_full_width();
        let two_column = settings.interface.two_column_library;
        if self.pending_scroll.is_some() || self.scroll_follow.is_active() {
            if reduced_motion {
                // 减弱动效下仍需一趟来敲定待定布局，并把在途的滚动动画吸附到终值；
                // 只是之后不再安排新的动画帧。
                self.advance_scroll_animation(window, cx, reduced_motion);
            } else {
                self.schedule_scroll_frame(window, cx);
            }
        }

        let theme = cx.global::<Theme>().clone();

        let is_playing =
            cx.global::<PlaybackInfo>().playback_state.read(cx) == &PlaybackState::Playing;

        let scroll_handle = self.scroll_handle.clone();

        div()
            .image_cache(meliora_cache(("release", self.album.id as u64), 1))
            .flex()
            .flex_col()
            .w_full()
            .max_h_full()
            .relative()
            .overflow_hidden()
            .when(!full_width, capped_table_width)
            .child(
                div()
                    .id("release-view")
                    .overflow_y_scroll()
                    .track_scroll(&scroll_handle)
                    .w_full()
                    .flex_shrink(1.0)
                    .overflow_x_hidden()
                    .child(self.render_header(
                        &theme,
                        HeaderState {
                            has_available_tracks: self.has_available_tracks,
                            current_track_in_album: self.current_track_in_album,
                            is_playing,
                            show_close_button: two_column,
                        },
                        window,
                        cx,
                    ))
                    .children(self.track_listing.track_elements())
                    .when(
                        self.release_info.is_some()
                            || self.album.release_date.is_some()
                            || self.album.isrc.is_some(),
                        |this| this.child(self.render_footer(&theme)),
                    ),
            )
            .child(floating_scrollbar("release_scrollbar", scroll_handle).right(px(4.0)))
    }
}
