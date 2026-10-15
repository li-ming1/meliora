//! Shared parts of the two KuGou discovery views (`kugou_ranks` and
//! `kugou_playlists`): the fixed track-row height, the like/unlike outcome
//! watcher and the track-row view builder. Gated behind the `kugou` cargo
//! feature together with its two users.
//!
//! These are the pieces that were byte-identical between the two views
//! (2026-10-15 收编台阶 D) once every real difference — element id prefix,
//! duration padding, liked predicate, rollback snapshot — is injected as a
//! parameter. Provider-agnostic layout pieces of the same consolidation
//! (list container, page scroller, Load More pill) live in
//! `ui::online_common`; this file holds the parts that are KuGou-specific
//! by nature (the liked-set predicate, the play/like/download callbacks).

use std::{sync::Arc, time::Duration};

use gpui::{AnyElement, App, Context, Entity, IntoElement};

use crate::ui::{
    kugou::{KugouTrackInfo, play_track_now},
    online_track_row::track_row,
    theme::Theme,
};

/// uniform_list strides rows by one fixed height measured from the first
/// row, so track rows are pinned to their natural height: 8px vertical
/// padding × 2 + title line (text_sm) + 1px gap + subtitle line (text_xs)
/// + 1px bottom border. Both discovery views render the same row layout,
/// so they share this value.
pub(crate) const ONLINE_TRACK_ROW_HEIGHT: f32 = 60.0;

/// How long the like/unlike watcher waits for the shared helper to confirm
/// the outcome through the global liked-set before rolling the optimistic
/// update back.
pub(crate) const LIKE_WATCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Watches a like/unlike request until the process-wide liked-set confirms
/// it (or the deadline passes), releasing the view's in-flight click-dedup
/// marker either way and rolling the optimistic snapshot back on failure.
///
/// The shared like/unlike helpers flip the global liked-set on success and
/// leave it untouched on failure, so the outcome is observable via
/// `crate::ui::kugou::liked_set_contains`; the optimistic update must be
/// undone when the request did not land, or a failed like leaves a wrong
/// star. `take_in_flight` drops the per-view dedup marker (called on both
/// paths); `rollback` restores the pre-click snapshot (failure path only,
/// hence `FnOnce` — each view captures its own snapshot there).
pub(crate) fn spawn_like_watcher<V: 'static>(
    cx: &mut Context<V>,
    hash: String,
    want_liked: bool,
    take_in_flight: impl Fn(&mut V, &str) + Send + 'static,
    rollback: impl FnOnce(&mut V, &str) + Send + 'static,
) {
    cx.spawn(async move |this, cx| {
        let deadline = std::time::Instant::now() + LIKE_WATCH_TIMEOUT;
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            if crate::ui::kugou::liked_set_contains(&hash) == want_liked {
                // landed: release the dedup marker, nothing to roll back
                this.update(cx, |this, _| take_in_flight(this, &hash)).ok();
                return;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
        }
        // never confirmed: undo the optimistic update
        this.update(cx, |this, cx| {
            take_in_flight(this, &hash);
            rollback(this, &hash);
            cx.notify();
        })
        .ok();
    })
    .detach();
}

/// Builds one discovery track row for either view. The row itself is
/// [`track_row`]; what differs between the views is only the element id
/// prefix, whether durations are padded to whole minutes, the liked
/// predicate and the like callback — all injected, so the per-row closure
/// plumbing (Arc clones instead of deep clones, the weak-entity upgrade)
/// exists once instead of twice.
///
/// Rows are built from inside the uniform_list render closure where only
/// `&App` is available, so there is no `cx.listener` here: the like handler
/// reaches the view through a weak handle.
#[allow(clippy::too_many_arguments)] // builder-style row API, mirrors track_row it feeds
pub(crate) fn kugou_track_row_view<V: 'static>(
    view: &V,
    entity: &Entity<V>,
    track: &Arc<KugouTrackInfo>,
    index: usize,
    cx: &App,
    id_prefix: &'static str,
    pad_minutes: bool,
    is_liked: impl Fn(&V, &str) -> bool,
    on_like: impl Fn(&mut V, &Arc<KugouTrackInfo>, &mut Context<V>) + 'static,
) -> AnyElement {
    let theme = cx.global::<Theme>();
    let liked = is_liked(view, &track.hash);
    // The row closures must own their data ('static): capture Arc clones
    // (refcount bump only) instead of three deep clones per visible row
    // per redraw — a full clone re-allocates `hash: String` each time.
    let play = track.clone();
    let like = track.clone();
    let download = track.clone();
    let weak = entity.downgrade();

    track_row(
        theme,
        track,
        index,
        id_prefix,
        true,
        pad_minutes,
        liked,
        move |_, _, cx| {
            play_track_now(cx, &play);
        },
        move |_, _, cx| {
            let Some(view) = weak.upgrade() else {
                return;
            };
            view.update(cx, |this, cx| on_like(this, &like, cx));
        },
        move |_, _, cx| {
            crate::ui::kugou::download_track_ui(cx, (*download).clone());
        },
    )
    .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the "both views must agree" contract: uniform_list strides every
    /// row by `ONLINE_TRACK_ROW_HEIGHT`, so a view that silently drifts off
    /// this value would mis-stride its rows.
    #[test]
    fn shared_row_height_matches_the_row_layout_contract() {
        assert_eq!(ONLINE_TRACK_ROW_HEIGHT, 60.0);
        assert_eq!(LIKE_WATCH_TIMEOUT, Duration::from_secs(30));
    }
}
