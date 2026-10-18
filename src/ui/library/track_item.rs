use cntp_i18n::tr;
use gpui::prelude::{FluentBuilder, *};
use gpui::{
    App, Entity, FontWeight, IntoElement, Pixels, SharedString, TextAlign, TextRun, Window, div,
    img, px,
};
use std::{rc::Rc, sync::Arc};

use crate::ui::components::drag_drop::{DragPreview, TrackDragData};
use crate::ui::components::icons::{STAR, STAR_FILLED, icon};
use crate::ui::library::context_menus::track::TrackContextMenu;
use crate::ui::library::context_menus::{add_to_playlist_state, play_track_next};
use crate::ui::models::{
    HasLikedState, is_song_liked, is_track_available_snapshot, subscribe_liked_updates, toggle_like,
};
use crate::ui::util::format_duration;

use crate::library::types::{DBString, Track};
use crate::ui::design::ICON_SM;
use crate::ui::{
    components::{context::context, table::table_data::elevated_menu_element},
    library::context_menus::{PlaylistMenuInfo, TrackContextMenuContext, play_from_track_listing},
    models::PlaybackInfo,
    theme::Theme,
};

#[derive(Clone, Debug, PartialEq)]
pub enum ArtistNameVisibility {
    Always,
    OnlyIfDifferent(Option<DBString>),
}

pub type TrackPlaylistInfo = PlaylistMenuInfo;

pub struct TrackItem {
    /// 用 Rc 共享，好让点击/右键菜单闭包 clone 的是引用计数，而不是每次渲染都
    /// clone 整条曲目（含 PathBuf）。
    pub track: Rc<Track>,
    pub is_start: bool,
    pub artist_name_visibility: ArtistNameVisibility,
    pub is_liked: Option<i64>,
    pub hover_group: SharedString,
    /// 只测量一次（字体度量不会每帧变）；在第一次有窗口可用的渲染之前都是 `None`。
    max_track_num_width: Option<Pixels>,
    track_number_label: SharedString,
    /// 构造时算好；否则 `format_duration` 要每帧跑一遍。
    duration_text: SharedString,
    /// 拖拽负载预先构造一次；`on_drag` 每帧按值取走它，所以 render 只 clone 这个小
    /// 结构体，不再重跑 `from_track`（那是每帧一次 `PathBuf` 分配）。
    /// 仅当本行不属于任何歌单时才是 `Some`——歌单会装上自己的拖拽处理器。
    drag_data: Option<TrackDragData>,
    left_field: TrackItemLeftField,
    album_art: Option<SharedString>,
    pl_info: Option<TrackPlaylistInfo>,
    vinyl_numbering: bool,
    max_track_num_str: Option<SharedString>,
    is_available: bool,
    queue_context: Option<Arc<Vec<Track>>>,
    show_go_to_album: bool,
    show_go_to_artist: bool,
}

#[derive(Eq, PartialEq)]
pub enum TrackItemLeftField {
    TrackNum,
    Art,
}

/// [`TrackItem::new`] 的显示标志位，打包成一个结构体，好让构造函数不必收一堆
/// 位置参数 bool。
#[derive(Default)]
pub struct TrackItemOptions {
    pub(crate) is_start: bool,
    pub(crate) vinyl_numbering: bool,
    pub(crate) show_go_to_album: bool,
    pub(crate) show_go_to_artist: bool,
}

fn measure_track_number_width(window: &mut Window, text: &SharedString) -> Pixels {
    let style = window.text_style();
    let font_size = style.font_size.to_pixels(window.rem_size());

    let run = TextRun {
        len: text.len(),
        font: style.font(),
        color: style.color,
        background_color: None,
        underline: None,
        strikethrough: None,
        letter_spacing: None,
    };

    let line = window
        .text_system()
        .shape_line(text.clone(), font_size, &[run], None);

    line.x_for_index(line.len())
}

impl TrackItem {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cx: &mut App,
        track: Track,
        anv: ArtistNameVisibility,
        left_field: TrackItemLeftField,
        pl_info: Option<TrackPlaylistInfo>,
        max_track_num_str: Option<SharedString>,
        queue_context: Option<Arc<Vec<Track>>>,
        opts: TrackItemOptions,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let track_id = track.id;

            subscribe_liked_updates(cx, move |_| Some(track_id));

            let track_number_label: SharedString =
                track.track_number.unwrap_or_default().to_string().into();
            let duration_text: SharedString = format_duration(track.duration, false).into();

            // 拖拽负载预先构造一次：`on_drag` 每帧按值取走它，所以 render 只 clone
            // 这个小结构体，不再重跑 `from_track`（每帧一次 `PathBuf` 分配）
            let drag_data = if pl_info.is_none() {
                Some(TrackDragData::from_track(
                    track.id,
                    track.album_id,
                    track.location.clone(),
                    track.title.clone().0,
                ))
            } else {
                None
            };

            Self {
                hover_group: format!("track-{}", track.id).into(),
                max_track_num_width: None,
                track_number_label,
                duration_text,
                drag_data,
                is_liked: is_song_liked(cx, track.id),
                album_art: Some(match track.album_id {
                    Some(album_id) => format!("!db://album/{album_id}/thumb").into(),
                    None => format!("!db://track/{}/thumb", track.id).into(),
                }),
                is_available: is_track_available_snapshot(cx, &track),
                track: Rc::new(track),
                is_start: opts.is_start,
                artist_name_visibility: anv,
                left_field,
                pl_info,
                vinyl_numbering: opts.vinyl_numbering,
                max_track_num_str,
                queue_context,
                show_go_to_album: opts.show_go_to_album,
                show_go_to_artist: opts.show_go_to_artist,
            }
        })
    }
}

impl Render for TrackItem {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let _render = crate::tally_render(crate::RenderedView::TrackRow);
        let track_id = self.track.id;
        let (show_add_to, add_to) = add_to_playlist_state("track-menu-state", track_id, window, cx);

        let theme = cx.global::<Theme>();

        let track_num_width = *self.max_track_num_width.get_or_insert_with(|| {
            self.max_track_num_str
                .as_ref()
                .map(|max_num_str| measure_track_number_width(window, max_num_str))
                .unwrap_or(px(22.0))
        });
        // 只是与本行自己的路径做相等比较——不需要 clone
        let is_current = cx
            .global::<PlaybackInfo>()
            .current_track
            .read(cx)
            .as_ref()
            .is_some_and(|current| *current == self.track.location);
        let is_available = self.is_available;
        let is_liked = self.is_liked.is_some();

        let show_artist_name = match &self.artist_name_visibility {
            ArtistNameVisibility::Always => true,
            ArtistNameVisibility::OnlyIfDifferent(anv) => *anv != self.track.artist_names,
        };

        let track_menu_context = TrackContextMenuContext {
            show_go_to_album: self.show_go_to_album,
            show_go_to_artist: self.show_go_to_artist,
            play_from_here: Some(Rc::new({
                let plid = self.pl_info.as_ref().map(|pl| pl.id);
                let queue_context = self.queue_context.clone();
                move |cx, track| play_from_track_listing(cx, track, plid, queue_context.clone())
            })),
        };

        div()
            .w_full()
            .child(
                context(("context", self.track.id as usize))
                    .with(
                        div()
                            .flex()
                            .flex_col()
                            .w_full()
                            .id(self.track.id as usize)
                            .when(is_available, |this| {
                                this.on_click({
                                    let track = self.track.clone();
                                    let plid = self.pl_info.as_ref().map(|pl| pl.id);
                                    let queue_context = self.queue_context.clone();
                                    move |_, _, cx| {
                                        play_from_track_listing(
                                            cx,
                                            &track,
                                            plid,
                                            queue_context.clone(),
                                        )
                                    }
                                })
                                .on_aux_click({
                                    let track = self.track.clone();
                                    move |ev, _, cx| {
                                        if ev.is_middle_click() {
                                            play_track_next(cx, &track);
                                        }
                                    }
                                })
                            })
                            .when(!is_available, |this| this.cursor_default().opacity(0.5))
                            .when(self.is_start, |this| {
                                this.child(
                                    div()
                                        .text_color(theme.text_secondary)
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        // 22px（来自 track # 宽度）+ 18 + 13
                                        .px(px(track_num_width.to_f64() as f32 + 18.0 + 13.0))
                                        .border_b_1()
                                        .w_full()
                                        .border_color(theme.border_color)
                                        .mt(px(18.0))
                                        .pb(px(6.0))
                                        .text_ellipsis()
                                        .when_some(self.track.disc_number, |this, num| {
                                            if self.vinyl_numbering && (1..=26).contains(&num) {
                                                let side = (b'A' + (num - 1) as u8) as char;
                                                // i18n 插值只接受字符串
                                                let side = side.to_string();
                                                this.child(tr!(
                                                    "TRACK_SIDE",
                                                    "Side {{side}}",
                                                    side = side
                                                ))
                                            } else if let Some(subtitle) = &self.track.disc_subtitle
                                            {
                                                let subtitle = subtitle.0.as_str();

                                                this.child(tr!(
                                                    "TRACK_DISC_SUBTITLE",
                                                    "Disc {{num}} - {{subtitle}}",
                                                    num = num,
                                                    subtitle = subtitle
                                                ))
                                            } else {
                                                this.child(tr!(
                                                    "TRACK_DISC",
                                                    "Disc {{num}}",
                                                    num = num
                                                ))
                                            }
                                        }),
                                )
                            })
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .border_b_1()
                                    .h(px(39.0))
                                    .id(("track", self.track.id as u64))
                                    .w_full()
                                    .border_color(theme.border_color)
                                    .when(is_available, |this| this.cursor_pointer())
                                    .when(!is_available, |this| this.cursor_default())
                                    .px(px(18.0))
                                    .py(px(6.0))
                                    .group(self.hover_group.clone())
                                    .when(is_available, |this| {
                                        this.hover(|this| this.bg(theme.nav_button_hover))
                                            .active(|this| this.bg(theme.nav_button_active))
                                    })
                                    // 只在不属于歌单时处理拖拽：歌单有自己的拖拽处理器
                                    .when(is_available, |this| {
                                        match self.drag_data.clone() {
                                            Some(drag) => {
                                                let display_name = drag.display_name.clone();
                                                this.on_drag(drag, move |_, _, _, cx| {
                                                    DragPreview::new(cx, display_name.clone())
                                                })
                                            }
                                            // 歌单行没有自己的拖拽负载
                                            None => this,
                                        }
                                    })
                                    .when(is_current, |this| this.bg(theme.queue_item_current))
                                    .when(!is_current, |this| this.bg(theme.background_primary))
                                    .max_w_full()
                                    .when(self.left_field == TrackItemLeftField::TrackNum, |this| {
                                        this.child(
                                            div()
                                                .min_w(track_num_width)
                                                .flex_shrink_0()
                                                .text_align(TextAlign::Right)
                                                .mr(px(13.0))
                                                .text_color(theme.text_secondary)
                                                // 数字取自预先构造好的 track_number_label；
                                                // 对齐靠宽度上限保证
                                                .child(self.track_number_label.clone()),
                                        )
                                    })
                                    .when(self.left_field == TrackItemLeftField::Art, |this| {
                                        this.child(
                                            div()
                                                .w(px(22.0))
                                                .h(px(22.0))
                                                .mr(px(12.0))
                                                .my_auto()
                                                .rounded(px(theme.radius_sm))
                                                .bg(theme.album_art_background)
                                                .when_some(self.album_art.clone(), |this, art| {
                                                    this.child(
                                                        img(art)
                                                            .w(px(22.0))
                                                            .h(px(22.0))
                                                            .rounded(px(theme.radius_sm)),
                                                    )
                                                }),
                                        )
                                    })
                                    .child(
                                        div()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .overflow_x_hidden()
                                            .text_ellipsis()
                                            .mr_auto()
                                            .child(self.track.title.clone()),
                                    )
                                    .child(
                                        div()
                                            .font_weight(FontWeight::LIGHT)
                                            .text_sm()
                                            .my_auto()
                                            .text_color(theme.text_secondary)
                                            .text_ellipsis()
                                            .overflow_x_hidden()
                                            .flex_shrink(1.0)
                                            .ml(px(12.0))
                                            .when(show_artist_name, |this| {
                                                this.when_some(
                                                    self.track.artist_names.clone(),
                                                    |this, v| this.child(v.0),
                                                )
                                            }),
                                    )
                                    .child(
                                        div()
                                            .id("like")
                                            .my_auto()
                                            .rounded_sm()
                                            .ml(px(10.0))
                                            .p(px(4.0))
                                            .child(
                                                icon(if is_liked { STAR_FILLED } else { STAR })
                                                    .size(ICON_SM)
                                                    .text_color(if is_liked {
                                                        theme.liked_song
                                                    } else {
                                                        theme.text_secondary
                                                    }),
                                            )
                                            .group(self.hover_group.clone())
                                            .when(is_available, |this| {
                                                this.hover(|this| {
                                                    this.bg(theme.button_secondary_hover)
                                                })
                                                .active(|this| {
                                                    this.bg(theme.button_secondary_active)
                                                })
                                                .on_click(cx.listener(move |_, _, _, cx| {
                                                    cx.stop_propagation();
                                                    toggle_like(track_id, cx.entity(), cx);
                                                }))
                                            }),
                                    )
                                    .child(
                                        div()
                                            .ml(px(10.0))
                                            .flex_shrink_0()
                                            .min_w(px(60.0))
                                            .border_l_1()
                                            .pl(px(10.0))
                                            .border_color(theme.border_color)
                                            .text_align(TextAlign::Right)
                                            .child(self.duration_text.clone()),
                                    ),
                            ),
                    )
                    .menu_on_open({
                        // 菜单树（TrackContextMenu 的艺术家 DB 查找、path stat、翻译）
                        // 只在菜单打开时构建，绕开逐行重绘路径
                        let track = self.track.clone();
                        let track_menu_context = track_menu_context;
                        let pl_info = self.pl_info;
                        let is_liked = self.is_liked;
                        move |_, cx| {
                            elevated_menu_element(
                                cx,
                                TrackContextMenu::new(
                                    track.clone(),
                                    is_available,
                                    is_liked,
                                    track_menu_context.clone(),
                                    pl_info,
                                    show_add_to.clone(),
                                ),
                            )
                        }
                    }),
            )
            .child(add_to)
    }
}

impl HasLikedState for TrackItem {
    fn is_liked(&self) -> Option<i64> {
        self.is_liked
    }
    fn set_liked(&mut self, item_id: Option<i64>) {
        self.is_liked = item_id;
    }
}
