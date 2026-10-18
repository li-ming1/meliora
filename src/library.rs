pub mod db;
pub mod playlist;
pub mod scan;
pub mod types;

use gpui::Global;
use sqlx::SqlitePool;

/// 库 SQLite 连接池的全局句柄。放在 `library`（而非它原先所在的
/// `ui::app`），让 playback/stats/library 代码不依赖 UI 层也能拿到池；
/// `ui::app` 再导出它，以保留现有 `ui::app::Pool` 引用。
pub struct Pool(pub SqlitePool);

impl Global for Pool {}

#[cfg(test)]
mod row_build_bench;
