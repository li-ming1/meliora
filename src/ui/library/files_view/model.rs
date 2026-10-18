use std::path::Path;
use std::sync::Arc;

use gpui::SharedString;

#[derive(Clone, Debug)]
pub struct TrackRef {
    pub id: i64,
    pub album_id: Option<i64>,
    /// 「我喜欢的」歌单项 id，若存在
    pub liked: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct RawEntry {
    pub name: SharedString,
    pub path: Arc<Path>,
    pub is_dir: bool,
    pub is_audio: bool,
    pub track: Option<TrackRef>,
}

#[derive(Clone, Debug)]
pub struct FlatRow {
    /// Arc 共享：行每个可见帧都要 clone，而给渲染闭包做逐行 `Rc<PathBuf>` clone
    /// 每次都是一次全新的堆分配。
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
