use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use cntp_i18n::tr;
use gpui::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled,
    Window, anchored, div, px,
};
use nucleo::Utf32String;
use tracing::error;

use crate::{
    library::{
        db::{self, LibraryAccess},
        types::Playlist,
    },
    ui::{
        app::Pool,
        components::{
            icons::PLAYLIST_ADD,
            modal::modal,
            palette::{ExtraItem, ExtraItemProvider, FinderItemLeft, Palette, PaletteItem},
        },
        models::{Models, PlaylistEvent},
    },
};

#[derive(Clone, Debug, PartialEq)]
enum TrackList {
    Single(i64),
    Multi(Vec<i64>),
}

impl TrackList {
    fn from_ids(ids: Vec<i64>) -> Self {
        if ids.len() == 1 {
            TrackList::Single(ids[0])
        } else {
            TrackList::Multi(ids)
        }
    }

    fn first(&self) -> i64 {
        match self {
            TrackList::Single(id) => *id,
            // An empty multi list is the album flow's initial state: its track
            // ids are loaded in the background after the dialog opens. Track
            // id 0 can never exist in the database, so it degrades to the
            // "not in playlist" branch instead of panicking on the index.
            TrackList::Multi(ids) => ids.first().copied().unwrap_or(0),
        }
    }

    fn is_multi(&self) -> bool {
        matches!(self, TrackList::Multi(ids) if ids.len() > 1)
    }

    fn ids(&self) -> &[i64] {
        match self {
            TrackList::Single(id) => std::slice::from_ref(id),
            TrackList::Multi(ids) => ids,
        }
    }
}

/// Main-thread shared handle to the current track selection. All access is on
/// the UI thread (the entity and its palette provider closures), so a
/// `RefCell` is enough — no locks, no atomics.
type SharedTrackList = Rc<RefCell<TrackList>>;

fn read_track_list(shared: &SharedTrackList) -> TrackList {
    shared.borrow().clone()
}

/// One palette row: the selection snapshot, the target playlist, and the
/// existing playlist-item id when a single track is already in that playlist.
/// The third slot is resolved once when the item list is built so that
/// `middle_content` never queries the database — previously it ran a
/// UI-thread `block_on` `playlist_has_track` query for every visible row on
/// every palette rebuild (each keystroke; doctrine §2.3 / §14).
type PlaylistEntry = (TrackList, Playlist, Option<i64>);

fn existing_item_id(track_list: &TrackList, playlist: &Playlist, cx: &mut App) -> Option<i64> {
    if track_list.is_multi() {
        return None;
    }
    cx.playlist_has_track(playlist.id, track_list.first())
        .ok()
        .flatten()
}

/// Awaits a spawned playlist DB task and logs its two failure modes with the
/// call site's message; yields the payload only when the task fully
/// succeeded, so callers can gate the follow-up event on it.
async fn await_playlist_task<T>(
    task: tokio::task::JoinHandle<Result<T, sqlx::Error>>,
    failure_log: &'static str,
    panic_log: &'static str,
) -> Option<T> {
    match task.await {
        Ok(Ok(payload)) => Some(payload),
        Ok(Err(err)) => {
            error!("{}: {err:?}", failure_log);
            None
        }
        Err(err) => {
            error!("{}: {err:?}", panic_log);
            None
        }
    }
}

impl PaletteItem for PlaylistEntry {
    fn left_content(&self, cx: &mut App) -> Option<FinderItemLeft> {
        self.1.left_content(cx)
    }

    fn middle_content(&self, _cx: &mut App) -> SharedString {
        if self.0.is_multi() || self.2.is_none() {
            tr!(
                "ADD_TO_SELECTED_PLAYLIST",
                "Add to {{name}}",
                name = self.1.name.0.as_str()
            )
            .into()
        } else {
            tr!(
                "REMOVE_FROM_SELECTED_PLAYLIST",
                "Remove from {{name}}",
                name = self.1.name.0.as_str()
            )
            .into()
        }
    }

    fn right_content(&self, cx: &mut App) -> Option<SharedString> {
        self.1.right_content(cx)
    }
}

type MatcherFunc = Box<dyn Fn(&Arc<PlaylistEntry>, &mut App) -> Utf32String + 'static>;
type OnAccept = Box<dyn Fn(&Arc<PlaylistEntry>, &mut App) + 'static>;

pub struct AddToPlaylist {
    show: Entity<bool>,
    palette: Entity<Palette<PlaylistEntry, MatcherFunc, OnAccept>>,
    track_list: SharedTrackList,
}

impl AddToPlaylist {
    pub fn new(cx: &mut App, show: Entity<bool>, track_ids: Vec<i64>) -> Entity<Self> {
        cx.new(|cx| {
            let track_list: SharedTrackList = Rc::new(RefCell::new(TrackList::from_ids(track_ids)));

            let track_list_for_observe = track_list.clone();
            cx.observe(&show, move |this: &mut Self, _, cx| {
                let current = read_track_list(&track_list_for_observe);
                this.palette.update(cx, |palette, cx| {
                    // a failed query keeps the palette's current list instead
                    // of panicking the observer
                    let Ok(playlists) = cx.get_all_playlists() else {
                        error!("Failed to load playlists for the add-to-playlist dialog");
                        return;
                    };
                    let new_playlists = (*playlists)
                        .clone()
                        .into_iter()
                        .map(|playlist| {
                            let has_track = existing_item_id(&current, &playlist, cx);
                            (current.clone(), playlist, has_track)
                        })
                        .map(Arc::new)
                        .collect::<Vec<_>>();

                    cx.emit(new_playlists);

                    palette.reset(cx);
                });

                cx.notify();
            })
            .detach();

            let matcher: MatcherFunc = Box::new(|playlist, _| playlist.1.name.0.to_string().into());

            let show_clone = show.clone();

            // Accept reads the shared track list at click time, not the
            // per-item snapshot: in the album flow the ids land in the shared
            // list after the dialog has already opened, so the snapshot
            // captured when the items were built is still empty and accepting
            // it would silently add nothing on the first open.
            let track_list_for_accept = track_list.clone();

            let on_accept: OnAccept = Box::new(move |playlist, cx| {
                let track_ids = track_list_for_accept.borrow().ids().to_vec();
                let playlist_id = playlist.1.id;

                let pool = cx.global::<Pool>().0.clone();
                let playlist_tracker = cx.global::<Models>().playlist_tracker.clone();

                if track_ids.len() == 1 {
                    let track_id = track_ids[0];
                    let has_track = cx.playlist_has_track(playlist_id, track_id).ok().flatten();

                    cx.spawn(async move |cx| {
                        let task = if let Some(id) = has_track {
                            crate::RUNTIME
                                .spawn(async move { db::remove_playlist_item(&pool, id).await })
                        } else {
                            crate::RUNTIME.spawn(async move {
                                db::add_playlist_item(&pool, playlist_id, track_id)
                                    .await
                                    .map(|_| ())
                            })
                        };

                        if await_playlist_task(
                            task,
                            "could not remove/add track from playlist",
                            "remove/add from playlist task panicked",
                        )
                        .await
                        .is_some()
                        {
                            playlist_tracker.update(cx, |_, cx| {
                                cx.emit(PlaylistEvent::PlaylistUpdated(playlist_id));
                            });
                        }
                    })
                    .detach();
                } else {
                    cx.spawn(async move |cx| {
                        let task = crate::RUNTIME.spawn(async move {
                            for track_id in &track_ids {
                                db::add_playlist_item(&pool, playlist_id, *track_id).await?;
                            }
                            Ok::<(), sqlx::Error>(())
                        });

                        if await_playlist_task(
                            task,
                            "could not add tracks to playlist",
                            "add tracks to playlist task panicked",
                        )
                        .await
                        .is_some()
                        {
                            playlist_tracker.update(cx, |_, cx| {
                                cx.emit(PlaylistEvent::PlaylistUpdated(playlist_id));
                            });
                        }
                    })
                    .detach();
                }

                show_clone.write(cx, false);
            });

            // MELIORA RELIABILITY AUDIT (2026-09-13): items are deliberately
            // NOT resolved here. The palette is only ever visible while `show`
            // is true, and every open path flips `show` after creation, which
            // makes the observer above rebuild the full list — with a fresh
            // per-playlist `has_track` — in the same effect flush, before the
            // modal's first frame. Resolving here cost one `get_all_playlists`
            // block_on plus one `playlist_has_track` block_on per playlist on
            // every keyed-state creation; that state is dropped whenever the
            // row scrolls out of the virtualized list (gpui `Frame::finish`
            // keeps only element states accessed in the last frame), so the
            // queries recurred on every scroll pass for rows the user never
            // right-clicked (doctrine §2.3 / §14 / §34).
            let items: Vec<Arc<PlaylistEntry>> = Vec::new();

            let palette = Palette::new(cx, items, matcher, on_accept, &show);

            let track_list_for_create = track_list.clone();
            let show_for_create = show.clone();
            let provider: ExtraItemProvider = Arc::new(move |query: &str| {
                let name = query.trim();
                if name.is_empty() {
                    return Vec::new();
                }

                let name_string = name.to_string();
                let display = tr!("CREATE_PLAYLIST", name = name_string);

                let show_clone2 = show_for_create.clone();
                let create_track_ids = read_track_list(&track_list_for_create).ids().to_vec();

                vec![ExtraItem {
                    left: Some(FinderItemLeft::Icon(PLAYLIST_ADD.into())),
                    middle: display.into(),
                    right: None,
                    on_accept: Arc::new(move |cx| {
                        let pool = cx.global::<Pool>().0.clone();
                        let playlist_tracker = cx.global::<Models>().playlist_tracker.clone();
                        let name_string = name_string.clone();
                        let create_track_ids = create_track_ids.clone();

                        cx.spawn(async move |cx| {
                            let task = crate::RUNTIME.spawn(async move {
                                let playlist_id = db::create_playlist(&pool, &name_string).await?;
                                for track_id in &create_track_ids {
                                    db::add_playlist_item(&pool, playlist_id, *track_id).await?;
                                }
                                Ok::<i64, sqlx::Error>(playlist_id)
                            });

                            let Some(playlist_id) = await_playlist_task(
                                task,
                                "could not create playlist and add track",
                                "create playlist task panicked",
                            )
                            .await
                            else {
                                return;
                            };

                            playlist_tracker.update(cx, |_, cx| {
                                cx.emit(PlaylistEvent::PlaylistUpdated(playlist_id));
                            });
                        })
                        .detach();

                        show_clone2.write(cx, false);
                    }),
                }]
            });

            cx.update_entity(&palette, |palette, cx| {
                palette.register_extra_provider(provider.clone(), cx);
            });

            Self {
                show,
                palette,
                track_list,
            }
        })
    }

    pub fn set_track_ids(&self, track_ids: Vec<i64>) {
        *self.track_list.borrow_mut() = TrackList::from_ids(track_ids);
    }
}

impl Render for AddToPlaylist {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let show = self.show.clone();
        let palette = self.palette.clone();
        let show_read = *self.show.read(cx);

        if show_read {
            cx.update_entity(&palette, |palette, cx| {
                palette.focus(window, cx);
            });

            modal()
                .child(div().w(px(550.0)).h(px(300.0)).child(palette.clone()))
                .on_exit(move |_, cx| {
                    show.update(cx, |show, cx| {
                        *show = false;
                        cx.update_entity(&palette, |palette, cx| {
                            palette.reset(cx);
                        });
                        cx.notify();
                    })
                })
                .into_any_element()
        } else {
            anchored().into_any_element()
        }
    }
}
