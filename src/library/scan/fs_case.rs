//! Case sensitivity checks and path folding for comparisons.
//! Folded paths are for comparison only - never use them for I/O or storage.

use std::{
    borrow::Cow,
    sync::{LazyLock, RwLock},
};

use camino::{Utf8Path, Utf8PathBuf};
use rustc_hash::FxHashMap;

const OS_DEFAULT_CASE_INSENSITIVE: bool = cfg!(any(windows, target_os = "macos"));

static CASE_CACHE: LazyLock<RwLock<FxHashMap<Utf8PathBuf, bool>>> =
    LazyLock::new(|| RwLock::new(FxHashMap::default()));

/// Whether the filesystem for `path` is case-insensitive. Probed once per volume, OS default if unknown.
pub fn is_case_insensitive(path: &Utf8Path) -> bool {
    let Some(key) = volume_key(path) else {
        return OS_DEFAULT_CASE_INSENSITIVE;
    };
    if let Some(result) = CASE_CACHE
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
    {
        return *result;
    }

    let Some(anchor) = nearest_existing_dir(path) else {
        return OS_DEFAULT_CASE_INSENSITIVE;
    };

    // if the path and its parent are missing, don't probe some ancestor we never checked
    if anchor != path && path.parent().is_none_or(|p| p != anchor) {
        return OS_DEFAULT_CASE_INSENSITIVE;
    }

    let Some(result) = probe_case_insensitive(&anchor) else {
        return OS_DEFAULT_CASE_INSENSITIVE; // inconclusive - don't cache
    };
    CASE_CACHE
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, result);
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

/// Windows volume key: drive letter or UNC share.
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

/// Unix volume key: device id of `path`'s filesystem. `None` if stat fails.
#[cfg(unix)]
fn volume_key(path: &Utf8Path) -> Option<Utf8PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let dev = std::fs::metadata(path.as_std_path()).ok()?.dev();
    Some(Utf8PathBuf::from(dev.to_string()))
}

/// Check whether `dir`'s filesystem is case-insensitive - look for a flipped-case entry, or create a temp file.
fn probe_case_insensitive(dir: &Utf8Path) -> Option<bool> {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let swapped = swap_case(&name);
            // no ASCII letters to flip - can't probe with this name
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

/// Swap ASCII letter case only. Unicode case folds can change length and won't match the filesystem.
fn swap_case(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' => c.to_ascii_uppercase(),
            'A'..='Z' => c.to_ascii_lowercase(),
            _ => c,
        })
        .collect()
}

/// Strip the Windows \\?\ prefix so canonical and configured paths match. Comparison only.
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

/// Fold a record key for folded prefix matching: strip `\\?\` and lowercase
/// unconditionally. [`fold_path`] is the case-sensitivity-aware variant.
pub fn fold_key(path: &Utf8Path) -> String {
    strip_verbatim(path.as_str()).to_lowercase()
}

/// Prefix match for a key already folded by [`fold_key`] against an
/// already-folded prefix - lets callers fold once and match against many
/// prefixes.
pub fn folded_starts_with(folded_key: &str, folded_prefix: &str) -> bool {
    let mut key_chars = folded_key.chars();
    for expected in folded_prefix.chars() {
        if key_chars.next() != Some(expected) {
            return false;
        }
    }
    // prefix must end on a path component boundary
    key_chars.next().is_none_or(|c| c == '/' || c == '\\')
}

/// Build a comparison key - strip \\?\ and fold case on case-insensitive volumes.
pub fn fold_path(path: &Utf8Path) -> Utf8PathBuf {
    let normalized = strip_verbatim(path.as_str());
    if is_case_insensitive(path) {
        Utf8PathBuf::from(normalized.to_lowercase())
    } else {
        Utf8PathBuf::from(normalized.into_owned())
    }
}

/// Path equality per [`fold_path`]: identical spellings match outright,
/// otherwise the folded forms are compared.
pub fn paths_equal(a: &Utf8Path, b: &Utf8Path) -> bool {
    a == b || fold_path(a) == fold_path(b)
}

/// Whether two paths are the same file (same device and inode). False if either can't be stat'ed.
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
        // str::to_lowercase maps word-final Σ to ς, char::to_lowercase does not
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
    //! Timing probe for audit finding F053: [`folded_starts_with`] walks two
    //! per-char iterators, and `discover/targeted.rs::index_records_under`
    //! calls it once per (record, target) pair after one `fold_key` per
    //! record - a targeted rescan over a six-figure record map pays ~1e8
    //! char-by-char comparisons plus one `to_lowercase` allocation per
    //! record. This bench times the current implementation against a
    //! byte-level equivalent in exactly that loop shape, and sizes the
    //! per-record `fold_key` pass next to it.
    //!
    //! Why the byte variant is the same predicate (audit argument, verified
    //! at runtime below over every pair the timed scans run): both inputs are
    //! already folded by `fold_key` / `fold_path`, i.e. valid UTF-8 with the
    //! match starting at a char boundary. UTF-8 encodes every char uniquely,
    //! so equal byte prefixes imply equal char prefixes (and vice versa);
    //! `/` and `\` are pure ASCII while UTF-8 multibyte sequences only
    //! contain bytes >= 0x80, so testing the raw byte after the prefix is
    //! exactly the "next char is a path separator" component check.
    //!
    //! Excluded from the normal test run - timings are environment-dependent
    //! and only meaningful as a human-read report:
    //!
    //! ```text
    //! cargo test --release folded_prefix -- --ignored --nocapture
    //! ```

    use std::{hint::black_box, time::Instant};

    use camino::Utf8Path;

    use super::{fold_key, folded_starts_with};

    /// simulated `scan_record` entries
    const KEYS: usize = 100_000;
    /// folded target prefixes matched against every record
    const TARGETS: usize = 1_000;
    /// ~10% of the targets match exactly one generated key
    const MATCHING_TARGETS: usize = 100;
    /// every Nth record key carries CJK path segments (multibyte UTF-8)
    const MULTIBYTE_STRIDE: usize = 20;
    const MEASURED_RUNS: usize = 3;

    /// Byte-level equivalent of [`folded_starts_with`] for inputs that are
    /// already folded (both sides are `to_lowercase` products, as every
    /// caller produces them). The prefix test is a memcmp via
    /// `[u8]::starts_with`; the component-boundary check tests the raw byte
    /// after the prefix, which is exact because a byte equal to `/` or `\`
    /// can never occur inside a UTF-8 multibyte sequence.
    fn folded_starts_with_byte(folded_key: &str, folded_prefix: &str) -> bool {
        let key = folded_key.as_bytes();
        if !key.starts_with(folded_prefix.as_bytes()) {
            return false;
        }
        key.get(folded_prefix.len())
            .is_none_or(|&b| b == b'/' || b == b'\\')
    }

    /// Windows-style recorded paths carrying the verbatim prefix the scanner
    /// stores; every `MULTIBYTE_STRIDE`-th key uses CJK segments so multibyte
    /// UTF-8 stays on both hit and miss paths (5% share).
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

    /// TARGETS folded prefixes: MATCHING_TARGETS matching exactly one
    /// generated key (half ASCII, half CJK spellings), the rest misses in
    /// three divergence shapes - a nonexistent artist id (diverges at the
    /// first digit), its CJK twin, and an ASCII spelling for an id that only
    /// exists as a CJK key (diverges right after the root). Shuffled
    /// deterministically so first-hit positions vary like a set walk.
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

    /// The exact loop shape of `index_records_under` (deterministic target
    /// order here instead of `FxHashSet` walk order): per record, per target,
    /// early-exit on the first hit.
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

    /// Same loop with the byte-level predicate.
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

    /// Warm up once, run MEASURED_RUNS timed passes, print min/mean, and
    /// return (min ms, mean ms, last result). Headline numbers use min - the
    /// least noise-contaminated sample.
    fn time_runs<T>(name: &str, mut f: impl FnMut() -> T) -> (f64, f64, T) {
        f(); // warmup, untimed and unread
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

    /// Multibyte and boundary edge cases the dataset cannot reach: empty
    /// inputs, a UTF-8 lead byte right after the prefix, precomposed vs
    /// decomposed accents, and the final-sigma fold both sides get from
    /// `to_lowercase`.
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
        // the raw byte after the prefix must not read as a separator when it
        // is a UTF-8 lead byte (the audit's hinge case)
        assert!(!folded_starts_with("abc中/x", "abc"));
        assert!(!folded_starts_with_byte("abc中/x", "abc"));
        assert!(folded_starts_with("abc中/x", "abc中"));
        assert!(folded_starts_with_byte("abc中/x", "abc中"));
        // final sigma: fold both sides exactly like fold_key does
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

        // Equivalence first: targeted edge cases, then every (key, target)
        // pair the timed scans below will run.
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

        // per-record fold pass exactly as index_records_under pays it: one
        // fold_key (strip_verbatim + to_lowercase allocation) per record
        let (fold_min, _fold_mean, _) =
            time_runs("fold_key pass (1 strip+lowercase alloc/record)", || {
                raw_keys
                    .iter()
                    .map(|raw| fold_key(Utf8Path::new(raw)))
                    .collect::<Vec<String>>()
            });
        // allocation floor: to_lowercase alone, separating the strip/Utf8Path
        // dispatch from the per-record String allocation
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
