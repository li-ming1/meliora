use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

/// 更新检查偏好（`settings.json` → `update`）。更新器本身只存在于 `online` 构建
/// （`kugou` 蕴含它），但这一组始终可反序列化，让设置文件在各种构建下保持同一形状。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct UpdateSettings {
    /// 每次启动检查一次 GitHub Releases，在启动几秒之后。
    #[serde(default = "default_true")]
    pub auto_check: bool,
    /// 检查发现新版本时自动下载匹配的新构建；安装仍然等用户显式重启
    /// （见 `updater.rs`）。
    #[serde(default = "default_true")]
    pub auto_download: bool,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            auto_check: true,
            auto_download: true,
        }
    }
}
