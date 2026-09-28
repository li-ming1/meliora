use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

/// Update-check preferences (`settings.json` → `update`). The updater itself
/// only exists in `online` builds (`kugou`/`netease` imply it), but the group
/// always deserializes so the settings file keeps one shape across builds.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct UpdateSettings {
    /// Check GitHub Releases once per launch, a few seconds after startup.
    #[serde(default = "default_true")]
    pub auto_check: bool,
    /// Download a matched new build automatically when a check finds one;
    /// installing still waits for an explicit restart (see `updater.rs`).
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
