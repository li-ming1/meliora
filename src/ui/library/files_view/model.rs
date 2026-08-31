use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::SharedString;

#[derive(Clone, Debug)]
pub struct TrackRef {
    pub id: i64,
    pub album_id: Option<i64>,
    /// Liked Songs playlist item ID, if one exists
    pub liked: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct RawEntry {
    pub name: SharedString,
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_audio: bool,
    pub track: Option<TrackRef>,
}

#[derive(Clone, Debug)]
pub struct FlatRow {
    /// Arc-shared: rows are cloned per visible frame, and a per-row `Rc<PathBuf>`
    /// clone for the render closures was a fresh heap allocation each time.
    pub path: Arc<Path>,
    pub name: SharedString,
    pub depth: usize,
    pub is_dir: bool,
    pub is_audio: bool,
    pub expanded: bool,
    pub loading: bool,
    pub has_children: bool,
    pub track: Option<TrackRef>,
}
