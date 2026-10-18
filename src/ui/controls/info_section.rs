use crate::sync::RecoveredRwLock;
#[cfg(feature = "kugou")]
use crate::ui::kugou::{
    KugouTrackInfo, like_track as kugou_like_track, online_track_is_liked,
    online_track_matching_path, unlike_track as kugou_unlike_track,
};
use crate::{
    library::types::Track,
    playback::{queue::QueueItemUIData, thread::PlaybackState},
    ui::{
        components::{
            context::context,
            icons::{STAR, STAR_FILLED, icon},
            managed_image::{ManagedImageKey, managed_image},
            tooltip::build_tooltip,
        },
        design::ICON_SM,
        library::context_menus::{
            info_section::InfoSectionContextMenu, navigate_to_track_album_and_reveal,
            navigate_to_track_artist,
        },
        models::{
            CurrentTrack, HasLikedState, LIKED_SONGS_PLAYLIST_ID, Models, PlaybackInfo,
            subscribe_liked_updates, toggle_like,
        },
        queue_metadata,
        theme::Theme,
    },
};
use cntp_i18n::tr;
use gpui::{InteractiveElement, prelude::FluentBuilder, *};
use std::{path::PathBuf, rc::Rc};

use super::observe_notify;

/// 当前正在播放的在线曲目（若有）：用运行中的流 URL 经逐音源的注册表反查回它的来源。
#[cfg(feature = "kugou")]
#[derive(Clone, Debug, PartialEq)]
enum OnlinePlayingTrack {
    Kugou(KugouTrackInfo),
}

#[cfg(feature = "kugou")]
impl OnlinePlayingTrack {
    /// 跨异步回调判断"还是同一首曲目"所用的身份键：酷狗 hash。
    fn identity(&self) -> String {
        match self {
            OnlinePlayingTrack::Kugou(track) => format!("kugou:{}", track.hash),
        }
    }

    fn matches_identity(&self, identity: &str) -> bool {
        self.identity() == identity
    }
}

pub struct InfoSection {
    track_name: Option<SharedString>,
    artist_name: Option<SharedString>,
    playback_info: PlaybackInfo,
    is_hovering_art: bool,
    current_track_path: Option<PathBuf>,
    current_library_track: Option<Rc<Track>>,
    can_navigate_to_album: bool,
    can_navigate_to_artist: bool,
    image_element_key: u64,
    is_liked: Option<i64>,
    /// 每次切歌递增；后台的音乐库解析只有在代号仍然匹配时才落地，所以快速切换
    /// 永远不会应用过期结果。
    library_resolve_generation: usize,
    #[cfg(feature = "kugou")]
    online_track: Option<OnlinePlayingTrack>,
    #[cfg(feature = "kugou")]
    is_online_liked: bool,
    queue_item_data: Option<Entity<Option<QueueItemUIData>>>,
    queue_item_subscription: Option<Subscription>,
}

impl HasLikedState for InfoSection {
    fn is_liked(&self) -> Option<i64> {
        self.is_liked
    }
    fn set_liked(&mut self, item_id: Option<i64>) {
        self.is_liked = item_id;
    }
}

impl InfoSection {
    pub fn new(cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let metadata_model = cx.global::<Models>().metadata.clone();
            let playback_info = cx.global::<PlaybackInfo>().clone();
            let current_track_model = playback_info.current_track.clone();
            let queue_model = cx.global::<Models>().queue.clone();

            observe_notify(cx, &playback_info.playback_state);

            cx.observe(&metadata_model, |this: &mut Self, m, cx| {
                this.update_track_metadata(m.read(cx));
                cx.notify();
            })
            .detach();

            // SongChanged 先于 QueuePositionChanged 广播，所以在队列位置跟上这次切歌
            // 之后要重新解析一次
            cx.observe(&queue_model, |this: &mut Self, _, cx| {
                this.resolve_queue_item_metadata(cx);
            })
            .detach();

            cx.observe(
                &current_track_model,
                |this: &mut Self, current_track, cx| {
                    let current_track = current_track.read(cx).clone();
                    this.update_current_track_state(current_track.as_ref(), cx);
                    #[cfg(feature = "kugou")]
                    this.detect_online_track();
                    #[cfg(feature = "kugou")]
                    this.schedule_online_liked_query(cx);
                    this.resolve_queue_item_metadata(cx);
                    cx.notify();
                },
            )
            .detach();

            let initial_current_track = current_track_model.read(cx).clone();
            let current_track_path = initial_current_track
                .as_ref()
                .map(|track| track.get_path().clone());
            let initial_metadata = metadata_model.read(cx).clone();

            subscribe_liked_updates(cx, |this: &Self| {
                this.current_library_track.as_ref().map(|t| t.id)
            });

            let mut info_section = Self {
                artist_name: None,
                track_name: None,
                playback_info,
                is_hovering_art: false,
                current_track_path,
                // 由音乐库派生的状态（曲目行、艺术家可否导航、喜欢状态）在下方线程外
                // 解析；本区一开始只用已经知道的数据。
                current_library_track: None,
                can_navigate_to_album: false,
                can_navigate_to_artist: false,
                image_element_key: 0,
                is_liked: None,
                library_resolve_generation: 0,
                #[cfg(feature = "kugou")]
                online_track: None,
                #[cfg(feature = "kugou")]
                is_online_liked: false,
                queue_item_data: None,
                queue_item_subscription: None,
            };
            info_section.update_track_metadata(&initial_metadata);
            #[cfg(feature = "kugou")]
            info_section.detect_online_track();
            #[cfg(feature = "kugou")]
            info_section.schedule_online_liked_query(cx);
            info_section.resolve_queue_item_metadata(cx);
            info_section.spawn_library_resolve(cx);

            info_section
        })
    }

    /// 用流元数据覆写展示名。只取流真正提供的值：在线文件可能带标签（或只有封面 /
    /// ReplayGain 帧）却没有可用的标题/艺术家，为那种情形抹掉由队列条目得出的名字，
    /// 会让播放栏一直显示 "Unknown Track" 直到下次切歌。
    fn update_track_metadata(&mut self, metadata: &crate::media::metadata::Metadata) {
        if let Some(name) = metadata.name.clone() {
            self.track_name = Some(SharedString::from(name));
        }
        if let Some(artist) = metadata
            .artist
            .clone()
            .or_else(|| metadata.album_artist.clone())
        {
            self.artist_name = Some(SharedString::from(artist));
        }
    }

    /// 只在本区没有名字的地方采用队列条目的展示名：已由流元数据提供的名字不得被覆盖。
    fn fill_missing_names(&mut self, data: QueueItemUIData) {
        if self.track_name.is_none() {
            self.track_name = data.name;
        }
        if self.artist_name.is_none() {
            self.artist_name = data.artist_name;
        }
    }

    /// 把队列条目的订阅重新接到当前队列位置上的那个条目，并采用它已持有的展示数据。
    fn resolve_queue_item_metadata(&mut self, cx: &mut Context<Self>) {
        // drop 会取消订阅；detach 则让整个会话期间旧队列条目实体上仍挂着一个活跃
        // 观察者（还让它继续触发）。
        drop(self.queue_item_subscription.take());
        self.queue_item_data = None;

        let queue = cx.global::<Models>().queue.read(cx);
        let position = queue.position;
        let item = queue.data.read_recovered().get(position).cloned();

        let Some(item) = item else { return };

        // 队列位置是"什么在播放"的唯一真相源；这里以 `current_track_path` 为准会让
        // 路径落后于队列时（例如会话恢复之后才刷新的在线 URL）封面必坏，出现队列条目
        // 有图而本区缩略图空白的状态。句柄来自 UI 侧的元数据旁表（P2-B 第 4 步）：
        // 同一 uid 恒解析到同一实体，而当前曲目的 uid 在队列里是活的，所以保留的句柄
        // 与下面的订阅都不可能过期。
        let data = queue_metadata::queue_ui_data(cx, &item);
        self.queue_item_data = Some(data.clone());

        // SongChanged 先于 QueuePositionChanged 广播，所以这段可能在 UI 位置还指向
        // 上一首时运行。只从真正装着那首刚开始曲目的槽位填名字——从过期槽位填会把
        // 错误曲目的名字锁住（随后的"仅在缺失时填"策略又会挡住位置变化那次解析去纠正）。
        let slot_is_current = self
            .current_track_path
            .as_ref()
            .is_some_and(|path| path == item.get_path());

        let item_path = item.get_path().clone();
        let subscription = cx.observe(&data, move |this: &mut Self, data, cx| {
            let data = data.read(cx).clone();
            if let Some(data) = data
                && this
                    .current_track_path
                    .as_ref()
                    .is_some_and(|path| *path == item_path)
            {
                this.fill_missing_names(data);
                cx.notify();
            }
        });
        self.queue_item_subscription = Some(subscription);

        let data = data.read(cx).clone();
        if slot_is_current && let Some(data) = data {
            self.fill_missing_names(data);
            cx.notify();
        }
    }

    /// 把当前播放位置重新解析为在线（酷狗）曲目。在线曲目没有音乐库条目，所以
    /// `current_library_track` 保持 `None`；我们改为把运行中的流 URL 反查回它的曲目。
    #[cfg(feature = "kugou")]
    fn detect_online_track(&mut self) {
        let matched = self.current_track_path.as_ref().and_then(|path| {
            if !crate::media::is_http_path(path) {
                return None;
            }
            if let Some(track) = online_track_matching_path(path) {
                return Some(OnlinePlayingTrack::Kugou(track));
            }
            None
        });
        if matched != self.online_track {
            if let Some(track) = &matched {
                tracing::info!(
                    identity = %track.identity(),
                    "info-section: online track detected for like button"
                );
            } else {
                tracing::debug!("info-section: no online track matched for like button");
            }
        }
        self.online_track = matched;
        self.is_online_liked = false;
    }

    /// 为"当前在线曲目是否已在其音源的喜欢列表里"发起一次后台查询，落地后再反映结果
    /// （这样本来就已喜欢的歌默认就把星标点亮）。
    #[cfg(feature = "kugou")]
    fn schedule_online_liked_query(&mut self, cx: &mut Context<Self>) {
        let Some(track) = self.online_track.clone() else {
            return;
        };
        let identity = track.identity();
        cx.spawn(async move |this, cx| {
            let liked = match &track {
                OnlinePlayingTrack::Kugou(track) => {
                    !track.hash.is_empty() && online_track_is_liked(&track.hash).await
                }
            };
            this.update(cx, |this, cx| {
                // 只有仍停在同一首曲目时才应用。
                if this
                    .online_track
                    .as_ref()
                    .is_some_and(|t| t.matches_identity(&identity))
                {
                    this.is_online_liked = liked;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// 本区右端的星标按钮：曲目被喜欢时（音乐库行或在线音源）填充，切换适用于当前
    /// 曲目的那套喜欢状态。
    fn like_button(&self, cx: &Context<Self>, is_liked_filled: bool, track_id: Option<i64>) -> Div {
        let theme = cx.global::<Theme>();

        div().pb(px(6.0)).h_full().flex().ml_auto().child(
            div()
                .id("info-like")
                .my_auto()
                .rounded_sm()
                .p(px(4.0))
                .cursor_pointer()
                .hover(|this| this.bg(theme.button_secondary_hover))
                .active(|this| this.bg(theme.button_secondary_active))
                .child(
                    icon(if is_liked_filled { STAR_FILLED } else { STAR })
                        .size(ICON_SM)
                        .text_color(if is_liked_filled {
                            theme.liked_song
                        } else {
                            theme.text_secondary
                        }),
                )
                .when(is_liked_filled, |this| {
                    this.tooltip(build_tooltip(tr!("UNLIKE", "Unlike")))
                })
                .when(!is_liked_filled, |this| {
                    this.tooltip(build_tooltip(tr!("LIKE", "Like")))
                })
                .on_click(cx.listener(move |_this, _, _, cx| {
                    if let Some(track_id) = track_id {
                        toggle_like(track_id, cx.entity(), cx);
                        return;
                    }
                    #[cfg(feature = "kugou")]
                    let this = _this;
                    #[cfg(feature = "kugou")]
                    if let Some(online) = this.online_track.clone() {
                        match &online {
                            OnlinePlayingTrack::Kugou(track) => {
                                if this.is_online_liked {
                                    kugou_unlike_track(cx, track);
                                } else {
                                    kugou_like_track(cx, track);
                                }
                            }
                        }
                        this.is_online_liked = !this.is_online_liked;
                        cx.notify();
                    }
                })),
        )
    }

    /// 清掉所有由曲目派生的状态并启动一次全新的音乐库解析。名称/艺术家来自元数据模型
    /// 或队列条目；DB 派生的状态在后台解析完成时落地。
    fn update_current_track_state(
        &mut self,
        current_track: Option<&CurrentTrack>,
        cx: &mut Context<Self>,
    ) {
        self.current_track_path = current_track.map(|track| track.get_path().clone());
        self.track_name = None;
        self.artist_name = None;
        self.current_library_track = None;
        self.can_navigate_to_album = false;
        self.can_navigate_to_artist = false;
        self.is_liked = None;
        self.image_element_key = self.image_element_key.wrapping_add(1);

        self.spawn_library_resolve(cx);
    }

    /// 在 runtime 上解析当前曲目的音乐库行、艺术家可否导航与喜欢状态——每次解析三个
    /// DB 查询，过去这些会把 UI 线程堵住。代号守卫会丢弃来自已经切走的曲目的结果；
    /// 落地之前本区继续显示已知的元数据（名称/艺术家/封面），只是没有音乐库动作。
    fn spawn_library_resolve(&mut self, cx: &mut Context<Self>) {
        let Some(track_path) = self.current_track_path.clone() else {
            return;
        };
        self.library_resolve_generation += 1;
        let generation = self.library_resolve_generation;
        let pool = cx.global::<crate::ui::app::Pool>().0.clone();

        cx.spawn(async move |this, cx| {
            let resolved = crate::RUNTIME
                .spawn(async move {
                    let track = crate::library::db::get_track_by_path(&pool, &track_path)
                        .await
                        .ok()
                        .flatten();
                    let track = track?;
                    let can_navigate_to_artist =
                        crate::library::db::artist_ids_for_track(&pool, track.id)
                            .await
                            .map(|v| !v.is_empty())
                            .unwrap_or(false);
                    let is_liked = crate::library::db::playlist_has_track(
                        &pool,
                        LIKED_SONGS_PLAYLIST_ID,
                        track.id,
                    )
                    .await
                    .ok()
                    .flatten();

                    Some(((*track).clone(), can_navigate_to_artist, is_liked))
                })
                .await;

            this.update(cx, |this, cx| {
                if this.library_resolve_generation != generation {
                    return;
                }
                // 查询出错或非音乐库（在线）路径就让已清空的状态原地保留——与同步路径
                // 得到 None 的结果一样。
                if let Ok(Some((track, can_navigate_to_artist, is_liked))) = resolved {
                    this.current_library_track = Some(Rc::new(track));
                    this.can_navigate_to_album = this
                        .current_library_track
                        .as_ref()
                        .is_some_and(|t| t.album_id.is_some());
                    this.can_navigate_to_artist = can_navigate_to_artist;
                    this.is_liked = is_liked;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
}

impl Render for InfoSection {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _render = crate::tally_render(crate::RenderedView::InfoSection);
        let add_to_state = self.current_library_track.as_ref().map(|track| {
            crate::ui::library::context_menus::add_to_playlist_state(
                "info-section-menu-state",
                track.id,
                window,
                cx,
            )
        });

        let image_key = self
            .current_library_track
            .as_ref()
            .map(|track| ManagedImageKey::Track(track.id))
            .or_else(|| {
                // 在线曲目没有音乐库曲目；复用当前队列条目存好的 cover_url，与队列
                // 缩略图的做法完全一致。
                #[cfg(feature = "online_sources")]
                {
                    self.queue_item_data
                        .as_ref()
                        .and_then(|data| data.read(cx).clone())
                        .and_then(|data| {
                            data.cover_url
                                .filter(|url| !url.is_empty())
                                .map(ManagedImageKey::HttpCover)
                        })
                }
                #[cfg(not(feature = "online_sources"))]
                {
                    None
                }
            })
            .or_else(|| {
                self.current_track_path
                    .as_ref()
                    .map(|p| ManagedImageKey::TrackFile(p.clone()))
            });
        let image_element_key = self.image_element_key;
        let theme = cx.global::<Theme>();
        let state = self.playback_info.playback_state.read(cx);
        let album_navigation_track = self
            .can_navigate_to_album
            .then(|| self.current_library_track.clone())
            .flatten();
        let artist_navigation_track = self
            .can_navigate_to_artist
            .then(|| self.current_library_track.clone())
            .flatten();
        let content = div()
            .id("info-section")
            .flex()
            .w_full()
            .h_full()
            .overflow_x_hidden()
            .flex_shrink_0()
            .child(
                div()
                    .mx(px(12.0))
                    .mt(px(12.0))
                    .mb(px(6.0))
                    .gap(px(10.0))
                    .flex()
                    .w_full()
                    .overflow_x_hidden()
                    .child(
                        div()
                            .id("album-art")
                            .rounded(px(theme.radius_sm))
                            .bg(theme.album_art_background)
                            .shadow_sm()
                            .w(px(36.0))
                            .h(px(36.0))
                            .mb(px(6.0))
                            .flex_shrink_0()
                            .on_hover(cx.listener(|this, is_hovering: &bool, _, cx| {
                                if this.is_hovering_art != *is_hovering {
                                    this.is_hovering_art = *is_hovering;
                                    cx.notify();
                                }
                            }))
                            // 双击进入沉浸视图；单击路径保持空闲（hover 显示大图预览）。
                            .on_click(|event: &ClickEvent, _, cx| {
                                if event.click_count() == 2 {
                                    crate::ui::immersive::set_immersive(true, cx);
                                }
                            })
                            .when_some(image_key, |this: Stateful<Div>, key| {
                                this.when(self.is_hovering_art, |this: Stateful<Div>| {
                                    this.child(
                                        anchored().anchor(Anchor::BottomRight).child(deferred(
                                            div()
                                                .id("album-art-preview")
                                                .occlude()
                                                .pb(px(26.0))
                                                .child(
                                                    managed_image(
                                                        (
                                                            "album-art-preview-img",
                                                            image_element_key,
                                                        ),
                                                        key.clone(),
                                                    )
                                                    .thumb_max(256)
                                                    .uncached()
                                                    .w(px(256.0))
                                                    .h(px(256.0))
                                                    .rounded(px(theme.radius_lg))
                                                    .shadow_md(),
                                                ),
                                        )),
                                    )
                                })
                                .child(
                                    managed_image(("album-art-thumb", image_element_key), key)
                                        .w(px(36.0))
                                        .h(px(36.0))
                                        .object_fit(ObjectFit::Fill)
                                        .rounded(px(theme.radius_sm))
                                        .thumb()
                                        .uncached(),
                                )
                            }),
                    )
                    .when(*state == PlaybackState::Stopped, |e| {
                        e.child(
                            div()
                                .line_height(rems(1.0))
                                .font_weight(FontWeight::EXTRA_BOLD)
                                .text_size(px(15.0))
                                .flex()
                                .h_full()
                                .items_center()
                                .pb(px(6.0))
                                .child(tr!(
                                    "APP_NAME",
                                    "Meliora",
                                    #description="Use the english name everywhere unless this \
                                        is strictly disagreeable.
                                ")),
                        )
                    })
                    .when(*state != PlaybackState::Stopped, |e| {
                        let is_liked = self.is_liked;
                        let track_id = self.current_library_track.as_ref().map(|t| t.id);
                        #[cfg(feature = "kugou")]
                        let is_liked_filled = (track_id.is_some() && is_liked.is_some())
                            || (self.online_track.is_some() && self.is_online_liked);
                        #[cfg(not(feature = "kugou"))]
                        let is_liked_filled = is_liked.is_some();
                        #[cfg(feature = "kugou")]
                        let has_track = track_id.is_some() || self.online_track.is_some();
                        #[cfg(not(feature = "kugou"))]
                        let has_track = track_id.is_some();

                        e.child(
                            div()
                                .flex()
                                .flex_col()
                                .line_height(rems(1.0))
                                .text_size(px(15.0))
                                .gap_1()
                                .w_full()
                                .overflow_x_hidden()
                                .child(
                                    div()
                                        .id("info-section-track-name")
                                        .font_weight(FontWeight::EXTRA_BOLD)
                                        .text_ellipsis()
                                        .w_full()
                                        .when_some(album_navigation_track, |this, track| {
                                            this.cursor_pointer().on_click(move |_, _, cx| {
                                                navigate_to_track_album_and_reveal(cx, &track);
                                            })
                                        })
                                        .child(self.track_name.clone().unwrap_or_else(|| {
                                            tr!("UNKNOWN_TRACK", "Unknown Track").into()
                                        })),
                                )
                                .child(
                                    div()
                                        .id("info-section-artist-name")
                                        .text_ellipsis()
                                        .w_full()
                                        .when_some(artist_navigation_track, |this, track| {
                                            this.cursor_pointer().on_click(move |ev, _, cx| {
                                                navigate_to_track_artist(cx, &track, ev.position());
                                            })
                                        })
                                        .child(self.artist_name.clone().unwrap_or_else(|| {
                                            tr!("UNKNOWN_ARTIST", "Unknown Artist").into()
                                        })),
                                ),
                        )
                        .when(has_track, |e| {
                            e.child(self.like_button(cx, is_liked_filled, track_id))
                        })
                    }),
            );

        // 没匹配到身份的在线曲目会让菜单只剩 0 项（艺术家/专辑/喜欢/加入全都以音乐库
        // 曲目为键，而"显示位置"对流是隐藏的）——干脆不要挂这个弹层。
        #[cfg(feature = "kugou")]
        let menu_has_items = self.current_library_track.is_some() || self.online_track.is_some();
        #[cfg(not(feature = "kugou"))]
        let menu_has_items = self.current_library_track.is_some();

        if menu_has_items
            && (self.current_track_path.is_some() || self.current_library_track.is_some())
        {
            let show_add_to = add_to_state.as_ref().map(|(s, _)| s.clone());
            let add_to = add_to_state.map(|(_, a)| a);

            // 在线曲目：把"在文件管理器中显示"换成下载动作
            #[cfg(feature = "kugou")]
            let online_download = self.online_track.clone().map(|track| match track {
                OnlinePlayingTrack::Kugou(track) => (
                    SharedString::from(crate::ui::kugou::download_label()),
                    Rc::new(move |cx: &mut App| {
                        crate::ui::kugou::download_track_ui(cx, track.clone())
                    }) as Rc<dyn Fn(&mut App)>,
                ),
            });
            #[cfg(not(feature = "kugou"))]
            let online_download: Option<(SharedString, Rc<dyn Fn(&mut App)>)> = None;

            div()
                .child(
                    context("info-section-context").with(content).child(
                        div()
                            .bg(theme.elevated_background)
                            .child(InfoSectionContextMenu::new(
                                self.current_track_path.clone(),
                                self.current_library_track.clone(),
                                self.can_navigate_to_artist,
                                self.is_liked,
                                show_add_to,
                                online_download,
                            )),
                    ),
                )
                .when_some(add_to, |d, add_to| d.child(add_to))
                .into_any_element()
        } else {
            content.into_any_element()
        }
    }
}
