pub mod equalizer;
pub mod interface;
pub mod playback;
pub mod replaygain;
pub mod scan;
pub mod storage;
pub mod update;

use std::{
    fs,
    fs::File,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::channel,
    },
    time::Duration,
};

use gpui::{App, AppContext, AsyncApp, Context, Entity, Global};
use notify::{Event, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::{library::scan::ScanInterface, playback::interface::PlaybackInterface};

/// User preferences persisted to `settings.json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Settings {
    #[serde(default)]
    pub scanning: scan::ScanSettings,
    #[serde(default)]
    pub playback: playback::PlaybackSettings,
    #[serde(default)]
    pub interface: interface::InterfaceSettings,
    #[serde(default)]
    pub update: update::UpdateSettings,
}

fn has_stored_theme_setting(value: &serde_json::Value) -> bool {
    value
        .get("interface")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|interface| interface.contains_key("theme"))
}

fn apply_legacy_theme_selection(path: &Path, settings: &mut Settings, has_theme_setting: bool) {
    if has_theme_setting || settings.interface.theme.is_some() {
        return;
    }

    let legacy_theme = path.parent().unwrap().join("theme.json");
    if legacy_theme.is_file() {
        settings.interface.theme = Some("theme.json".to_string());
    }
}

/// Defaults with the legacy `theme.json` migration applied; used whenever the
/// settings file cannot be read or parsed. `has_theme_setting` is the raw
/// [`has_stored_theme_setting`] verdict, or `false` when there is no file.
fn fallback_settings(path: &Path, has_theme_setting: bool) -> Settings {
    let mut settings = Settings::default();
    apply_legacy_theme_selection(path, &mut settings, has_theme_setting);
    settings
}

/// Result of reading `settings.json`: either the parsed [`Settings`], or
/// defaults paired with the offending path so the UI can surface a corruption
/// notice (see [`SettingsGlobal::initial_corrupt_path`]).
#[derive(Debug)]
pub enum SettingsLoadOutcome {
    Loaded(Settings),
    Corrupt { settings: Settings, path: PathBuf },
}

impl SettingsLoadOutcome {
    /// The settings to run with, whether or not the file was corrupt.
    pub fn into_settings(self) -> Settings {
        match self {
            SettingsLoadOutcome::Loaded(settings) => settings,
            SettingsLoadOutcome::Corrupt { settings, .. } => settings,
        }
    }
}

/// Read the settings file at `path`, applying the legacy `theme.json`
/// migration to the result. A missing file counts as `Loaded` defaults; a
/// parse or deserialize failure yields `Corrupt` carrying the path.
pub fn create_settings(path: &Path) -> SettingsLoadOutcome {
    let Ok(contents) = fs::read_to_string(path) else {
        return SettingsLoadOutcome::Loaded(fallback_settings(path, false));
    };

    let value: serde_json::Value = match serde_json::from_str(&contents) {
        Ok(value) => value,
        Err(e) => {
            warn!("Failed to parse settings file ({e}), scanner will wait for recovery");
            return SettingsLoadOutcome::Corrupt {
                settings: fallback_settings(path, false),
                path: path.to_path_buf(),
            };
        }
    };

    let has_theme_setting = has_stored_theme_setting(&value);
    let mut settings: Settings = match serde_json::from_value(value) {
        Ok(settings) => settings,
        Err(e) => {
            warn!("Failed to deserialize settings file ({e}), scanner will wait for recovery");
            return SettingsLoadOutcome::Corrupt {
                settings: fallback_settings(path, has_theme_setting),
                path: path.to_path_buf(),
            };
        }
    };

    apply_legacy_theme_selection(path, &mut settings, has_theme_setting);
    SettingsLoadOutcome::Loaded(settings)
}

/// 磁盘写的尾沿防抖，每次写盘经 spawn_blocking 离开 UI 线程。滑条拖动经
/// DebouncedSave（ui/settings/debounced_save.rs）与各视图内 300ms 防抖后只在
/// 尾沿调用一次 `save_settings`；这里的 500ms 吸收连续开关/多源快速变更。
/// 文件监视器回读每次写盘，内容有变才触发全窗口刷新。
const SETTINGS_SAVE_DEBOUNCE: Duration = Duration::from_millis(500);
static SETTINGS_SAVE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Push `settings` onto the playback thread and the scanner immediately, then
/// debounce the disk write (trailing edge, see [`SETTINGS_SAVE_DEBOUNCE`]).
pub fn save_settings(cx: &mut App, settings: &Settings) {
    let playback = cx.global::<PlaybackInterface>();
    playback.update_settings(settings.playback.clone());

    let scan = cx.global::<ScanInterface>();
    scan.update_settings(settings.scanning.clone());

    let path = cx.global::<SettingsGlobal>().path.clone();
    let snapshot = settings.clone();

    // The globals above must take effect live; only the disk write collapses
    // into one trailing-edge write (same pattern as the equalizer's save).
    let generation = SETTINGS_SAVE_GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    crate::RUNTIME.spawn(async move {
        tokio::time::sleep(SETTINGS_SAVE_DEBOUNCE).await;
        if SETTINGS_SAVE_GENERATION.load(Ordering::Relaxed) != generation {
            return;
        }
        let result = tokio::task::spawn_blocking(move || {
            File::create(path).and_then(|file| {
                serde_json::to_writer_pretty(file, &snapshot).map_err(|e| e.into())
            })
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("Failed to save settings file: {e:?}"),
            Err(e) => warn!("settings save task failed: {e}"),
        }
    });
}

pub struct SettingsGlobal {
    pub model: Entity<Settings>,
    pub path: PathBuf,
    /// `Some(path)` when the initial load at startup found a corrupt settings file.
    /// Consumed by `build_models` to set up `SettingsHealth`, is `None` afterwards.
    pub initial_corrupt_path: Option<PathBuf>,
    #[allow(dead_code)]
    pub watcher: Option<Box<dyn Watcher>>,
}

impl Global for SettingsGlobal {}

/// Load settings from `path`, publish [`SettingsGlobal`], and watch the
/// containing directory so hand edits to `settings.json` are re-read live.
pub fn setup_settings(cx: &mut App, path: PathBuf) {
    let outcome = create_settings(&path);
    let initial_corrupt_path = match &outcome {
        SettingsLoadOutcome::Corrupt { path, .. } => Some(path.clone()),
        SettingsLoadOutcome::Loaded(_) => None,
    };
    let settings = cx.new(|_| outcome.into_settings());
    let settings_model = settings.clone(); // for the closure

    // create and setup file watcher
    let (tx, rx) = channel::<notify::Result<Event>>();

    let watcher = notify::recommended_watcher(tx);

    let Ok(mut watcher) = watcher else {
        warn!("failed to create settings watcher");

        let global = SettingsGlobal {
            model: settings,
            path: path.clone(),
            initial_corrupt_path,
            watcher: None,
        };

        cx.set_global(global);
        return;
    };
    // settings.json 固定在数据目录根，NonRecursive 在 Windows 上仍监视直接
    // 子文件（手编与原子替换均发生在目录级），image-cache 等子目录的高频写盘
    // 事件得以在事件源外挡掉，而不是靠下游逐条过滤。
    if let Err(e) = watcher.watch(path.parent().unwrap(), RecursiveMode::NonRecursive) {
        warn!("failed to watch settings file: {:?}", e);
    }

    let settings_path = path.clone();
    let path_for_watcher = path.clone();

    cx.spawn(async move |app: &mut AsyncApp| {
        loop {
            while let Ok(event) = rx.try_recv() {
                match event {
                    Ok(v) => {
                        if !v.paths.iter().any(|t| t.ends_with("settings.json")) {
                            continue;
                        }
                        match v.kind {
                            notify::EventKind::Remove(_) => {
                                info!("Settings file removed, using default settings");
                            }
                            notify::EventKind::Create(_) | notify::EventKind::Modify(_) => {}
                            _ => continue,
                        }
                        let outcome = create_settings(&path_for_watcher);
                        settings_model.update(app, |v, cx| {
                            apply_settings_outcome(cx, v, outcome);
                        });
                    }
                    Err(e) => warn!("watch error: {:?}", e),
                }
            }

            // settings.json is hand-edited; a 1 s poll reacts within
            // perception without waking the main thread multiple times a
            // second.
            app.background_executor()
                .timer(Duration::from_secs(1))
                .await;
        }
    })
    .detach();

    let global = SettingsGlobal {
        model: settings,
        path: settings_path,
        initial_corrupt_path,
        watcher: Some(Box::new(watcher)),
    };

    cx.set_global(global);
}

/// Applies a fresh [`SettingsLoadOutcome`] produced by the file watcher. When the file parses
/// cleanly the in-memory `Settings` is replaced and health is marked `Ok`; when the file is
/// corrupt the existing `Settings` is preserved (so the scanner keeps using the last known-good
/// configuration) and health is flipped to `Corrupt`.
fn apply_settings_outcome(
    cx: &mut Context<Settings>,
    current: &mut Settings,
    outcome: SettingsLoadOutcome,
) {
    use crate::ui::models::{Models, SettingsHealth};

    let next_health = match outcome {
        SettingsLoadOutcome::Loaded(settings) => {
            // deliver external edits to the playback thread and the scanner, save_settings
            // pushes on its own and its reload diff is empty
            if current.playback != settings.playback && cx.has_global::<PlaybackInterface>() {
                cx.global::<PlaybackInterface>()
                    .update_settings(settings.playback.clone());
            }
            if current.scanning != settings.scanning && cx.has_global::<ScanInterface>() {
                cx.global::<ScanInterface>()
                    .update_settings(settings.scanning.clone());
            }
            // same-content reloads (our own save echo, notify storms) must not
            // cascade into the app-wide refresh_windows observer
            let changed = *current != settings;
            *current = settings;
            if changed {
                cx.notify();
            }
            SettingsHealth::Ok
        }
        SettingsLoadOutcome::Corrupt { path, .. } => SettingsHealth::Corrupt { path },
    };

    if cx.has_global::<Models>() {
        let health = cx.global::<Models>().settings_health.clone();
        health.update(cx, |h, cx| {
            if *h != next_health {
                *h = next_health;
                cx.notify();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Settings, SettingsLoadOutcome, apply_legacy_theme_selection, create_settings,
        has_stored_theme_setting,
    };
    use crate::test_support::TestDir;
    use serde_json::json;
    use std::{fs, path::PathBuf};

    fn create_test_dir() -> TestDir {
        TestDir::new("meliora-settings-test")
    }

    fn settings_path(dir: &TestDir) -> PathBuf {
        dir.join("settings.json")
    }

    #[test]
    fn has_stored_theme_setting_detects_raw_theme_key_presence() {
        assert!(has_stored_theme_setting(&json!({
            "interface": { "theme": "custom.json" }
        })));
        assert!(has_stored_theme_setting(&json!({
            "interface": { "theme": null }
        })));
        assert!(!has_stored_theme_setting(&json!({ "interface": {} })));
        assert!(!has_stored_theme_setting(&json!({})));
    }

    #[test]
    fn apply_legacy_theme_selection_only_applies_when_allowed() {
        let dir = create_test_dir();
        let settings_path = settings_path(&dir);
        fs::write(dir.path().join("theme.json"), "{}").unwrap();

        let mut settings = Settings::default();
        apply_legacy_theme_selection(&settings_path, &mut settings, false);
        assert_eq!(settings.interface.theme.as_deref(), Some("theme.json"));

        let mut settings = Settings::default();
        apply_legacy_theme_selection(&settings_path, &mut settings, true);
        assert_eq!(settings.interface.theme, None);

        let mut settings = Settings::default();
        settings.interface.theme = Some("custom.json".to_string());
        apply_legacy_theme_selection(&settings_path, &mut settings, false);
        assert_eq!(settings.interface.theme.as_deref(), Some("custom.json"));
    }

    #[test]
    fn create_settings_missing_file_reports_loaded() {
        let dir = create_test_dir();
        let outcome = create_settings(&settings_path(&dir));

        assert!(matches!(outcome, SettingsLoadOutcome::Loaded(_)));

        let settings = outcome.into_settings();
        let defaults = Settings::default();
        assert_eq!(settings.interface, defaults.interface);
        assert_eq!(settings.playback, defaults.playback);
    }

    #[test]
    fn create_settings_invalid_json_reports_corrupt() {
        let dir = create_test_dir();
        let path = settings_path(&dir);
        fs::write(&path, "{not valid json").unwrap();

        let outcome = create_settings(&path);

        match outcome {
            SettingsLoadOutcome::Corrupt {
                settings,
                path: reported,
            } => {
                assert_eq!(reported, path);
                let defaults = Settings::default();
                assert_eq!(settings.interface, defaults.interface);
                assert_eq!(settings.playback, defaults.playback);
            }
            SettingsLoadOutcome::Loaded(_) => {
                panic!("expected corrupt outcome for malformed settings file")
            }
        }
    }

    #[test]
    fn create_settings_type_mismatch_reports_corrupt() {
        let dir = create_test_dir();
        let path = settings_path(&dir);
        fs::write(&path, r#"{"playback": "not an object"}"#).unwrap();

        assert!(matches!(
            create_settings(&path),
            SettingsLoadOutcome::Corrupt { .. }
        ));
    }

    #[test]
    fn create_settings_deserializes_valid_json() {
        let dir = create_test_dir();
        fs::write(
            settings_path(&dir),
            serde_json::to_vec(&json!({
                "playback": {
                    "always_repeat": true,
                    "prev_track_jump_first": true,
                    "keep_current_on_queue_clear": false
                },
                "interface": {
                    "theme": "custom.json",
                    "full_width_library": true,
                    "reduced_motion": true,
                    "always_show_scrollbars": true
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let outcome = create_settings(&settings_path(&dir));
        assert!(matches!(outcome, SettingsLoadOutcome::Loaded(_)));
        let settings = outcome.into_settings();

        assert!(settings.playback.always_repeat);
        assert!(settings.playback.prev_track_jump_first);
        assert!(!settings.playback.keep_current_on_queue_clear);
        assert_eq!(settings.interface.theme.as_deref(), Some("custom.json"));
        assert!(settings.interface.full_width_library);
        assert!(settings.interface.reduced_motion);
        assert!(settings.interface.always_show_scrollbars);
    }

    #[test]
    fn all_categories_deserialize_when_empty() {
        let empty_settings = json!({
            "scanning": {},
            "playback": {},
            "interface": {},
            "update": {}
        });

        let _: Settings = serde_json::from_value(empty_settings).unwrap();
    }

    #[test]
    fn update_settings_default_when_key_missing() {
        let dir = create_test_dir();
        fs::write(settings_path(&dir), r#"{"interface": {}}"#).unwrap();

        let settings = create_settings(&settings_path(&dir)).into_settings();

        assert!(settings.update.auto_check);
        assert!(settings.update.auto_download);
    }
}
