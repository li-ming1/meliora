pub mod db;
pub mod playlist;
pub mod scan;
pub mod types;

use gpui::Global;
use sqlx::SqlitePool;

/// Global handle to the library's SQLite pool. Lives in `library` (not
/// `ui::app`, where it used to live) so playback/stats/library code can
/// reach the pool without depending on the UI layer; `ui::app` re-exports
/// it for the existing `ui::app::Pool` references.
pub struct Pool(pub SqlitePool);

impl Global for Pool {}

#[cfg(test)]
mod row_build_bench;
