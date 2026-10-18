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
        Arc, Mutex,
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

/// 持久化到 `settings.json` 的用户偏好。
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

/// 套用了旧版 `theme.json` 迁移的默认值；设置文件读不到或解析不了时一律走它。
/// `has_theme_setting` 是 [`has_stored_theme_setting`] 的原始判定，没有文件时为 `false`。
fn fallback_settings(path: &Path, has_theme_setting: bool) -> Settings {
    let mut settings = Settings::default();
    apply_legacy_theme_selection(path, &mut settings, has_theme_setting);
    settings
}

/// 读取 `settings.json` 的结果：要么解析出 [`Settings`]，要么给出默认值并附带出错
/// 路径，好让 UI 弹出损坏提示（见 [`SettingsGlobal::initial_corrupt_path`]）。
#[derive(Debug)]
pub enum SettingsLoadOutcome {
    Loaded(Settings),
    Corrupt { settings: Settings, path: PathBuf },
}

impl SettingsLoadOutcome {
    /// 无论文件是否损坏，实际用来运行的设置。
    pub fn into_settings(self) -> Settings {
        match self {
            SettingsLoadOutcome::Loaded(settings) => settings,
            SettingsLoadOutcome::Corrupt { settings, .. } => settings,
        }
    }
}

/// 加载/重载时观测到的设置文件健康度：能解析是 `Ok`；不能解析则是 `Corrupt`
/// 并携带出错路径（由 UI 的损坏设置提示浮出，见 `SettingsGlobal::initial_corrupt_path`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsHealth {
    Ok,
    Corrupt { path: PathBuf },
}

/// 读取 `path` 处的设置文件，并对结果套用旧版 `theme.json` 迁移。
/// 只有 `NotFound` 才算 `Loaded` 默认值：其它任何读取错误（索引器/杀软引起的共享冲突、
/// 权限问题）都必须报 `Corrupt`，这样 watcher 才保留内存态设置，
/// 而不是拿默认值替换掉再把默认值存回磁盘。
pub fn create_settings(path: &Path) -> SettingsLoadOutcome {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return SettingsLoadOutcome::Loaded(fallback_settings(path, false));
        }
        Err(error) => {
            warn!(%error, path = %path.display(), "Failed to read settings file");
            return SettingsLoadOutcome::Corrupt {
                settings: fallback_settings(path, false),
                path: path.to_path_buf(),
            };
        }
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
/// 已确认落盘的最高 generation。防抖任务被进程退出取消时（runtime drop
/// 会掐掉 sleep 中的 async 任务），最后一笔靠 [`flush_pending_settings`]
/// 同步补写。
static SETTINGS_WRITTEN_GENERATION: AtomicU64 = AtomicU64::new(0);

/// 最近一次请求的快照登记（generation、内容、目标路径）：退出 flush 的
/// 数据源。防抖任务正常落盘后摘除自己的登记。
struct PendingSettingsSave {
    generation: u64,
    snapshot: Arc<Settings>,
    path: PathBuf,
}
static PENDING_SETTINGS_SAVE: Mutex<Option<PendingSettingsSave>> = Mutex::new(None);

/// 原子写盘：先写同目录临时文件再 rename 替换（同 session_io::save）——
/// 原地截断+重写会在进程中途死亡时留下截断文件，watcher 回读竞态还会把
/// 正常会话误标为 Corrupt。tmp 名带 generation：防抖不保证上一次写盘已
/// 结束，固定名会让两代写盘截断同一个 tmp 交错写。
fn write_settings_file(snapshot: &Settings, path: &Path, generation: u64) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("json.tmp{generation}"));
    File::create(&tmp)
        .and_then(|file| serde_json::to_writer_pretty(file, snapshot).map_err(|e| e.into()))
        .and_then(|()| fs::rename(&tmp, path))
        // 旧文件原样保留，只清理残留临时文件
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
}

/// 立即把 `settings` 推给播放线程与扫描器，再对磁盘写入做防抖（trailing edge，
/// 见 [`SETTINGS_SAVE_DEBOUNCE`]）。
pub fn save_settings(cx: &mut App, settings: &Settings) {
    let playback = cx.global::<PlaybackInterface>();
    playback.update_settings(settings.playback.clone());

    let scan = cx.global::<ScanInterface>();
    scan.update_settings(settings.scanning.clone());

    let path = cx.global::<SettingsGlobal>().path.clone();
    let snapshot = Arc::new(settings.clone());

    // 上面这些全局对象必须实时生效；只有磁盘写入会被合并为
    // 一次 trailing-edge 写入（与均衡器的保存同一套路）。
    let generation = SETTINGS_SAVE_GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    if let Ok(mut pending) = PENDING_SETTINGS_SAVE.lock() {
        *pending = Some(PendingSettingsSave {
            generation,
            snapshot: snapshot.clone(),
            path: path.clone(),
        });
    }
    crate::RUNTIME.spawn(async move {
        tokio::time::sleep(SETTINGS_SAVE_DEBOUNCE).await;
        if SETTINGS_SAVE_GENERATION.load(Ordering::Relaxed) != generation {
            return;
        }
        let result =
            tokio::task::spawn_blocking(move || write_settings_file(&snapshot, &path, generation))
                .await;
        let written = matches!(&result, Ok(Ok(())));
        match &result {
            Ok(Ok(())) => {
                SETTINGS_WRITTEN_GENERATION.fetch_max(generation, Ordering::Relaxed);
            }
            Ok(Err(e)) => warn!("Failed to save settings file: {e:?}"),
            Err(e) => warn!("settings save task failed: {e}"),
        }
        // 已落盘的登记摘除（可能已被更新的 generation 覆盖，只在相等时摘）。
        if written
            && let Ok(mut pending) = PENDING_SETTINGS_SAVE.lock()
            && pending.as_ref().is_some_and(|p| p.generation == generation)
        {
            *pending = None;
        }
    });
}

/// 进程退出路径的兜底：防抖窗口内最后一笔还没落盘时（runtime drop 会取
/// 消 sleep 中的防抖任务），同步补写。挂在主窗口 on_release（先于进程退
/// 出）与硬 cap 阀门的 exit(70) 前。flush 与防抖任务并发写同一内容时用
/// 不同 tmp 名（flush 后缀），rename 各自原子、字节一致，不会互相截断。
pub fn flush_pending_settings() {
    let Some(pending) = PENDING_SETTINGS_SAVE
        .lock()
        .ok()
        .and_then(|mut pending| pending.take())
    else {
        return;
    };
    if SETTINGS_WRITTEN_GENERATION.load(Ordering::Relaxed) >= pending.generation {
        return;
    }
    let generation = pending.generation;
    let tmp = pending
        .path
        .with_extension(format!("json.tmpflush{generation}"));
    let result = File::create(&tmp)
        .and_then(|file| {
            serde_json::to_writer_pretty(file, &*pending.snapshot).map_err(|e| e.into())
        })
        .and_then(|()| fs::rename(&tmp, &pending.path))
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        });
    if result.is_ok() {
        SETTINGS_WRITTEN_GENERATION.fetch_max(generation, Ordering::Relaxed);
    } else if let Err(e) = result {
        warn!("settings flush on exit failed: {e:?}");
    }
}

pub struct SettingsGlobal {
    pub model: Entity<Settings>,
    pub path: PathBuf,
    /// 启动时的首次加载发现设置文件损坏时为 `Some(path)`。
    /// 由 `build_models` 消费用来建立 `SettingsHealth`，之后为 `None`。
    pub initial_corrupt_path: Option<PathBuf>,
    /// 从不被读：存在的唯一意义是让 notify watcher 靠所有权保活——
    /// 丢弃它会注销 watch，从而静默断掉 settings.json 的热重载。
    #[allow(dead_code)]
    pub watcher: Option<Box<dyn Watcher>>,
}

impl Global for SettingsGlobal {}

/// 从 `path` 加载设置、发布 [`SettingsGlobal`]，并 watch 所在目录，
/// 使手工编辑的 `settings.json` 能被实时重读。
pub fn setup_settings(cx: &mut App, path: PathBuf) {
    let outcome = create_settings(&path);
    let initial_corrupt_path = match &outcome {
        SettingsLoadOutcome::Corrupt { path, .. } => Some(path.clone()),
        SettingsLoadOutcome::Loaded(_) => None,
    };
    let settings = cx.new(|_| outcome.into_settings());
    let settings_model = settings.clone(); // 供 watcher 闭包使用

    // 创建并配置 settings.json 的文件 watcher
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

            // settings.json 是被手工改的；1 s 轮询能在可感知时间内反应，
            // 又不会每秒多次唤醒主线程。
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

/// 应用文件 watcher 产出的新鲜 [`SettingsLoadOutcome`]。文件能干净解析时替换内存中的
/// `Settings` 并把健康度标为 `Ok`；文件损坏时保留现有 `Settings`（扫描器因此继续用
/// 最后一份已知良好的配置）并把健康度翻成 `Corrupt`。
fn apply_settings_outcome(
    cx: &mut Context<Settings>,
    current: &mut Settings,
    outcome: SettingsLoadOutcome,
) {
    use crate::ui::models::Models;

    let next_health = match outcome {
        SettingsLoadOutcome::Loaded(settings) => {
            // 把外部编辑派发给播放线程与扫描器；save_settings 自己会推送，
            // 且它的 reload diff 是空的
            if current.playback != settings.playback && cx.has_global::<PlaybackInterface>() {
                cx.global::<PlaybackInterface>()
                    .update_settings(settings.playback.clone());
            }
            if current.scanning != settings.scanning && cx.has_global::<ScanInterface>() {
                cx.global::<ScanInterface>()
                    .update_settings(settings.scanning.clone());
            }
            // prevent_idle 不在播放线程的 SettingsChanged 消费路径里：
            // 电源管理器需要自己那条 diff 推送，否则手工改的
            // settings.json 要等到重启才生效。
            if current.playback.prevent_idle != settings.playback.prevent_idle
                && cx.has_global::<crate::power::PowerManager>()
            {
                let power = cx.global::<crate::power::PowerManager>().clone();
                power.set_prevent_idle(cx, settings.playback.prevent_idle);
            }
            // 内容相同的 reload（我们自己保存的回声、notify 风暴）不得
            // 级联触发全应用的 refresh_windows 观察者
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

    /// 非 `NotFound` 的读取失败不得伪装成「没有文件」：那样 watcher 会拿默认值
    /// 覆盖生效中的设置（EQ、ReplayGain、扫描路径）再把默认值存回磁盘。
    #[test]
    fn create_settings_unreadable_file_reports_corrupt() {
        let dir = create_test_dir();
        let path = settings_path(&dir);
        fs::create_dir(&path).unwrap();
        assert!(matches!(
            create_settings(&path),
            SettingsLoadOutcome::Corrupt { path: reported, .. } if reported == path
        ));
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
