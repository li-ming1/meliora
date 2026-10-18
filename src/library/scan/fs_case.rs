//! 大小写敏感性检查与用于比较的路径折叠。
//! 折叠后的路径只用于比较——绝不用于 I/O 或存储。

use crate::sync::RecoveredRwLock;
use std::{
    borrow::Cow,
    sync::{LazyLock, RwLock},
};

use camino::{Utf8Path, Utf8PathBuf};
use rustc_hash::FxHashMap;

const OS_DEFAULT_CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

static CASE_CACHE: LazyLock<RwLock<FxHashMap<Utf8PathBuf, bool>>> =
    LazyLock::new(|| RwLock::new(FxHashMap::default()));

/// `path` 所在文件系统是否大小写不敏感。每个卷只探测一次，探测不出结果时取操作系统默认值。
pub fn is_case_insensitive(path: &Utf8Path) -> bool {
    let Some(key) = volume_key(path) else {
        return OS_DEFAULT_CASE_INSENSITIVE;
    };
    if let Some(result) = CASE_CACHE.read_recovered().get(&key) {
        return *result;
    }

    let Some(anchor) = nearest_existing_dir(path) else {
        return OS_DEFAULT_CASE_INSENSITIVE;
    };

    // 路径与其父目录都不存在时，别去探测我们从未检查过的祖先目录
    if anchor != path && path.parent().is_none_or(|p| p != anchor) {
        return OS_DEFAULT_CASE_INSENSITIVE;
    }

    let Some(result) = probe_case_insensitive(&anchor) else {
        return OS_DEFAULT_CASE_INSENSITIVE; // 结论不定——不写缓存
    };
    CASE_CACHE.write_recovered().insert(key, result);
    result
}

fn nearest_existing_dir(path: &Utf8Path) -> Option<Utf8PathBuf> {
    let mut current = Some(path);
    while let Some(p) = current {
        if p.is_dir() {
            return Some(p.to_path_buf());
        }
        current = p.parent();
    }
    None
}

/// Windows 卷键：盘符或 UNC 共享。
#[cfg(windows)]
fn volume_key(path: &Utf8Path) -> Option<Utf8PathBuf> {
    let stripped = strip_verbatim(path.as_str());
    if let Some(rest) = stripped.strip_prefix(r"\\") {
        let mut parts = rest.splitn(3, '\\');
        let server = parts.next().unwrap_or_default();
        let share = parts.next().unwrap_or_default();
        return Some(Utf8PathBuf::from(
            format!(r"\\{server}\{share}").to_lowercase(),
        ));
    }
    if stripped.len() >= 2 && stripped.as_bytes()[1] == b':' {
        return Some(Utf8PathBuf::from(stripped[..2].to_lowercase()));
    }
    Some(Utf8PathBuf::from(stripped.into_owned().to_lowercase()))
}

/// Unix 卷键：`path` 所在文件系统的设备号。stat 失败时为 `None`。
#[cfg(unix)]
fn volume_key(path: &Utf8Path) -> Option<Utf8PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let dev = std::fs::metadata(path.as_std_path()).ok()?.dev();
    Some(Utf8PathBuf::from(dev.to_string()))
}

/// 检查 `dir` 所在文件系统是否大小写不敏感——找一个大小写翻转的条目，或建一个临时文件。
fn probe_case_insensitive(dir: &Utf8Path) -> Option<bool> {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let swapped = swap_case(&name);
            // 没有可翻转的 ASCII 字母——这个名字探测不了
            if swapped == name {
                continue;
            }
            let Ok(original) = entry.path().canonicalize() else {
                continue;
            };
            match dir.join(&swapped).canonicalize_utf8() {
                Ok(flipped) => return Some(flipped.as_std_path() == original),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some(false),
                Err(_) => continue,
            }
        }
    }
    temp_file_probe(dir)
}

fn temp_file_probe(dir: &Utf8Path) -> Option<bool> {
    let name = format!(".meliora-case-probe-{:x}", rand::random::<u64>());
    let path = dir.join(&name);
    std::fs::File::create(&path).ok()?;
    let result = dir.join(name.to_uppercase()).try_exists();
    let _ = std::fs::remove_file(&path);
    result.ok()
}

/// 只翻转 ASCII 字母大小写。Unicode 大小写折叠会改变长度，与文件系统对不上。
fn swap_case(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' => c.to_ascii_uppercase(),
            'A'..='Z' => c.to_ascii_lowercase(),
            _ => c,
        })
        .collect()
}

/// 剥掉 Windows 的 \\?\ 前缀，让规范化路径与配置路径对得上。仅用于比较。
fn strip_verbatim(path: &str) -> Cow<'_, str> {
    if !cfg!(windows) {
        return Cow::Borrowed(path);
    }
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        Cow::Owned(format!(r"\\{rest}"))
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        Cow::Borrowed(rest)
    } else {
        Cow::Borrowed(path)
    }
}

/// 为折叠前缀匹配而折叠记录 key：剥掉 `\\?\` 并无条件转小写。
/// [`fold_path`] 才是感知大小写敏感性的变体。
pub fn fold_key(path: &Utf8Path) -> String {
    strip_verbatim(path.as_str()).to_lowercase()
}

/// 已经由 [`fold_key`] 折叠过的 key 与同样折叠过的前缀做前缀匹配——调用方因此
/// 可以折一次就与多个前缀比对。
pub fn folded_starts_with(folded_key: &str, folded_prefix: &str) -> bool {
    let mut key_chars = folded_key.chars();
    for expected in folded_prefix.chars() {
        if key_chars.next() != Some(expected) {
            return false;
        }
    }
    // 前缀必须恰好在路径分量边界处结束
    key_chars.next().is_none_or(|c| c == '/' || c == '\\')
}

/// 构造比较用的 key——剥 \\?\，并在大小写不敏感的卷上折叠大小写。
pub fn fold_path(path: &Utf8Path) -> Utf8PathBuf {
    let normalized = strip_verbatim(path.as_str());
    if is_case_insensitive(path) {
        Utf8PathBuf::from(normalized.to_lowercase())
    } else {
        Utf8PathBuf::from(normalized.into_owned())
    }
}

/// 按 [`fold_path`] 判定的路径相等：拼写完全相同直接算等，否则比较折叠后的形态。
pub fn paths_equal(a: &Utf8Path, b: &Utf8Path) -> bool {
    a == b || fold_path(a) == fold_path(b)
}

/// 两条路径是否为同一个文件（设备号与 inode 相同）。任一条 stat 不到即为 false。
pub fn same_file(a: &Utf8Path, b: &Utf8Path) -> bool {
    same_file::is_same_file(a.as_std_path(), b.as_std_path()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDir;

    #[test]
    fn verbatim_prefixes_are_normalized_on_windows() {
        if !cfg!(windows) {
            return;
        }
        assert_eq!(
            strip_verbatim(r"\\?\C:\Music\track.flac").as_ref(),
            r"C:\Music\track.flac"
        );
        assert_eq!(
            strip_verbatim(r"\\?\UNC\server\share\track.flac").as_ref(),
            r"\\server\share\track.flac"
        );
    }

    #[test]
    fn probe_agrees_with_filesystem_behavior() {
        let dir = TestDir::new("fs-case-test");
        std::fs::write(dir.join("Track.flac"), b"").unwrap();
        let wrong_case = dir.utf8_join("track.flac");
        assert_eq!(
            is_case_insensitive(&dir.utf8_path()),
            wrong_case.try_exists().unwrap()
        );
    }

    #[test]
    fn swap_case_flips_ascii_only() {
        assert_eq!(swap_case("Track.FLAC"), "tRACK.flac");
        assert_eq!(swap_case("Straße"), "sTRAßE");
        assert_eq!(swap_case("İstanbul.mp3"), "İSTANBUL.MP3");
    }

    #[test]
    fn probe_is_not_fooled_by_non_ascii_names() {
        let dir = TestDir::new("fs-case-test");
        std::fs::write(dir.join("Straße.flac"), b"").unwrap();
        let expected = dir.utf8_join("STRAßE.flac").try_exists().unwrap();
        assert_eq!(probe_case_insensitive(&dir.utf8_path()), Some(expected));
    }

    #[test]
    fn same_file_matches_case_variants_only_when_insensitive() {
        let dir = TestDir::new("fs-case-test");
        std::fs::write(dir.join("Track.flac"), b"").unwrap();
        let a = dir.utf8_join("Track.flac");
        let b = dir.utf8_join("TRACK.flac");
        assert_eq!(same_file(&a, &b), is_case_insensitive(&a));
    }

    #[test]
    fn same_file_distinguishes_different_files() {
        let dir = TestDir::new("fs-case-test");
        std::fs::write(dir.join("a.flac"), b"").unwrap();
        std::fs::write(dir.join("b.flac"), b"").unwrap();
        assert!(!same_file(
            &dir.utf8_join("a.flac"),
            &dir.utf8_join("b.flac")
        ));
        assert!(!same_file(
            &dir.utf8_join("a.flac"),
            &dir.utf8_join("missing.flac")
        ));
    }

    #[test]
    fn case_variants_compare_equal_when_volume_is_insensitive() {
        let dir = TestDir::new("fs-case-test");
        let a = dir.utf8_join("Track.flac");
        let b = dir.utf8_join("track.flac");
        assert_eq!(paths_equal(&a, &b), is_case_insensitive(&a));
    }

    #[test]
    fn folded_starts_with_matches_on_component_boundaries_only() {
        let prefix = fold_key(Utf8Path::new("/music/artist"));
        assert!(folded_starts_with(
            &fold_key(Utf8Path::new("/Music/Artist/album/track.flac")),
            &prefix
        ));
        assert!(folded_starts_with(
            &fold_key(Utf8Path::new("/music/artist")),
            &prefix
        ));
        assert!(!folded_starts_with(
            &fold_key(Utf8Path::new("/Music/Artist2/track.flac")),
            &prefix
        ));
        assert!(!folded_starts_with(
            &fold_key(Utf8Path::new("/music-other/track.flac")),
            &fold_key(Utf8Path::new("/music"))
        ));
        assert!(!folded_starts_with(
            &fold_key(Utf8Path::new("/music")),
            &prefix
        ));
    }

    #[test]
    fn folded_starts_with_folds_unicode_like_fold_path() {
        let prefix = fold_key(Utf8Path::new("/müsik/straße"));
        assert!(folded_starts_with(
            &fold_key(Utf8Path::new("/MÜSIK/STRAßE/track.flac")),
            &prefix
        ));
        assert!(!folded_starts_with(
            &fold_key(Utf8Path::new("/MUSIK/STRASSE/track.flac")),
            &prefix
        ));
    }

    #[test]
    fn folded_starts_with_folds_final_sigma_like_fold_path() {
        // str::to_lowercase 会把词尾的 Σ 映射成 ς，char::to_lowercase 不会
        let prefix = "/music/AΣ".to_lowercase();
        assert_eq!(prefix, "/music/aς");
        assert!(folded_starts_with(
            &fold_key(Utf8Path::new("/music/AΣ/track.flac")),
            &prefix
        ));
    }

    #[test]
    fn folded_paths_prefix_match_case_variant_roots() {
        let dir = TestDir::new("fs-case-test");
        if !is_case_insensitive(&dir.utf8_path()) {
            return;
        }
        let root = fold_path(&dir.utf8_path());
        let variant = Utf8PathBuf::from(dir.utf8_path().as_str().to_uppercase()).join("track.flac");
        assert!(fold_path(&variant).starts_with(&root));
    }
}

#[cfg(test)]
mod bench {
    //! 审计发现 F053 的耗时探针：[`folded_starts_with`] 走两个逐字符迭代器，而
    //! `discover/targeted.rs::index_records_under` 在每条记录只做一次 `fold_key`
    //! 之后，对每个 (记录, 目标) 组合都调它一次——在六位数的记录表上做定向重扫，
    //! 要付约 1e8 次逐字符比较外加每条记录一次 `to_lowercase` 分配。本 bench 就在
    //! 这个确切的循环形态下把当前实现与一个字节级等价实现对比计时，并顺带量出每条
    //! 记录的 `fold_key` 那一遍的大小。
    //!
    //! 为什么字节变体是同一个谓词（审计论证，下面在运行时对计时扫描要跑的每一对
    //! 输入都做了校验）：两侧都已由 `fold_key` / `fold_path` 折叠过，即合法 UTF-8
    //! 且匹配从字符边界开始。UTF-8 对每个字符的编码唯一，所以相等的字节前缀蕴含
    //! 相等的字符前缀（反之亦然）；`/` 与 `\` 是纯 ASCII，而 UTF-8 多字节序列只含
    //! >= 0x80 的字节，因此直接测前缀之后的那个原始字节，恰好就是「下一个字符是
    //! 路径分隔符」的分量检查。
    //!
    //! 已从常规测试运行中排除——耗时依赖环境，只作为给人读的报告才有意义：
    //!
    //! ```text
    //! cargo test --release folded_prefix -- --ignored --nocapture
    //! ```

    use std::{hint::black_box, time::Instant};

    use camino::Utf8Path;

    use super::{fold_key, folded_starts_with};

    /// 模拟的 `scan_record` 条目数
    const KEYS: usize = 100_000;
    /// 与每条记录都比一次的折叠目标前缀数
    const TARGETS: usize = 1_000;
    /// 约 10% 的目标恰好命中一条生成的 key
    const MATCHING_TARGETS: usize = 100;
    /// 每隔 N 条记录的 key 带 CJK 路径段（多字节 UTF-8）
    const MULTIBYTE_STRIDE: usize = 20;
    const MEASURED_RUNS: usize = 3;

    /// [`folded_starts_with`] 的字节级等价实现，用于已折叠的输入（两侧都是
    /// `to_lowercase` 的产物，所有调用方都如此产出）。前缀测试是用
    /// `[u8]::starts_with` 做的 memcmp；分量边界检查直接测前缀之后的那个原始字节，
    /// 这是精确的，因为等于 `/` 或 `\` 的字节绝不可能出现在 UTF-8 多字节序列内部。
    fn folded_starts_with_byte(folded_key: &str, folded_prefix: &str) -> bool {
        let key = folded_key.as_bytes();
        if !key.starts_with(folded_prefix.as_bytes()) {
            return false;
        }
        key.get(folded_prefix.len())
            .is_none_or(|&b| b == b'/' || b == b'\\')
    }

    /// Windows 风格的录制路径，带扫描器实际存下的 verbatim 前缀；每隔
    /// `MULTIBYTE_STRIDE` 条用一次 CJK 段，让多字节 UTF-8 同时出现在命中与未命中
    /// 两条路径上（占比 5%）。
    fn raw_key(i: usize) -> String {
        let track = i % 1_000;
        if i.is_multiple_of(MULTIBYTE_STRIDE) {
            format!(r"\\?\D:\Music\艺术家-{i:06}\专辑-{i:06}\曲目-{track:03}.flac")
        } else {
            format!(r"\\?\D:\Music\artist-{i:06}\album-{i:06}\track-{track:03}.flac")
        }
    }

    fn folded_target(stem: &str, id: usize) -> String {
        fold_key(Utf8Path::new(&format!("{stem}{id:06}")))
    }

    /// TARGETS 个折叠前缀：MATCHING_TARGETS 个恰好命中一条生成的 key（一半 ASCII、
    /// 一半 CJK 拼写），其余为未命中，分成三种分歧形态——不存在的歌手 id（在第一个
    /// 数字处分歧）、它的 CJK 孪生，以及只以 CJK key 存在的 id 的 ASCII 拼写（紧接
    /// 根路径之后分歧）。用确定性洗牌打乱，让首次命中的位置像遍历集合那样变化。
    fn build_targets() -> Vec<String> {
        let mut targets = Vec::with_capacity(TARGETS);
        for k in 0..MATCHING_TARGETS / 2 {
            let ascii_id = k * MULTIBYTE_STRIDE + 1;
            targets.push(folded_target(r"\\?\D:\Music\artist-", ascii_id));
            let cjk_id = k * MULTIBYTE_STRIDE;
            targets.push(folded_target(r"\\?\D:\Music\艺术家-", cjk_id));
        }
        for j in 0..TARGETS - MATCHING_TARGETS {
            let id = KEYS + j;
            match j % 3 {
                0 => targets.push(folded_target(r"\\?\D:\Music\artist-", id)),
                1 => targets.push(folded_target(r"\\?\D:\Music\艺术家-", id)),
                _ => {
                    let cjk_only_id = j * MULTIBYTE_STRIDE;
                    targets.push(folded_target(r"\\?\D:\Music\artist-", cjk_only_id));
                }
            }
        }
        for i in (1..targets.len()).rev() {
            targets.swap(i, (i * 7 + 3) % (i + 1));
        }
        targets
    }

    /// `index_records_under` 的确切循环形态（这里用确定性的目标顺序代替
    /// `FxHashSet` 的遍历顺序）：每条记录对每个目标比一次，首次命中即提前退出。
    fn scan_current(keys: &[String], targets: &[String]) -> usize {
        let mut hits = 0usize;
        'records: for key in black_box(keys) {
            for target in black_box(targets) {
                if folded_starts_with(key, target) {
                    hits += 1;
                    continue 'records;
                }
            }
        }
        hits
    }

    /// 同样的循环，换用字节级谓词。
    fn scan_byte_level(keys: &[String], targets: &[String]) -> usize {
        let mut hits = 0usize;
        'records: for key in black_box(keys) {
            for target in black_box(targets) {
                if folded_starts_with_byte(key, target) {
                    hits += 1;
                    continue 'records;
                }
            }
        }
        hits
    }

    /// 先跑一次预热，再跑 MEASURED_RUNS 轮计时并打印 min/mean，返回
    /// (最小 ms, 平均 ms, 最后一轮结果)。报告口径取 min——受噪声影响最小的样本。
    fn time_runs<T>(name: &str, mut f: impl FnMut() -> T) -> (f64, f64, T) {
        f(); // 预热：不计入耗时，结果也不读
        let mut samples = Vec::with_capacity(MEASURED_RUNS);
        let mut result = None;
        for _ in 0..MEASURED_RUNS {
            let start = Instant::now();
            result = Some(f());
            samples.push(start.elapsed().as_secs_f64() * 1_000.0);
        }
        let min = samples.iter().copied().fold(f64::INFINITY, f64::min);
        let mean = samples.iter().sum::<f64>() / MEASURED_RUNS as f64;
        println!("{name:<46} min={min:>8.1}ms  mean={mean:>8.1}ms");
        (min, mean, result.expect("MEASURED_RUNS >= 1"))
    }

    fn assert_agree(key: &str, prefix: &str) {
        assert_eq!(
            folded_starts_with(key, prefix),
            folded_starts_with_byte(key, prefix),
            "byte variant diverged: key={key:?} prefix={prefix:?}"
        );
    }

    /// 数据集到不了的多字节与边界用例：空输入、前缀之后紧跟一个 UTF-8 首字节、
    /// 预组合与分解形式的带音符字母，以及两侧都从 `to_lowercase` 得到的词尾 sigma 折叠。
    fn verify_edge_cases() {
        for (key, prefix) in [
            ("", ""),
            ("", "a"),
            ("a", ""),
            ("abc", ""),
            ("\\", ""),
            ("a/b", "a"),
            ("a\\b", "a"),
            ("ab", "a"),
            ("a/b", "a/"),
            ("a/", "a/"),
            ("abcd", "ab"),
            ("", "abcd"),
            ("艺", "艺术"),
            ("艺术家", "艺术家"),
            ("艺术/曲目", "艺术"),
            ("艺术家-000042\\专辑-000042\\曲目.flac", "艺术家-000042"),
            ("艺术家-000042\\专辑", "artist-000042"),
            ("artist-000042\\x", "艺术家"),
            ("ae\u{301}/x", "aé"),
            ("aé/x", "aé"),
        ] {
            assert_agree(key, prefix);
        }
        // 前缀之后的原始字节是 UTF-8 首字节时，不能被读成分隔符（审计的关键用例）
        assert!(!folded_starts_with("abc中/x", "abc"));
        assert!(!folded_starts_with_byte("abc中/x", "abc"));
        assert!(folded_starts_with("abc中/x", "abc中"));
        assert!(folded_starts_with_byte("abc中/x", "abc中"));
        // 词尾 sigma：两侧都严格按 fold_key 的方式折叠
        let sigma_key = "/music/ΑΣ/x".to_lowercase();
        let sigma_prefix = "/music/ΑΣ".to_lowercase();
        assert_eq!(sigma_prefix, "/music/ας");
        assert_agree(&sigma_key, &sigma_prefix);
        assert_agree("/music/ΑΣ/x", "/music/ας");
    }

    #[test]
    #[ignore = "timing report, run with --ignored --nocapture"]
    fn report_folded_prefix_match_costs() {
        let raw_keys: Vec<String> = (0..KEYS).map(raw_key).collect();
        let targets = build_targets();
        println!(
            "dataset: {KEYS} record keys ({}% CJK segments) x {TARGETS} folded targets, \
             {MATCHING_TARGETS} matching exactly one key",
            100 / MULTIBYTE_STRIDE
        );

        // 先验等价性：先定点边界用例，再覆盖下面计时扫描会跑的每一对 (key, target)。
        verify_edge_cases();
        let keys: Vec<String> = raw_keys
            .iter()
            .map(|raw| fold_key(Utf8Path::new(raw)))
            .collect();
        let mut matches = 0usize;
        for key in &keys {
            for target in &targets {
                let char_result = folded_starts_with(key, target);
                assert_eq!(
                    char_result,
                    folded_starts_with_byte(key, target),
                    "byte variant diverged: key={key:?} prefix={target:?}"
                );
                if char_result {
                    matches += 1;
                }
            }
        }
        assert_eq!(matches, MATCHING_TARGETS, "constructed dataset drifted");
        let avg_key_len = keys.iter().map(String::len).sum::<usize>() / keys.len();
        let avg_target_len = targets.iter().map(String::len).sum::<usize>() / targets.len();
        println!(
            "equivalence: {KEYS} x {TARGETS} pairs agree, {matches} matches; \
             avg folded key {avg_key_len}B, target {avg_target_len}B"
        );

        // 每条记录的折叠那一遍，完全按 index_records_under 的实际付法：每条记录一次
        // fold_key（strip_verbatim + to_lowercase 分配）
        let (fold_min, _fold_mean, _) =
            time_runs("fold_key pass (1 strip+lowercase alloc/record)", || {
                raw_keys
                    .iter()
                    .map(|raw| fold_key(Utf8Path::new(raw)))
                    .collect::<Vec<String>>()
            });
        // 分配下限：只跑 to_lowercase，把 strip/Utf8Path 分派与每条记录的 String
        // 分配分离开来
        let (lower_min, _lower_mean, _) = time_runs("to_lowercase only (allocation floor)", || {
            raw_keys
                .iter()
                .map(|raw| raw.to_lowercase())
                .collect::<Vec<String>>()
        });

        let (char_min, _char_mean, char_hits) =
            time_runs("folded_starts_with scan (current, per-char)", || {
                scan_current(&keys, &targets)
            });
        assert_eq!(char_hits, MATCHING_TARGETS);
        let (byte_min, _byte_mean, byte_hits) =
            time_runs("folded_starts_with scan (byte-level)", || {
                scan_byte_level(&keys, &targets)
            });
        assert_eq!(byte_hits, MATCHING_TARGETS);

        println!(
            "per-call average (1e8 calls/scan): char {:.1}ns vs byte {:.1}ns",
            char_min * 1e6 / (KEYS * TARGETS) as f64,
            byte_min * 1e6 / (KEYS * TARGETS) as f64,
        );
        println!(
            "fold_key breakdown: to_lowercase floor {lower_min:.0} ms, \
             strip_verbatim+Utf8Path dispatch ~{:.0} ms",
            fold_min - lower_min
        );
        println!(
            "fold_key share of current index-build (fold+scan): {:.1}%",
            fold_min / (fold_min + char_min) * 100.0
        );
        println!(
            "index-build totals (fold + scan): current {:.0} ms vs byte-level {:.0} ms",
            fold_min + char_min,
            fold_min + byte_min
        );
        println!("speedup (scan, min-of-runs): {:.1}x", char_min / byte_min);
        println!(
            "bench_fs_case: 100k×1k 现实现 {char_min:.0} ms vs 字节级 {byte_min:.0} ms（{:.0}% 加速）",
            (1.0 - byte_min / char_min) * 100.0
        );
    }
}
