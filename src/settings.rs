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
    io::Write,
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

impl Settings {
    /// 修复手编 settings.json 可能带入的越界值（对照 equalizer.rs 的
    /// sanitize 先例：Deserialize 后钳位）。ReplayGain 前置增益的文档承诺
    /// -6.0..+6.0（replaygain.rs），但序列化侧只有 `#[serde(default)]`
    /// 没有钳位——越界值会直接进 `10^(dB/20)` 的指数（+20dB 即 10 倍
    /// 增益），回退前置增益同理；非有限值则让整个增益计算失控。
    fn sanitize(&mut self) {
        use crate::playback::dsp::equalizer::clamp_finite;
        self.playback.replaygain.preamp_db =
            clamp_finite(self.playback.replaygain.preamp_db, -6.0, 6.0, 0.0);
        self.playback.replaygain.fallback_preamp_db =
            clamp_finite(self.playback.replaygain.fallback_preamp_db, -6.0, 6.0, 0.0);
    }
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
    settings.sanitize();
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

/// 最近一次请求的快照登记（generation、内容、目标路径、登记时的文件字节
/// 基线）：退出 flush 的数据源。防抖任务正常落盘后摘除自己的登记。
struct PendingSettingsSave {
    generation: u64,
    /// 登记瞬间的 settings.json 字节快照：防抖醒来与退出 flush 写盘前
    /// 比对，文件已被动过（手编/他进程）即弃写——丢弃语义见防抖任务。
    baseline: Option<Vec<u8>>,
    snapshot: Arc<Settings>,
    path: PathBuf,
}
static PENDING_SETTINGS_SAVE: Mutex<Option<PendingSettingsSave>> = Mutex::new(None);

/// 本进程最近一次成功落盘的 settings.json 字节。防抖（`""` tag）与退出
/// flush（`"flush"` tag）两条通道唯一收口在 [`write_settings_file`]，
/// rename 成功即单点登记；供 [`file_unchanged_since_registration`] 把
/// 「登记基线之外只多出本进程自己的写盘」豁免为未动过。
static SETTINGS_LAST_SELF_WRITE: Mutex<Option<Vec<u8>>> = Mutex::new(None);

/// 进程内写盘互斥：防抖的「校验 + 代纪复查 + rename + 自写登记」收口在
/// [`commit_if_current`]、退出 flush 的写盘段收口在 [`flush_pending_settings`]，
/// 两处共用本锁。此前防抖经 spawn_blocking 写盘期间不再复查——慢写盘
/// （AV 实时扫描可放大到 100ms+）期间新代登记后，旧代写盘仍会把旧快照
/// rename 上去并覆盖 [`SETTINGS_LAST_SELF_WRITE`]；锁内代纪复查使「后到代
/// 必后 rename」成立。锁序单向：本锁 → [`SETTINGS_LAST_SELF_WRITE`]（后者的
/// 两个加锁点都不再引其它锁），无死锁。防抖写盘走 spawn_blocking 冷路径，
/// 无热路径影响。
pub(crate) static SETTINGS_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// 原子写盘：先写同目录临时文件再 rename 替换（同 session_io::save）——
/// 原地截断+重写会在进程中途死亡时留下截断文件，watcher 回读竞态还会把
/// 正常会话误标为 Corrupt。tmp 名带 generation：防抖不保证上一次写盘已
/// 结束，固定名会让两代写盘截断同一个 tmp 交错写。
/// `tmp_tag` 区分写盘来源：防抖路径用 `""`，退出前的强制 flush 用 `"flush"`，
/// 两条通道即使在同一个 generation 上也不会撞进同一个临时文件。
/// 序列化先落 Vec 再 write_all：rename 成功后要把字节登记进
/// [`SETTINGS_LAST_SELF_WRITE`]，两条写盘通道在同一个函数里完成登记。
fn write_settings_file(
    snapshot: &Settings,
    path: &Path,
    generation: u64,
    tmp_tag: &str,
) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("json.tmp{tmp_tag}{generation}"));
    let bytes = serde_json::to_vec_pretty(snapshot)?;
    File::create(&tmp)
        .and_then(|mut file| file.write_all(&bytes))
        .and_then(|()| fs::rename(&tmp, path))
        // 旧文件原样保留，只清理残留临时文件
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
        // rename 成功才登记自写字节；两通道单点覆盖
        .inspect(|_| {
            if let Ok(mut last) = SETTINGS_LAST_SELF_WRITE.lock() {
                *last = Some(bytes);
            }
        })
}

/// 写盘前校验：登记以来 settings.json 的字节没被动过。磁盘字节与登记基线
/// 一致算未动过；None（不存在/读不了）与登记时一致同样算"未动过"。比对
/// 字节而不是看 watcher 是否装载过：watcher 轮询最长 1s，还没轮到装载的
/// 手编同样必须拦下。基线不等时再看是否恰为本进程最近一次落盘的内容
/// （[`SETTINGS_LAST_SELF_WRITE`]）：上一代写盘可能在本代登记之后才 rename
/// 完成——防抖任务通过 generation 一次性检查后进入 spawn_blocking，写盘
/// 期间不再复查，本代登记读到的还是旧字节；AV 实时扫描会把该窗口放大到
/// 100ms+。没有这份豁免会把本代写盘误判为手编而弃写（第二次改动静默
/// 丢失 + UI 回跳）；真正的手编（字节既非基线亦非自写）仍拦下。锁毒化
/// 时豁免失效，按外部改动保守弃写。
fn file_unchanged_since_registration(path: &Path, baseline: &Option<Vec<u8>>) -> bool {
    let current = fs::read(path).ok();
    if current == *baseline {
        return true;
    }
    // 比较须在 guard 借用域内完成：guard 指向的字节不能带出闭包
    SETTINGS_LAST_SELF_WRITE
        .lock()
        .map(|last| {
            last.as_deref()
                .is_some_and(|bytes| current.as_deref() == Some(bytes))
        })
        .unwrap_or(false)
}

/// 防抖写盘的收口：持 [`SETTINGS_WRITE_LOCK`] 依次做
/// [`file_unchanged_since_registration`] 校验与代纪复查（
/// [`SETTINGS_SAVE_GENERATION`] 已被新代推进即弃写，写盘归新代），两关都过
/// 才 [`write_settings_file`]。校验与写盘刻意放同一临界区——评审确认锁内外
/// 皆安全，同临界区让读序一目了然，且代纪复查到 rename 之间不再有缝隙
/// （check-to-rename 原子化，防抖-vs-防抖确定性后到代胜出）。
/// 返回 Ok(false) = 弃写（原因已在此记录），Ok(Err) = 写盘 IO 失败。
fn commit_if_current(
    generation: u64,
    baseline: &Option<Vec<u8>>,
    snapshot: &Settings,
    path: &Path,
    tmp_tag: &str,
) -> std::io::Result<bool> {
    let Ok(_guard) = SETTINGS_WRITE_LOCK.lock() else {
        warn!("settings debounce drop: write lock poisoned");
        return Ok(false);
    };
    if !file_unchanged_since_registration(path, baseline) {
        info!("settings debounce drop: file changed externally during window");
        return Ok(false);
    }
    if SETTINGS_SAVE_GENERATION.load(Ordering::Relaxed) != generation {
        info!("settings debounce drop: newer generation holds the write");
        return Ok(false);
    }
    write_settings_file(snapshot, path, generation, tmp_tag).map(|()| true)
}

/// 摘除指定 generation 的 pending 登记（可能已被更新的 generation 覆盖，
/// 那笔归它自己的任务处理，只在相等时摘）。
fn take_pending_registration(generation: u64) {
    if let Ok(mut pending) = PENDING_SETTINGS_SAVE.lock()
        && pending.as_ref().is_some_and(|p| p.generation == generation)
    {
        *pending = None;
    }
}

/// 立即把 `settings` 推给播放线程与扫描器，再对磁盘写入做防抖（trailing edge，
/// 见 [`SETTINGS_SAVE_DEBOUNCE`]）。
pub fn save_settings(cx: &mut App, settings: &Settings) {
    let playback = cx.global::<PlaybackInterface>();
    playback.update_settings(settings.playback.clone());

    let scan = cx.global::<ScanInterface>();
    scan.update_settings(settings.scanning.clone());

    // 音频缓存开关不住在播放线程的消费路径里：设置一翻就要同时停掉在途下载。
    #[cfg(feature = "online_sources")]
    crate::media::audio_cache::set_audio_cache_enabled(settings.playback.audio_cache_enabled);

    let path = cx.global::<SettingsGlobal>().path.clone();
    let snapshot = Arc::new(settings.clone());

    // 上面这些全局对象必须实时生效；只有磁盘写入会被合并为
    // 一次 trailing-edge 写入（与均衡器的保存同一套路）。
    let generation = SETTINGS_SAVE_GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    // 登记瞬间的文件字节基线：防抖醒来时若文件已被手编动过则弃写（丢弃
    // 语义见防抖任务内注释）。读的是 KB 级配置文件，且调用方已在 300ms
    // UI 防抖之后——冷路径。
    let baseline = fs::read(&path).ok();
    if let Ok(mut pending) = PENDING_SETTINGS_SAVE.lock() {
        *pending = Some(PendingSettingsSave {
            generation,
            // 克隆一份给防抖任务自用：登记与异步校验两处都要这份基线
            // （KB 级配置，冷路径）。
            baseline: baseline.clone(),
            snapshot: snapshot.clone(),
            path: path.clone(),
        });
    }
    crate::RUNTIME.spawn(async move {
        tokio::time::sleep(SETTINGS_SAVE_DEBOUNCE).await;
        if SETTINGS_SAVE_GENERATION.load(Ordering::Relaxed) != generation {
            return;
        }
        let result = tokio::task::spawn_blocking(move || {
            // 写盘收口：校验 + 代纪复查 + rename 在写盘互斥锁内一次完成，
            // 「后到代必后 rename」。丢弃语义不变：磁盘上后写的文件是用户
            // 意图，内存为准（watcher 已把/会把装载刷进内存），这笔旧快照
            // 落盘只会把手编覆盖掉、再被回声把内存回滚，手编静默丢失。
            // 登记刻意保留：退出 flush 会带着同一校验再试（文件恢复原样则
            // 照常补写，仍是手编则同样弃写）。
            commit_if_current(generation, &baseline, &snapshot, &path, "")
        })
        .await;
        match result {
            Ok(Ok(true)) => {
                SETTINGS_WRITTEN_GENERATION.fetch_max(generation, Ordering::Relaxed);
                // 已落盘的登记摘除（可能已被更新的 generation 覆盖，只在
                // 相等时摘）。
                take_pending_registration(generation);
            }
            // 弃写原因（外部改动 / 新代持有写盘 / 锁毒化）已由
            // commit_if_current 记录。
            Ok(Ok(false)) => {}
            Ok(Err(e)) => warn!("Failed to save settings file: {e:?}"),
            Err(e) => warn!("settings save task failed: {e}"),
        }
    });
}

/// 进程退出路径的兜底：防抖窗口内最后一笔还没落盘时（runtime drop 会取
/// 消 sleep 中的防抖任务），同步补写。挂在主窗口 on_release（先于进程退
/// 出）与硬 cap 阀门的 exit(70) 前。写盘段与防抖收口同锁串行
/// （[`SETTINGS_WRITE_LOCK`]）；与防抖并发写同一内容时仍用不同 tmp 名
/// （flush 后缀），rename 各自原子、字节一致，不会互相截断。
/// 同时是两处退出路径的收听统计收口：先把 stats 模块在途的异步 flush
/// 排空再继续（见 stats::drain_inflight_flushes_for_exit）。
pub fn flush_pending_settings() {
    crate::stats::drain_inflight_flushes_for_exit();
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
    let Ok(_guard) = SETTINGS_WRITE_LOCK.lock() else {
        warn!("settings write lock poisoned; skipping exit flush");
        return;
    };
    // 有意不做代纪复查：pending 本就是最新登记的快照（退出路径没有更新的
    // 代可登记）；旧代防抖其后进锁，会被 commit_if_current 的锁内代纪复查
    // 弃写，写盘归本 flush。
    // 与防抖任务同一丢弃语义：登记以来文件被手编动过就不覆盖——磁盘上
    // 后写的文件是用户意图，内存为准。
    if !file_unchanged_since_registration(&pending.path, &pending.baseline) {
        info!("settings exit flush drop: file changed externally during window");
        return;
    }
    let generation = pending.generation;
    let result = write_settings_file(&pending.snapshot, &pending.path, generation, "flush");
    if result.is_ok() {
        SETTINGS_WRITTEN_GENERATION.fetch_max(generation, Ordering::Relaxed);
    } else if let Err(e) = result {
        warn!("settings flush on exit failed: {e:?}");
    }
}

/// 硬 cap 阀门 exit(70) 前的桌面歌词位置补写（main.rs 探针消费）：读当前
/// settings.json，只改 `interface.desktop_lyrics_pos` 后 tmp+rename 原子替换。
/// 不经 [`write_settings_file`]——那是整份快照的通道；tmp 名以 `"probe"`
/// tag 区分，与防抖 `""` / flush `"flush"` 不撞。返回是否成功替换。
/// rename 前先推进 [`SETTINGS_SAVE_GENERATION`]：尚未进入写盘临界区的在途
/// 防抖会在 [`commit_if_current`] 的锁内代纪复查处弃写（写盘归本补写）；
/// 已过复查的在途防抖与本补写的残余竞态退化为一次性位置回退（与 main.rs
/// 阀门注释里「桌面歌词位置不随 exit(70) 落盘」的已接受缺口同级）。
pub(crate) fn patch_desktop_lyrics_pos_on_exit(path: &Path, pos: [f64; 2]) -> bool {
    let Ok(contents) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return false;
    };
    value["interface"]["desktop_lyrics_pos"] = serde_json::json!([pos[0], pos[1]]);
    let Ok(pretty) = serde_json::to_string_pretty(&value) else {
        return false;
    };
    // 代纪推进即补写身份：其后进锁的防抖一律视为过代弃写
    let generation = SETTINGS_SAVE_GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    let tmp = path.with_extension(format!("json.tmpprobe{generation}"));
    File::create(&tmp)
        .and_then(|mut file| file.write_all(pretty.as_bytes()))
        .and_then(|()| fs::rename(&tmp, path))
        // 目标文件原样保留，只清理残留临时文件
        .inspect_err(|_| {
            let _ = fs::remove_file(&tmp);
        })
        .is_ok()
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

/// 发布 [`SettingsGlobal`] 并 watch 设置文件所在目录，使手工编辑的
/// `settings.json` 能被实时重读。设置文件的读取/解析已在别处完成：启动预取
/// 在 tokio 的 spawn_blocking 上并行做（见 `ui/app.rs` 的启动序列），启动
/// 关键路径上只做 gpui 侧的注册与 watch。watcher 回读仍走同步的
/// [`create_settings`]——那是 1s 轮询上的冷路径，不值得再拆。
pub fn setup_settings_with_outcome(cx: &mut App, path: PathBuf, outcome: SettingsLoadOutcome) {
    let initial_corrupt_path = match &outcome {
        SettingsLoadOutcome::Corrupt { path, .. } => Some(path.clone()),
        SettingsLoadOutcome::Loaded(_) => None,
    };
    let settings = cx.new(|_| outcome.into_settings());
    // 启动即按持久化的开关初始化媒体层，否则一份 `audio_cache_enabled=false`
    // 的 settings.json 要到用户再次改动设置才生效。
    #[cfg(feature = "online_sources")]
    crate::media::audio_cache::set_audio_cache_enabled(
        settings.read(cx).playback.audio_cache_enabled,
    );
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
                                // 我们自己的原子保存（tmp + rename 覆盖）在 Windows 上
                                // 同样以 Remove 呈现：文件还在就只是一次替换，回读拿到
                                // 的就是新内容。别把"用默认设置"写进日志误导排查。
                                if !path_for_watcher.exists() {
                                    info!("Settings file removed, using default settings");
                                }
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
            // desktop_lyrics 同样不在播放线程的 SettingsChanged 消费路径里：
            // 悬浮窗实态由 desktop_lyrics 模块自持，手编设置位后若不推送，
            // 开关按钮与窗口实态各执一词（位=true 而窗不存在）。设置位与
            // 位置都由调用方带入——本函数跑在 Settings 实体自身的 update
            // 闭包内，被调函数里不得再 read/update 该实体（重入 panic）。
            // 注意 pos 只在开关位翻转触发 open 时被消费：窗口开着时其自身
            // 位置即权威，手编仅改 pos 不会重定位已开的悬浮窗（重启或关开
            // 生效）——有意取舍，不是接线遗漏。
            if current.interface.desktop_lyrics != settings.interface.desktop_lyrics
                && cx.has_global::<crate::ui::desktop_lyrics::DesktopLyricsState>()
            {
                let on = settings.interface.desktop_lyrics;
                let pos = settings.interface.desktop_lyrics_pos;
                crate::ui::desktop_lyrics::apply_enabled(cx, on, pos);
            }
            // 音频缓存开关同样不在播放线程的 SettingsChanged 消费路径里：
            // 手编 settings.json 后必须自己推给媒体层，否则要等重启。
            #[cfg(feature = "online_sources")]
            {
                if current.playback.audio_cache_enabled != settings.playback.audio_cache_enabled {
                    crate::media::audio_cache::set_audio_cache_enabled(
                        settings.playback.audio_cache_enabled,
                    );
                }
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

    /// 手编的越界 ReplayGain 前置增益必须在读入时被钳回文档承诺的
    /// -6.0..+6.0（replaygain.rs），否则直接进 10^(dB/20) 的指数。
    #[test]
    fn create_settings_clamps_out_of_range_replaygain_preamp() {
        let dir = create_test_dir();
        fs::write(
            settings_path(&dir),
            r#"{"playback": {"replaygain": {"preamp_db": 100.0, "fallback_preamp_db": -100.0}}}"#,
        )
        .unwrap();

        let settings = create_settings(&settings_path(&dir)).into_settings();

        assert_eq!(settings.playback.replaygain.preamp_db, 6.0);
        assert_eq!(settings.playback.replaygain.fallback_preamp_db, -6.0);
    }

    /// 界内值原样通过：钳位不得改动合法设置。
    #[test]
    fn create_settings_keeps_in_range_replaygain_preamp() {
        let dir = create_test_dir();
        fs::write(
            settings_path(&dir),
            r#"{"playback": {"replaygain": {"preamp_db": -3.5, "fallback_preamp_db": 2.0}}}"#,
        )
        .unwrap();

        let settings = create_settings(&settings_path(&dir)).into_settings();

        assert_eq!(settings.playback.replaygain.preamp_db, -3.5);
        assert_eq!(settings.playback.replaygain.fallback_preamp_db, 2.0);
    }

    /// 串行化触碰写盘 statics 的测试（#[test] 并行跑会互踩）。
    static SETTINGS_STATICS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 登记 pending 之后文件被手编动过（字节变化）：退出 flush 必须弃写，
    /// 磁盘上后写的文件是用户意图，不得被登记时的旧快照覆盖。
    /// 覆盖防抖任务与 flush 共用的同一校验（file_unchanged_since_registration）。
    #[test]
    fn flush_pending_settings_drops_when_file_changed_under_pending() {
        let _guard = SETTINGS_STATICS_LOCK.lock().unwrap();
        let dir = create_test_dir();
        let path = settings_path(&dir);
        let old_bytes = b"{\"scanning\": {}}".to_vec();
        fs::write(&path, &old_bytes).unwrap();

        super::SETTINGS_SAVE_GENERATION.store(9, std::sync::atomic::Ordering::Relaxed);
        super::SETTINGS_WRITTEN_GENERATION.store(0, std::sync::atomic::Ordering::Relaxed);
        *super::PENDING_SETTINGS_SAVE.lock().unwrap() = Some(super::PendingSettingsSave {
            generation: 9,
            baseline: Some(old_bytes),
            snapshot: std::sync::Arc::new(Settings::default()),
            path: path.clone(),
        });

        // 登记后手编：文件字节变了（含"文件从无到有"的 None→Some 变体）
        let hand_edited = "{\"playback\": {\"always_repeat\": true}}";
        fs::write(&path, hand_edited).unwrap();
        super::flush_pending_settings();

        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(
            on_disk.contains("always_repeat"),
            "手编内容不得被旧快照覆盖"
        );
    }

    /// 文件自登记起未动过：退出 flush 照常补写 pending 快照。
    #[test]
    fn flush_pending_settings_writes_when_file_unchanged() {
        let _guard = SETTINGS_STATICS_LOCK.lock().unwrap();
        let dir = create_test_dir();
        let path = settings_path(&dir);

        super::SETTINGS_SAVE_GENERATION.store(10, std::sync::atomic::Ordering::Relaxed);
        super::SETTINGS_WRITTEN_GENERATION.store(0, std::sync::atomic::Ordering::Relaxed);
        *super::PENDING_SETTINGS_SAVE.lock().unwrap() = Some(super::PendingSettingsSave {
            generation: 10,
            // 登记时文件不存在（基线 None），到 flush 时仍不存在 = 未动过
            baseline: None,
            snapshot: std::sync::Arc::new(Settings::default()),
            path: path.clone(),
        });

        super::flush_pending_settings();

        assert!(path.exists(), "文件未动时退出 flush 必须补写");
        let written = create_settings(&path).into_settings();
        assert_eq!(written, Settings::default());
    }

    /// 跨代基线竞态的回归钉（自写字节豁免）：gen1 落盘在本代（gen2）登记
    /// 之后才 rename 完成——gen2 登记读到的还是 gen1 写盘前的旧字节，校验
    /// 时磁盘上摆的已是 gen1 的落盘内容。修复前判为外部改动而整笔弃写
    /// （第二次改动静默丢失 + UI 回跳）；修复后经 SETTINGS_LAST_SELF_WRITE
    /// 豁免照常补写。
    #[test]
    fn flush_pending_settings_passes_when_disk_only_holds_self_write() {
        let _guard = SETTINGS_STATICS_LOCK.lock().unwrap();
        let dir = create_test_dir();
        let path = settings_path(&dir);
        // gen1 写盘前的磁盘内容 = gen2 登记时的基线
        let stale_bytes = b"{\"scanning\": {}}".to_vec();
        fs::write(&path, &stale_bytes).unwrap();

        // gen1 落盘成功（与防抖通道同一个收口）：磁盘字节随之改变
        let mut gen1 = Settings::default();
        gen1.playback.always_repeat = true;
        super::write_settings_file(&gen1, &path, 21, "").unwrap();
        assert_ne!(
            fs::read(&path).unwrap(),
            stale_bytes,
            "gen1 落盘必须改变磁盘字节"
        );

        // gen2 在 gen1 的写盘窗口内登记，基线仍是 rename 前的旧字节
        super::SETTINGS_SAVE_GENERATION.store(22, std::sync::atomic::Ordering::Relaxed);
        super::SETTINGS_WRITTEN_GENERATION.store(0, std::sync::atomic::Ordering::Relaxed);
        let mut gen2 = Settings::default();
        gen2.interface.reduced_motion = true;
        *super::PENDING_SETTINGS_SAVE.lock().unwrap() = Some(super::PendingSettingsSave {
            generation: 22,
            baseline: Some(stale_bytes),
            snapshot: std::sync::Arc::new(gen2),
            path: path.clone(),
        });

        super::flush_pending_settings();

        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(
            on_disk.contains("\"reduced_motion\": true"),
            "gen2 写盘必须经自写豁免通过校验"
        );
        assert!(
            on_disk.contains("\"always_repeat\": false"),
            "gen2 落盘应覆盖 gen1 的 always_repeat=true"
        );
    }

    /// 第三方手编的字节（既非登记基线亦非本进程自写）仍必须弃写：自写
    /// 豁免不得把真正的手编放行。
    #[test]
    fn flush_pending_settings_still_drops_on_third_party_bytes() {
        let _guard = SETTINGS_STATICS_LOCK.lock().unwrap();
        let dir = create_test_dir();
        let path = settings_path(&dir);

        // 先让本进程成功落盘一次，SETTINGS_LAST_SELF_WRITE 持有自写字节
        let mut self_written = Settings::default();
        self_written.playback.always_repeat = true;
        super::write_settings_file(&self_written, &path, 31, "").unwrap();

        // 第三方手编：单行 JSON，与 pretty 序列化的自写字节必然不同
        let hand_edited = "{\"playback\": {\"prev_track_jump_first\": true}}";
        fs::write(&path, hand_edited).unwrap();

        super::SETTINGS_SAVE_GENERATION.store(32, std::sync::atomic::Ordering::Relaxed);
        super::SETTINGS_WRITTEN_GENERATION.store(0, std::sync::atomic::Ordering::Relaxed);
        *super::PENDING_SETTINGS_SAVE.lock().unwrap() = Some(super::PendingSettingsSave {
            generation: 32,
            baseline: Some(b"{\"scanning\": {}}".to_vec()),
            snapshot: std::sync::Arc::new(Settings::default()),
            path: path.clone(),
        });

        super::flush_pending_settings();

        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(
            on_disk.contains("prev_track_jump_first"),
            "第三方手编不得被旧快照覆盖"
        );
    }

    /// 防抖-vs-防抖在途窗的回归钉：新代（gen 2）已登记并落盘后，旧代
    /// （gen 1）的慢写盘才进写盘临界区——锁内代纪复查必须弃写，磁盘保持
    /// 新生代字节、SETTINGS_LAST_SELF_WRITE 不被旧代覆盖。修复前防抖经
    /// spawn_blocking 写盘期间不再复查，旧代会把旧快照 rename 上去并覆盖
    /// 自写登记（第二次改动静默丢失）。
    #[test]
    fn commit_if_current_drops_stale_generation_and_keeps_new_generation_bytes() {
        let _guard = SETTINGS_STATICS_LOCK.lock().unwrap();
        let dir = create_test_dir();
        let path = settings_path(&dir);

        // 新代 gen 2 落盘成功（write_settings_file 同时登记自写字节）
        let mut newer = Settings::default();
        newer.playback.always_repeat = true;
        super::write_settings_file(&newer, &path, 2, "").unwrap();
        super::SETTINGS_SAVE_GENERATION.store(2, std::sync::atomic::Ordering::Relaxed);

        // 旧代 gen 1 的写盘此刻才进临界区：文件校验经自写豁免通过（磁盘
        // 上是新代自写字节，基线是它登记时的旧字节），随后被锁内代纪复查
        // 拦下
        let stale_bytes = b"{\"scanning\": {}}".to_vec();
        let committed =
            super::commit_if_current(1, &Some(stale_bytes), &Settings::default(), &path, "")
                .unwrap();
        assert!(!committed, "旧代写盘必须被锁内代纪复查弃写");

        let on_disk = fs::read_to_string(&path).unwrap();
        assert!(
            on_disk.contains("\"always_repeat\": true"),
            "磁盘必须仍是新生代字节"
        );
        let last_self_write = super::SETTINGS_LAST_SELF_WRITE.lock().unwrap().clone();
        assert_eq!(
            last_self_write,
            Some(serde_json::to_vec_pretty(&newer).unwrap()),
            "SETTINGS_LAST_SELF_WRITE 不得被旧代覆盖"
        );
    }

    /// 补写（probe 通道）tmp 阶段失败的原子性钉：tmp 路径被目录占住时
    /// File::create 失败，rename 不得发生，目标文件保持原字节
    /// （仿 session_io.rs 的 save_never_leaves_a_half_written_target）。
    #[test]
    fn probe_patch_leaves_target_intact_when_tmp_stage_fails() {
        let _guard = SETTINGS_STATICS_LOCK.lock().unwrap();
        let dir = create_test_dir();
        let path = settings_path(&dir);
        fs::write(&path, r#"{"interface": {}}"#).unwrap();
        let before = fs::read(&path).unwrap();

        // tmp 名含 fetch_add 后的 generation：先钉住代纪再占住 tmp 路径
        super::SETTINGS_SAVE_GENERATION.store(40, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension("json.tmpprobe41");
        fs::create_dir(&tmp).unwrap();

        assert!(
            !super::patch_desktop_lyrics_pos_on_exit(&path, [7.0, 8.0]),
            "tmp 阶段失败必须报未替换"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "tmp 阶段失败目标文件必须原样"
        );
    }
}
