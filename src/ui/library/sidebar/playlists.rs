use std::sync::Arc;

use cntp_i18n::{tr, trn};
use gpui::{
    App, AppContext, Context, DragMoveEvent, Entity, FontWeight, InteractiveElement, IntoElement,
    MouseButton, ParentElement, Render, ScrollHandle, SharedString, StatefulInteractiveElement,
    StyleRefinement, Styled, Window, div, prelude::FluentBuilder, px, rgba,
};
use tracing::error;

use crate::{
    library::{
        db::{self, LibraryAccess},
        playlist::export_playlist,
        types::{Playlist, PlaylistType},
    },
    playback::interface::PlaybackInterface,
    settings::SettingsGlobal,
    toasts::{Toast, emit_toast},
    ui::{
        app::Pool,
        components::{
            button::{ButtonIntent, button},
            context::context,
            drag_drop::{
                AlbumDragData, DragData, DragDropItemState, DragDropListConfig,
                DragDropListManager, DragPreview, DropIndicator, TrackDragData,
                check_drag_cancelled, handle_drag_move, handle_drop,
            },
            icons::{CROSS, FILE_EXPORT, PENCIL, PLAY, PLAYLIST, PLUS, SHUFFLE, STAR},
            menu::{menu, menu_item, menu_separator},
            popover::{PopoverPosition, popover},
            scrollbar::{ScrollableHandle, floating_scrollbar},
            sidebar::sidebar_item,
            textbox::Textbox,
        },
        library::{NavigationHistory, ViewSwitchMessage, playlist_view::find_playlist_tracks},
        models::{Models, PlaybackInfo, PlaylistEvent},
        theme::Theme,
    },
};

const PLAYLIST_SIDEBAR_LIST_ID: &str = "sidebar-playlists";
const PLAYLIST_SIDEBAR_ITEM_HEIGHT: f32 = 55.0; // effective height is + 1 px because of gap

pub struct PlaylistList {
    playlists: Arc<Vec<Playlist>>,
    /// Bumped on every reload so a load that finishes after a newer one
    /// cannot overwrite it (same guard as `Table::reload_rows`).
    playlists_generation: u64,
    /// A reload is currently running on the runtime.
    reload_in_flight: bool,
    /// Set when a PlaylistEvent arrives while a reload is in flight: the
    /// in-flight snapshot may predate that event's DB write, so exactly one
    /// follow-up reload is queued instead of one query per event.
    reload_queued: bool,
    nav_model: Entity<NavigationHistory>,
    scroll_handle: ScrollHandle,
    popover_open: bool,
    new_playlist_input: Entity<Textbox>,
    rename_popover_playlist: Option<i64>,
    rename_playlist_input: Entity<Textbox>,
    pending_delete_playlist: Option<i64>,
    drag_drop_manager: Entity<DragDropListManager>,
}

impl PlaylistList {
    pub fn new(cx: &mut App, nav_model: Entity<NavigationHistory>) -> Entity<Self> {
        cx.new(|cx| {
            let sidebar_collapsed = cx.global::<Models>().sidebar_collapsed.clone();
            cx.observe(&sidebar_collapsed, |_, _, cx| cx.notify())
                .detach();

            let playlist_tracker = cx.global::<Models>().playlist_tracker.clone();

            cx.subscribe(
                &playlist_tracker,
                |this: &mut Self, _, _: &PlaylistEvent, cx| {
                    this.reload_playlists(cx);
                },
            )
            .detach();

            cx.observe(&nav_model, |_, _, cx| {
                cx.notify();
            })
            .detach();

            let weak_self = cx.entity().downgrade();
            let new_playlist_input =
                Textbox::new_with_submit(cx, StyleRefinement::default(), move |cx| {
                    if let Some(entity) = weak_self.upgrade() {
                        entity.update(cx, |this, cx| this.handle_submit(cx));
                    }
                });

            let weak_self_rename = cx.entity().downgrade();
            let rename_playlist_input =
                Textbox::new_with_submit(cx, StyleRefinement::default(), move |cx| {
                    if let Some(entity) = weak_self_rename.upgrade() {
                        entity.update(cx, |this, cx| this.handle_rename_submit(cx));
                    }
                });

            let drag_drop_config =
                DragDropListConfig::new(PLAYLIST_SIDEBAR_LIST_ID, px(PLAYLIST_SIDEBAR_ITEM_HEIGHT));
            let drag_drop_manager = DragDropListManager::new(cx, drag_drop_config);

            let mut this = Self {
                playlists: Arc::new(Vec::new()),
                playlists_generation: 0,
                reload_in_flight: false,
                reload_queued: false,
                nav_model,
                scroll_handle: ScrollHandle::new(),
                popover_open: false,
                new_playlist_input,
                rename_popover_playlist: None,
                rename_playlist_input,
                pending_delete_playlist: None,
                drag_drop_manager,
            };
            // Initial list load happens off the UI thread (see reload_playlists).
            this.reload_playlists(cx);
            this
        })
    }

    /// Reloads the playlist list (one GROUP BY/SUM aggregate) on the
    /// background runtime: construction and every like/unlike used to run it
    /// synchronously on the UI thread. The sidebar keeps showing its previous
    /// entries until the fresh list lands. A generation guard drops results
    /// superseded by a newer reload; events arriving mid-flight coalesce into
    /// one follow-up load instead of one query per event.
    fn reload_playlists(&mut self, cx: &mut Context<Self>) {
        if self.reload_in_flight {
            self.reload_queued = true;
            return;
        }
        self.reload_in_flight = true;
        self.playlists_generation = self.playlists_generation.wrapping_add(1);
        let generation = self.playlists_generation;
        let pool = cx.global::<Pool>().0.clone();

        cx.spawn(async move |this, cx| {
            let playlists = crate::RUNTIME
                .spawn(async move { db::get_all_playlists(&pool).await })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.reload_in_flight = false;
                // A newer reload superseded this one: drop the stale result.
                if this.playlists_generation != generation {
                    return;
                }

                match playlists {
                    Ok(Ok(playlists)) => {
                        this.playlists = playlists;
                        cx.notify();
                    }
                    Ok(Err(err)) => tracing::warn!(error = %err, "playlist list query failed"),
                    Err(err) => tracing::warn!(error = ?err, "playlist list task failed"),
                }

                if this.reload_queued {
                    this.reload_queued = false;
                    this.reload_playlists(cx);
                }
            });
        })
        .detach();
    }

    fn handle_submit(&mut self, cx: &mut Context<Self>) {
        let name = self.new_playlist_input.read(cx).value(cx);
        if name.is_empty() {
            return;
        }

        if let Ok(id) = cx.create_playlist(&name) {
            let playlist_tracker = cx.global::<Models>().playlist_tracker.clone();
            playlist_tracker.update(cx, |_, cx| {
                cx.emit(PlaylistEvent::PlaylistUpdated(id));
            });
        }

        self.popover_open = false;
        self.new_playlist_input.update(cx, |tb, cx| tb.reset(cx));
        cx.notify();
    }

    fn close_popover(&mut self, cx: &mut Context<Self>) {
        self.popover_open = false;
        cx.notify();
    }

    fn handle_rename_submit(&mut self, cx: &mut Context<Self>) {
        let name = self.rename_playlist_input.read(cx).value(cx);
        if name.is_empty() {
            return;
        }

        if let Some(pl_id) = self.rename_popover_playlist {
            if let Err(err) = cx.rename_playlist(pl_id, &name) {
                error!("Failed to rename playlist: {}", err);
            } else {
                let playlist_tracker = cx.global::<Models>().playlist_tracker.clone();
                playlist_tracker.update(cx, |_, cx| {
                    cx.emit(PlaylistEvent::PlaylistUpdated(pl_id));
                });
            }
        }

        self.rename_popover_playlist = None;
        self.rename_playlist_input.update(cx, |tb, cx| tb.reset(cx));
        cx.notify();
    }

    fn close_rename_popover(&mut self, cx: &mut Context<Self>) {
        self.rename_popover_playlist = None;
        cx.notify();
    }

    fn clear_pending_delete_playlist(&mut self, cx: &mut Context<Self>) {
        if self.pending_delete_playlist.take().is_some() {
            cx.notify();
        }
    }
}

fn delete_playlist_and_refresh(pl_id: i64, cx: &mut App) {
    if let Err(err) = cx.delete_playlist(pl_id) {
        error!("Failed to delete playlist: {}", err);
        return;
    }

    let playlist_tracker = cx.global::<Models>().playlist_tracker.clone();
    playlist_tracker.update(cx, |_, cx| cx.emit(PlaylistEvent::PlaylistDeleted(pl_id)));

    let playlist_sort_methods = cx.global::<Models>().playlist_sort_methods.clone();
    playlist_sort_methods.update(cx, |map, _| {
        map.remove(&pl_id);
    });

    let switcher_model = cx.global::<Models>().switcher_model.clone();
    switcher_model.update(cx, |history, cx| {
        history.retain(|v| *v != ViewSwitchMessage::Playlist(pl_id));
        cx.emit(ViewSwitchMessage::Refresh);
        cx.notify();
    })
}

/// Adds a batch of tracks to a playlist on the background runtime (rows
/// already present are skipped) and broadcasts PlaylistUpdated on success;
/// fire-and-forget. `what` only feeds the log wording ("tracks" / "album
/// tracks").
fn spawn_add_missing_tracks(
    pl_id: i64,
    track_ids: Vec<i64>,
    what: &'static str,
    cx: &mut Context<PlaylistList>,
) {
    let pool = cx.global::<Pool>().0.clone();
    let playlist_tracker = cx.global::<Models>().playlist_tracker.clone();

    cx.spawn(async move |_, cx| {
        let task = crate::RUNTIME.spawn(async move {
            db::add_tracks_to_playlist_if_missing(&pool, pl_id, &track_ids).await
        });

        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                error!("could not add {what} to playlist: {err:?}");
                return;
            }
            Err(err) => {
                error!("add {what} to playlist task panicked: {err:?}");
                return;
            }
        }

        playlist_tracker.update(cx, |_, cx| {
            cx.emit(PlaylistEvent::PlaylistUpdated(pl_id));
        });
    })
    .detach();
}

impl Render for PlaylistList {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        check_drag_cancelled(self.drag_drop_manager.clone(), cx);

        let theme = cx.global::<Theme>();
        let collapsed = *cx.global::<Models>().sidebar_collapsed.read(cx);
        let scroll_handle = self.scroll_handle.clone();
        let playlist_count = self.playlists.len();
        let allow_reorder = !collapsed;
        let mut main = div()
            .pt(px(6.0))
            .id("sidebar-playlist")
            .flex_grow(1.0)
            .min_h(px(0.0))
            .overflow_y_scroll()
            .track_scroll(&scroll_handle)
            .when(allow_reorder, |this| {
                this.on_drag_move::<DragData>(cx.listener(
                    move |this: &mut PlaylistList, event: &DragMoveEvent<DragData>, _, cx| {
                        let before = {
                            let manager = this.drag_drop_manager.read(cx);
                            (manager.state.is_dragging, manager.state.drop_target)
                        };
                        let scroll_handle: ScrollableHandle = this.scroll_handle.clone().into();
                        let reduced_motion = cx
                            .global::<SettingsGlobal>()
                            .model
                            .read(cx)
                            .interface
                            .reduced_motion;

                        let scrolled = handle_drag_move(
                            this.drag_drop_manager.clone(),
                            scroll_handle,
                            event,
                            playlist_count,
                            cx,
                            reduced_motion,
                        );

                        // repaint only when something visible moved: drag move
                        // fires at mouse report rate and an unconditional notify
                        // re-rendered the whole sidebar on every move
                        let changed = {
                            let manager = this.drag_drop_manager.read(cx);
                            scrolled
                                || (manager.state.is_dragging, manager.state.drop_target) != before
                        };
                        if changed {
                            cx.notify();
                        }
                    },
                ))
                .on_drop(cx.listener(
                    move |this: &mut PlaylistList, drag_data: &DragData, _, cx| {
                        let playlists = this.playlists.clone();
                        handle_drop(
                            this.drag_drop_manager.clone(),
                            drag_data,
                            cx,
                            move |from, to, cx| {
                                if from >= playlists.len() {
                                    return;
                                }
                                let source = &playlists[from];

                                let new_position = if to < playlists.len() {
                                    playlists[to].position
                                } else {
                                    playlists.iter().map(|p| p.position).max().unwrap_or(0) + 1
                                };

                                if source.position == new_position {
                                    return;
                                }

                                if let Err(e) = cx.reorder_playlist(source.id, new_position) {
                                    error!("Failed to reorder playlist: {}", e);
                                    return;
                                }

                                let tracker = cx.global::<Models>().playlist_tracker.clone();
                                tracker.update(cx, |_, cx| {
                                    cx.emit(PlaylistEvent::PlaylistUpdated(source.id));
                                });
                            },
                        );
                        cx.notify();
                    },
                ))
            });

        let current_view = self.nav_model.read(cx).current();

        let two_column = cx
            .global::<SettingsGlobal>()
            .model
            .read(cx)
            .interface
            .two_column_library;

        let sidebar_view = if two_column && current_view.is_detail_page() {
            self.nav_model
                .read(cx)
                .last_matching(ViewSwitchMessage::is_key_page)
                .unwrap_or(current_view)
        } else {
            current_view
        };

        let rename_input = self.rename_playlist_input.clone();
        let weak_entity = cx.entity().downgrade();

        for (idx, playlist) in self.playlists.iter().enumerate() {
            let pl_id = playlist.id;

            let playlist_label: SharedString = if playlist.is_liked_songs() {
                tr!("LIKED_SONGS", "Liked Songs").into()
            } else {
                playlist.name.0.clone()
            };

            let item_state = DragDropItemState::for_index(self.drag_drop_manager.read(cx), idx);

            let mut item = sidebar_item(("main-sidebar-pl", playlist.id as u64)).icon(
                if playlist.playlist_type == PlaylistType::System {
                    STAR
                } else {
                    PLAYLIST
                },
            );

            if collapsed {
                item = item.collapsed().collapsed_label(playlist_label.clone());
            } else {
                item = item
                    .child(
                        div()
                            .child(playlist_label.clone())
                            .text_ellipsis()
                            .flex_shrink(1.0)
                            .overflow_x_hidden()
                            .w_full(),
                    )
                    .child(
                        div()
                            .font_weight(FontWeight::NORMAL)
                            .text_color(theme.text_secondary)
                            .text_xs()
                            .text_ellipsis()
                            .flex_shrink(1.0)
                            .w_full()
                            .overflow_x_hidden()
                            .mt(px(2.0))
                            .child(trn!(
                                "PLAYLIST_TRACK_COUNT",
                                "{{count}} track",
                                "{{count}} tracks",
                                count = playlist.track_count
                            )),
                    );
            }

            let is_system_playlist = playlist.playlist_type == PlaylistType::System;

            let item = item
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.nav_model.update(cx, move |_, cx| {
                        cx.emit(ViewSwitchMessage::Playlist(pl_id));
                    });
                }))
                .when(
                    sidebar_view == ViewSwitchMessage::Playlist(playlist.id),
                    |this| this.active(),
                )
                .when(allow_reorder, |this| {
                    let drag_label = playlist_label.clone();
                    this.on_drag(
                        DragData::new(idx, PLAYLIST_SIDEBAR_LIST_ID),
                        move |_, _, _, cx| DragPreview::new(cx, drag_label.clone()),
                    )
                    .drag_over::<DragData>(|style, _, _, _| style.bg(rgba(0x88888822)))
                })
                .drag_over::<TrackDragData>(|style, _, _, _| style.bg(rgba(0x88888822)))
                .drag_over::<AlbumDragData>(|style, _, _, _| style.bg(rgba(0x88888822)))
                .on_drop(cx.listener(
                    move |_: &mut PlaylistList, drag_data: &TrackDragData, _, cx| {
                        let is_from_queue = drag_data
                            .source_list_id
                            .as_ref()
                            .map(|id| *id == "queue".into())
                            .unwrap_or(false);

                        let track_ids: Vec<i64> = if is_from_queue {
                            let Some(_source_index) = drag_data.source_index else {
                                return;
                            };
                            let queue = cx.global::<Models>().queue.read(cx);
                            let queue_data = queue.data.read().unwrap_or_else(|e| e.into_inner());
                            let all_indices = drag_data.all_indices();
                            all_indices
                                .into_iter()
                                .filter_map(|i| queue_data.get(i).and_then(|item| item.get_db_id()))
                                .collect()
                        } else {
                            drag_data.track_id.into_iter().collect()
                        };

                        if track_ids.is_empty() {
                            return;
                        }

                        spawn_add_missing_tracks(pl_id, track_ids, "tracks", cx);
                    },
                ))
                .on_drop(cx.listener(
                    move |_: &mut PlaylistList, drag_data: &AlbumDragData, _, cx| {
                        let track_ids: Vec<i64> =
                            if let Ok(tracks) = cx.list_tracks_in_album(drag_data.album_id) {
                                tracks.iter().map(|t| t.id).collect()
                            } else {
                                Vec::new()
                            };

                        if track_ids.is_empty() {
                            return;
                        }

                        spawn_add_missing_tracks(pl_id, track_ids, "album tracks", cx);
                    },
                ));

            let rename_open = self.rename_popover_playlist == Some(pl_id);
            // snapshot for the lazy menu builder: it runs when the menu opens
            // and captures this render's state (a re-render after the confirm
            // click rebuilds the builder with the fresh value)
            let pending_delete_open = self.pending_delete_playlist == Some(pl_id);
            let weak_self = weak_entity.clone();
            let weak_self2 = weak_entity.clone();
            let weak_context = weak_entity.clone();
            // the lazy builder is a move closure rebuilt every render, so it
            // gets its own refcount instead of consuming `weak_entity`
            let menu_weak_entity = weak_entity.clone();
            let name = playlist.name.0.clone();

            main = main.child(
                div()
                    .relative()
                    .when(item_state.is_being_dragged, |this| this.opacity(0.5))
                    .child(
                        context(("playlist", pl_id as usize))
                            .with(item)
                            .on_close(move |_, cx| {
                                if let Some(entity) = weak_context.upgrade() {
                                    entity.update(cx, |this, cx| {
                                        this.clear_pending_delete_playlist(cx);
                                    });
                                }
                            })
                            // menu tree is built only when the menu opens, off
                            // the per-item repaint path (same lazy pattern as
                            // grid_item's context menu)
                            .menu_on_open(move |_, cx| {
                                let theme = cx.global::<Theme>();
                                div().bg(theme.elevated_background).child(
                                    menu()
                                        .item(menu_item(
                                            "playlist_play",
                                            Some(PLAY),
                                            tr!("PLAY"),
                                            move |_, _, cx| {
                                                let tracks = find_playlist_tracks(cx, pl_id);
                                                let interface = cx.global::<PlaybackInterface>();
                                                interface.replace_queue(tracks);
                                            },
                                        ))
                                        .item(menu_item(
                                            "playlist_play_next",
                                            None::<&'static str>,
                                            tr!("PLAY_NEXT"),
                                            move |_, _, cx| {
                                                let tracks = find_playlist_tracks(cx, pl_id);
                                                let queue_position =
                                                    cx.global::<Models>().queue.read(cx).position;
                                                let interface = cx.global::<PlaybackInterface>();
                                                interface
                                                    .insert_list_at(tracks, queue_position + 1);
                                            },
                                        ))
                                        .item(menu_item(
                                            "playlist_shuffle",
                                            Some(SHUFFLE),
                                            tr!("SHUFFLE"),
                                            move |_, _, cx| {
                                                let tracks = find_playlist_tracks(cx, pl_id);
                                                let interface = cx.global::<PlaybackInterface>();
                                                if !(*cx
                                                    .global::<PlaybackInfo>()
                                                    .shuffling
                                                    .read(cx))
                                                {
                                                    interface.toggle_shuffle();
                                                }
                                                interface.replace_queue(tracks);
                                            },
                                        ))
                                        .item(menu_item(
                                            "playlist_add_to_queue",
                                            Some(PLUS),
                                            tr!("ADD_TO_QUEUE"),
                                            move |_, _, cx| {
                                                let tracks = find_playlist_tracks(cx, pl_id);
                                                let interface = cx.global::<PlaybackInterface>();
                                                interface.queue_list(tracks);
                                            },
                                        ))
                                        .item(menu_separator())
                                        .when(!is_system_playlist, |menu| {
                                            menu.item(menu_item(
                                                "rename_playlist",
                                                Some(PENCIL),
                                                tr!("RENAME_PLAYLIST", "Rename playlist"),
                                                {
                                                    // the lazy builder is an Fn (the menu can
                                                    // open repeatedly): give the handler its own
                                                    // refcounts instead of moving the builder's
                                                    // captures out
                                                    let weak_self = weak_self.clone();
                                                    let name = name.clone();
                                                    move |_, window, cx| {
                                                        if let Some(entity) = weak_self.upgrade() {
                                                            let name = name.clone();
                                                            entity.update(cx, move |this, cx| {
                                                                this.rename_popover_playlist =
                                                                    Some(pl_id);
                                                                this.rename_playlist_input
                                                                    .read(cx)
                                                                    .focus_handle()
                                                                    .focus(window, cx);

                                                                this.rename_playlist_input.update(
                                                                    cx,
                                                                    move |input, cx| {
                                                                        input.set_value(cx, name);
                                                                    },
                                                                );

                                                                cx.notify();
                                                            });
                                                        }
                                                    }
                                                },
                                            ))
                                        })
                                        .item(menu_item(
                                            "export_playlist",
                                            Some(FILE_EXPORT),
                                            tr!("EXPORT_PLAYLIST", "Export to M3U"),
                                            {
                                                let playlist_label = playlist_label.clone();
                                                move |_, _, cx| {
                                                    if let Err(err) =
                                                        export_playlist(cx, pl_id, &playlist_label)
                                                    {
                                                        emit_toast(Toast::error(tr!(
                                                            "EXPORT_PLAYLIST_FAILED",
                                                            "Failed to export playlist"
                                                        )));
                                                        tracing::error!(
                                                            ?err,
                                                            "playlist export failed"
                                                        );
                                                    }
                                                }
                                            },
                                        ))
                                        .when(!is_system_playlist, |menu| {
                                            if pending_delete_open {
                                                let weak_for_delete = menu_weak_entity.clone();
                                                menu.item(
                                                    menu_item(
                                                        "delete_playlist_confirm",
                                                        Some(CROSS),
                                                        tr!(
                                                            "DELETE_PLAYLIST_CONFIRM",
                                                            "Click again to delete"
                                                        ),
                                                        move |_, _, cx| {
                                                            if let Some(entity) =
                                                                weak_for_delete.upgrade()
                                                            {
                                                                entity.update(cx, |this, cx| {
                                                                    this.clear_pending_delete_playlist(cx);
                                                                });
                                                            }
                                                            delete_playlist_and_refresh(pl_id, cx);
                                                        },
                                                    )
                                                    .text_color(theme.status_error)
                                                    .icon_color(theme.status_error),
                                                )
                                            } else {
                                                let weak_for_confirm = menu_weak_entity.clone();
                                                menu.item(menu_item(
                                                    "delete_playlist",
                                                    Some(CROSS),
                                                    tr!("DELETE_PLAYLIST", "Delete playlist"),
                                                    move |_, _, cx| {
                                                        if let Some(entity) =
                                                            weak_for_confirm.upgrade()
                                                        {
                                                            entity.update(cx, |this, cx| {
                                                                this.pending_delete_playlist =
                                                                    Some(pl_id);
                                                                cx.notify();
                                                            });
                                                        }
                                                        cx.stop_propagation();
                                                    },
                                                ))
                                            }
                                        }),
                                    )
                                    .into_any_element()
                            }),
                    )
                    .when(rename_open && !is_system_playlist, |this| {
                        this.child(
                            popover()
                                .position(PopoverPosition::RightTop)
                                .edge_offset(px(12.0))
                                .on_dismiss(move |_, cx| {
                                    if let Some(entity) = weak_self2.upgrade() {
                                        entity.update(cx, |this, cx| this.close_rename_popover(cx));
                                    }
                                })
                                .min_w(px(250.0))
                                .flex()
                                .flex_col()
                                .gap(px(6.0))
                                .on_any_mouse_down(|_, _, cx| {
                                    cx.stop_propagation();
                                })
                                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.close_rename_popover(cx);
                                }))
                                .child(rename_input.clone())
                                .child(
                                    div()
                                        .flex()
                                        .justify_end()
                                        .gap(px(6.0))
                                        .child(
                                            button()
                                                .id(("cancel-rename", pl_id as u64))
                                                .child(tr!("CANCEL"))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.close_rename_popover(cx);
                                                })),
                                        )
                                        .child(
                                            button()
                                                .id(("rename-playlist", pl_id as u64))
                                                .intent(ButtonIntent::Primary)
                                                .child(tr!("RENAME", "Rename"))
                                                .on_click(cx.listener(|this, _, _, cx| {
                                                    this.handle_rename_submit(cx);
                                                })),
                                        ),
                                ),
                        )
                    })
                    .child(DropIndicator::with_state(
                        item_state.is_drop_target_before,
                        item_state.is_drop_target_after,
                        theme.button_primary,
                    )),
            );
        }

        let popover_open = self.popover_open;
        let new_playlist_input = self.new_playlist_input.clone();
        let weak_self = cx.entity().downgrade();

        main = main.child(
            div()
                .relative()
                .child(
                    sidebar_item("new-playlist-btn")
                        .icon(PLUS)
                        .child(tr!("NEW_PLAYLIST", "New Playlist"))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();

                                this.popover_open = !popover_open;
                                if !popover_open {
                                    this.new_playlist_input
                                        .read(cx)
                                        .focus_handle()
                                        .focus(window, cx);
                                }
                                cx.notify();
                            }),
                        ),
                )
                .when(popover_open, |this| {
                    let weak_self_for_dismiss = weak_self.clone();
                    this.child(
                        popover()
                            .position(PopoverPosition::RightTop)
                            .edge_offset(px(12.0))
                            .on_dismiss(move |_, cx| {
                                if let Some(entity) = weak_self_for_dismiss.upgrade() {
                                    entity.update(cx, |this, cx| this.close_popover(cx));
                                }
                            })
                            .min_w(px(250.0))
                            .flex()
                            .flex_col()
                            .gap(px(6.0))
                            .on_any_mouse_down(|_, _, cx| {
                                cx.stop_propagation();
                            })
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.close_popover(cx);
                            }))
                            .child(new_playlist_input.clone())
                            .child(
                                div()
                                    .flex()
                                    .justify_end()
                                    .gap(px(6.0))
                                    .child(
                                        button()
                                            .id("cancel-playlist")
                                            .child(tr!("CANCEL", "Cancel"))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.close_popover(cx);
                                            })),
                                    )
                                    .child(
                                        button()
                                            .id("create-playlist")
                                            .intent(ButtonIntent::Primary)
                                            .child(tr!("CREATE", "Create"))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.handle_submit(cx);
                                            })),
                                    ),
                            ),
                    )
                }),
        );

        div()
            .gap(px(2.0))
            .mt(px(-6.0))
            .flex()
            .flex_col()
            .w_full()
            .flex_grow(1.0)
            .min_h(px(0.0))
            .relative()
            .child(main)
            .when(!collapsed, |this| {
                this.child(floating_scrollbar("playlist_list_scrollbar", scroll_handle))
            })
    }
}
