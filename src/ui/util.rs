#[cfg(target_os = "windows")]
use std::path::PathBuf;
use std::path::Path;
use std::sync::Arc;

use gpui::{App, Entity, Render, RenderImage};
use rustc_hash::FxHashMap;
use tracing::{debug, error};

/// Rows this far outside the visible window stay cached. Scrolling back
/// within the band reuses the existing row entities instead of re-running
/// their DB/stat construction pipeline on the UI thread; the band must stay
/// wider than any viewport so visible rows are never pruned.
const VIEW_KEEP_AROUND: usize = 128;

pub fn prune_views<T>(
    views_model: &Entity<FxHashMap<usize, Entity<T>>>,
    render_counter: &Entity<usize>,
    current: usize,
    cx: &mut App,
) -> bool
where
    T: Render,
{
    let last = *render_counter.read(cx);
    let mut to_remove: Vec<usize> = Vec::new();
    let mut did_remove = false;

    // Prune views outside the union of the previous and current windows
    // plus the keep-around band. Covers both scroll directions: upward
    // (current < last) behaves exactly as before, and downward scrolling
    // now also prunes far-above rows instead of accumulating the whole
    // table's row views on a long one-way scroll. Don't prune the first
    // view so this still works with uniform_list.
    let lower = last.min(current).saturating_sub(VIEW_KEEP_AROUND);
    let upper = last.max(current) + 1 + VIEW_KEEP_AROUND;
    for idx in views_model.read(cx).keys() {
        if (*idx < lower || *idx >= upper) && *idx != 0_usize {
            to_remove.push(*idx);
        }
    }

    for idx in to_remove {
        did_remove = true;
        views_model.update(cx, |m, _| {
            debug!("Removing view at index: {}", idx);
            m.remove(&idx);
        });
    }

    // update the render counter
    render_counter.update(cx, |m, _| {
        *m = current;
    });

    did_remove
}

pub fn create_or_retrieve_view<T>(
    views_model: &Entity<FxHashMap<usize, Entity<T>>>,
    key: usize,
    creation_fn: impl FnOnce(&mut App) -> Entity<T>,
    cx: &mut App,
) -> Entity<T>
where
    T: Render,
{
    let view = views_model.read(cx).get(&key).cloned();
    match view {
        Some(view) => view,
        None => {
            let view = creation_fn(cx);
            views_model.update(cx, |m, _| {
                m.insert(key, view.clone());
            });
            view
        }
    }
}

/// Drops the atlas tiles for a batch of images whose owning elements are gone.
/// Unlike the old `drop_image_from_app` this tolerates windows disappearing
/// mid-reclaim (skips instead of panicking), which matters when draining the
/// render cache's eviction queue: the cache outlives any single window.
///
/// Each drop is additionally wrapped in `catch_unwind`: etagere's generation
/// assertion (stale tile id after the atlas recycles a page — an upstream
/// gpui-windows issue) used to abort the whole process from inside the
/// Windows message callback. The assertion fires before any state mutation,
/// so catching it degrades to one leaked tile per occurrence and the app
/// keeps running; the panic hook still logs the occurrence.
pub fn reclaim_images_from_app(cx: &mut App, images: Vec<Arc<RenderImage>>) {
    if images.is_empty() {
        return;
    }
    cx.defer(move |cx| {
        for image in images {
            for window in cx.windows() {
                let image = image.clone();
                let _ = window.update(cx, move |_, window, _| {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let _ = window.drop_image(image);
                    }));
                    if let Err(payload) = result {
                        crate::ui::components::managed_image::note_tile_drop_panic();
                        let message = payload
                            .downcast_ref::<&str>()
                            .map(|s| (*s).to_string())
                            .or_else(|| payload.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "unknown panic payload".into());
                        error!(
                            "[atlas] tile drop panicked (tile leaked, app continues): {message}"
                        );
                    }
                });
            }
        }
    });
}

pub fn reveal_path_for_file_manager(path: &Path, cx: &mut App) {
    #[cfg(windows)]
    {
        // this is some crazy garbage but it has to be this way because of windows wonkyness
        let path_for_reveal = match path.to_string_lossy().strip_prefix("\\\\?\\") {
            Some(stripped) => PathBuf::from(stripped),
            None => path.to_path_buf(),
        };

        cx.reveal_path(path_for_reveal.as_path());
    }

    #[cfg(not(windows))]
    {
        cx.reveal_path(path);
    }
}


fn split_duration(secs: i64) -> (i64, i64, i64) {
    let secs = secs.max(0);
    (secs / 3_600, (secs % 3_600) / 60, secs % 60)
}

/// 紧凑时长格式：不足 1 小时为 "m:ss"（如 "2:22"、"0:45"），达到 1 小时为 "h:mm:ss"（如 "1:02:03"）。
/// 直接复用 [`format_duration`]，保证全 app（播放队列等）时长显示一致。
pub fn format_duration_compact(secs: i64) -> String {
    format_duration(secs, false)
}

pub fn format_duration(secs: i64, pad_minutes: bool) -> String {
    let (hours, minutes, seconds) = split_duration(secs);

    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else if pad_minutes {
        format!("{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}
