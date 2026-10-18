use super::*;
use crate::test_support::{TestDir, register_test_media_providers};
use std::fs;

#[test]
fn resolve_lyrics_prefers_sidecar() {
    let dir = TestDir::new("decode-lyrics-test");
    let track = dir.utf8_join("track.flac");
    fs::write(&track, b"").unwrap();
    fs::write(dir.join("track.lrc"), "[00:00.00] sidecar lyrics").unwrap();

    let result = resolve_lyrics(
        &track,
        SidecarLyrics::Present,
        Some("[00:00.00] embedded lyrics".to_string()),
    );
    assert_eq!(result.as_deref(), Some("[00:00.00] sidecar lyrics"));
}

#[test]
fn resolve_lyrics_falls_back_to_embedded() {
    let dir = TestDir::new("decode-lyrics-test");
    let track = dir.utf8_join("track.flac");
    fs::write(&track, b"").unwrap();

    let result = resolve_lyrics(
        &track,
        SidecarLyrics::Unknown,
        Some("[00:00.00] embedded lyrics".to_string()),
    );
    assert_eq!(result.as_deref(), Some("[00:00.00] embedded lyrics"));
}

#[test]
fn resolve_lyrics_ignores_empty_sidecar() {
    let dir = TestDir::new("decode-lyrics-test");
    let track = dir.utf8_join("track.flac");
    fs::write(&track, b"").unwrap();
    fs::write(dir.join("track.lrc"), "   \n").unwrap();

    let result = resolve_lyrics(
        &track,
        SidecarLyrics::Unknown,
        Some("[00:00.00] embedded lyrics".to_string()),
    );
    assert_eq!(result.as_deref(), Some("[00:00.00] embedded lyrics"));
}

#[test]
fn resolve_lyrics_absent_hint_skips_the_sidecar_probe() {
    let dir = TestDir::new("decode-lyrics-absent-test");
    let track = dir.utf8_join("track.flac");
    fs::write(&track, b"").unwrap();
    // sidecar 文件确实在盘上：内嵌封面胜出就证明 Absent 确实跳过了读取
    fs::write(dir.join("track.lrc"), "[00:00.00] sidecar lyrics").unwrap();

    let result = resolve_lyrics(
        &track,
        SidecarLyrics::Absent,
        Some("[00:00.00] embedded lyrics".to_string()),
    );
    assert_eq!(result.as_deref(), Some("[00:00.00] embedded lyrics"));
}

#[test]
fn metadata_duration_skips_the_decoder_fallback() {
    let mut fallback_called = false;
    let duration = duration_ms_or_else(Some(42_000), || {
        fallback_called = true;
        Ok(1)
    })
    .unwrap();

    assert_eq!(duration, 42_000);
    assert!(!fallback_called);
}

#[test]
fn missing_or_zero_metadata_duration_uses_the_decoder_fallback() {
    for hint in [None, Some(0)] {
        let mut fallback_called = false;
        let duration = duration_ms_or_else(hint, || {
            fallback_called = true;
            Ok(42_000)
        })
        .unwrap();

        assert_eq!(duration, 42_000);
        assert!(fallback_called);
    }
}

#[test]
fn process_owned_album_art_creates_thumbnail() {
    let image = crate::test_support::audio_fixtures::fixture("cover.jpg");
    let (full, thumb) = process_owned_album_art(RawArt::Owned(image.into_boxed_slice())).unwrap();
    assert!(!full.as_ref().is_empty());
    assert!(thumb.starts_with(b"BM"));
}

#[test]
fn owned_small_art_reuses_its_original_buffer() {
    let image = image::RgbImage::from_pixel(16, 16, image::Rgb([1, 2, 3]));
    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image)
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
    let boxed = encoded.into_inner().into_boxed_slice();
    // 装进盒子之后再取指针：`into_boxed_slice` 会收缩多余容量并可能搬动这块
    // 分配，而下面那条复用分支必须原样交还这个缓冲区
    let original_ptr = boxed.as_ptr();

    let (processed, _) = process_owned_album_art(RawArt::Owned(boxed)).unwrap();
    let ProcessedImage::Owned(processed) = processed else {
        panic!("owned artwork should remain owned");
    };

    assert_eq!(processed.as_ptr(), original_ptr);
}

#[test]
fn shared_small_art_reuses_its_original_buffer() {
    let image = image::RgbImage::from_pixel(16, 16, image::Rgb([1, 2, 3]));
    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image)
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
    let encoded = Arc::new(encoded.into_inner());
    let original = Arc::clone(&encoded);

    let (processed, _) = process_owned_album_art(RawArt::Shared(encoded)).unwrap();
    let ProcessedImage::Shared(processed) = processed else {
        panic!("shared artwork should remain shared");
    };

    assert!(Arc::ptr_eq(&processed, &original));
}

#[test]
fn read_metadata_for_path_prefers_sidecar_lyrics() {
    register_test_media_providers();
    let dir = TestDir::new("decode-meta-test");
    let track = dir.utf8_join("track.flac");
    fs::write(
        &track,
        crate::test_support::audio_fixtures::fixture("fixture.flac"),
    )
    .unwrap();
    fs::write(dir.join("track.lrc"), "[00:00.00] override lyrics").unwrap();

    let info = read_metadata_for_path(&track, SidecarLyrics::Unknown).unwrap();
    assert_eq!(info.0.lyrics.as_deref(), Some("[00:00.00] override lyrics"));
}

#[test]
fn classify_io_kind_only_not_found_is_missing() {
    assert_eq!(
        classify_io_kind(std::io::ErrorKind::NotFound),
        ScanReadError::Missing
    );
    for kind in [
        std::io::ErrorKind::PermissionDenied,
        std::io::ErrorKind::Interrupted,
        std::io::ErrorKind::WouldBlock,
        std::io::ErrorKind::TimedOut,
        std::io::ErrorKind::UnexpectedEof,
    ] {
        assert_eq!(classify_io_kind(kind), ScanReadError::Transient);
    }
}

#[test]
fn read_metadata_for_nonexistent_path_is_missing() {
    register_test_media_providers();
    let dir = TestDir::new("decode-missing-test");
    let track = dir.utf8_join("nonexistent.flac");

    let err = read_metadata_for_path(&track, SidecarLyrics::Unknown).unwrap_err();
    assert_eq!(err, ScanReadError::Missing);
}

#[test]
fn read_metadata_for_garbage_file_is_corrupt() {
    register_test_media_providers();
    let dir = TestDir::new("decode-corrupt-test");
    let track = dir.utf8_join("garbage.flac");
    fs::write(&track, b"this is definitely not a flac stream").unwrap();

    let err = read_metadata_for_path(&track, SidecarLyrics::Unknown).unwrap_err();
    assert_eq!(err, ScanReadError::Corrupt);
}

#[test]
fn read_metadata_for_truncated_file_is_corrupt() {
    register_test_media_providers();
    let dir = TestDir::new("decode-truncated-test");
    let track = dir.utf8_join("truncated.flac");
    // 被截断的码流是损坏，不是临时状态
    let bytes = crate::test_support::audio_fixtures::fixture("fixture.flac");
    fs::write(&track, &bytes[..bytes.len() / 4]).unwrap();

    let err = read_metadata_for_path(&track, SidecarLyrics::Unknown).unwrap_err();
    assert_eq!(err, ScanReadError::Corrupt);
}

#[cfg(unix)]
#[test]
fn read_metadata_for_unreadable_file_is_transient() {
    use std::os::unix::fs::PermissionsExt;
    register_test_media_providers();
    let dir = TestDir::new("decode-transient-test");
    let track = dir.utf8_join("locked.flac");
    fs::write(
        &track,
        crate::test_support::audio_fixtures::fixture("fixture.flac"),
    )
    .unwrap();
    fs::set_permissions(&track, fs::Permissions::from_mode(0o000)).unwrap();

    // 如果我们仍然打得开它（例如以 root 身份运行）就跳过
    if std::fs::File::open(&track).is_ok() {
        return;
    }

    let err = read_metadata_for_path(&track, SidecarLyrics::Unknown).unwrap_err();
    assert_eq!(err, ScanReadError::Transient);
}

/// 封面缩放基准，手工运行方式：
/// `cargo test --release --features kugou -- bench_album_art --ignored --nocapture`
///
/// `decode.rs::process_source_image` 处「选哪个 `FilterType`？」这一决策的证据：
/// 生产用 `Lanczos3` 缩放 >1024px 的主图，70x70 缩略图则用 `imageops::thumbnail`
/// （整数面积平均，不是 `FilterType`）。只有 CatmullRom/Triangle 比 Lanczos3 快
/// >=30% 才换滤镜（GPUI HARDCORE 教条 §1/§39：没有测量就没有优化）。
///
/// 2026-09-12 实测，release 构建，2 轮预热后取 20 轮交错运行的中位数，输入为确定性
/// xorshift 噪声（每次运行完全相同）。两次运行相符在 ~10% 以内；引用第二次：
///
/// ```text
/// source -> target         Lanczos3    CatmullRom   Triangle    thumbnail() (production thumb path)
/// 1024x1024 -> 70x70       14.109 ms    9.890 ms    4.500 ms    1.949 ms
/// 3000x3000 -> 70x70      140.647 ms   90.652 ms   35.217 ms   10.270 ms
/// 3000x3000 -> 1024x1024  152.339 ms  116.527 ms   87.504 ms   (production main-image site)
/// 1024x1024 -> 1024x1024   ~0.29 ms for every filter: image 0.25.10
///                          对同尺寸缩放短路成一次纯拷贝
///                          (imageops/sample.rs:985)——不涉及任何滤波器；
///                          而且生产路径本来就把 <=1024 的图原样留着。
/// ```
///
/// 结论：保留 Lanczos3。在唯一的生产位点上 CatmullRom 没达到 >=30% 这条线
/// （3000->1024：-23.5%）。Triangle 达标（-42.6%），但换用它仍被否决：
/// 1. 这次缩放每个 >1024px 的封面只在后台扫描工作线程里跑一次（绝不在 UI/音频路径），
///    与数据库工作相互重叠，结果还会缓存进数据库。没有 profiling 证据把封面缩放标成
///    扫描瓶颈，所以每个超大封面省下的 ~65 ms 并非已证明的端到端收益（§1/§39）。
/// 2. 质量依据，取自 image 0.25.10 源码：`resize` 会按缩小倍率放大核窗口并对权重归一化
///    （sample.rs:259-261 与 295-297），所以在 2.93x 缩小倍率下 Triangle 用帽形核对约
///    +/-2.9 px 的窗口取平均，而 Lanczos3 用 3 叶 sinc 对约 +/-8.8 px 取平均。Lanczos3
///    因此有最好的阻带抑制——细节更锐，细密文字与密集图案上的摩尔纹/走样泄漏最少，
///    而那正是专辑封面携带的内容。主图是全应用看得最多的封面；为一次纯后台微基准的
///    收益去换它，通不过 §39 的检验。
/// 3. 70x70 缩略图那条路径根本不用 `FilterType` 缩放：生产用的是
///    `imageops::thumbnail`（真正的方框面积平均），实测在这些尺寸下比最快的滤镜还快
///    7-14 倍——那里没有可换项。
mod bench {
    use super::*;
    use std::hint::black_box;
    use std::time::Instant;

    const WARMUP_ROUNDS: usize = 2;
    const ROUNDS: usize = 20;

    /// 确定性 xorshift64* 噪声，好让每次测量都跑在同一份输入上。按审计规格生成为
    /// RGBA，再完全照生产解码路径的方式转成 rgb8（`decode().into_rgb8()`）。
    fn noise_image(size: u32) -> image::RgbImage {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut img = image::RgbaImage::new(size, size);
        for px in img.pixels_mut() {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let v = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
            *px = image::Rgba([v as u8, (v >> 8) as u8, (v >> 16) as u8, (v >> 24) as u8]);
        }
        DynamicImage::ImageRgba8(img).into_rgb8()
    }

    fn median_ms(mut samples: Vec<f64>) -> f64 {
        samples.sort_by(|a, b| a.total_cmp(b));
        samples[samples.len() / 2]
    }

    #[test]
    #[ignore = "benchmark: run with --ignored"]
    fn bench_album_art_filter_scaling() {
        let filters: [(&str, image::imageops::FilterType); 3] = [
            ("Lanczos3   ", image::imageops::FilterType::Lanczos3),
            ("CatmullRom ", image::imageops::FilterType::CatmullRom),
            ("Triangle   ", image::imageops::FilterType::Triangle),
        ];
        let sources: [(&str, image::RgbImage); 2] = [
            ("1024x1024", noise_image(1024)),
            ("3000x3000", noise_image(3000)),
        ];
        let targets: [(&str, u32, u32); 2] = [("70x70    ", 70, 70), ("1024x1024", 1024, 1024)];

        for (src_label, src) in &sources {
            for (tgt_label, tw, th) in targets {
                // 最后一个采样槽位是生产 thumbnail() 的参照（仅 70x70）
                let with_thumb_ref = th == 70;
                let mut samples: Vec<Vec<f64>> =
                    vec![Vec::new(); filters.len() + usize::from(with_thumb_ref)];

                // 轮次交错，好让每个变体看到的机器状态都相同
                for round in 0..WARMUP_ROUNDS + ROUNDS {
                    for (fi, (_, filter)) in filters.iter().enumerate() {
                        let start = Instant::now();
                        let out = imageops::resize(src, tw, th, *filter);
                        black_box(out.as_raw().as_slice());
                        if round >= WARMUP_ROUNDS {
                            samples[fi].push(start.elapsed().as_secs_f64() * 1e3);
                        }
                    }
                    if with_thumb_ref {
                        let start = Instant::now();
                        let out = imageops::thumbnail(src, tw, th);
                        black_box(out.as_raw().as_slice());
                        if round >= WARMUP_ROUNDS {
                            samples[filters.len()].push(start.elapsed().as_secs_f64() * 1e3);
                        }
                    }
                }

                let lanczos = median_ms(std::mem::take(&mut samples[0]));
                println!("{src_label} -> {tgt_label}:");
                println!(
                    "  Lanczos3    median {lanczos:8.3} ms  (production filter for the >1024 main image)"
                );
                for (fi, (name, _)) in filters.iter().enumerate().skip(1) {
                    let m = median_ms(std::mem::take(&mut samples[fi]));
                    println!(
                        "  {name} median {m:8.3} ms  ({:+6.1}% vs Lanczos3)",
                        (m - lanczos) / lanczos * 100.0
                    );
                }
                if with_thumb_ref {
                    let m = median_ms(std::mem::take(&mut samples[filters.len()]));
                    println!("  thumbnail() median {m:8.3} ms  (production thumbnail path)");
                }
                // image 0.25.10 会把同尺寸缩放短路成一次朴素拷贝
                // （imageops/sample.rs:985），所以那一行量的是 memcpy，
                // 不是滤镜。
                if src.width() == tw && src.height() == th {
                    println!(
                        "  note: same-size resize is short-circuited to a copy in image 0.25 - no filter work involved"
                    );
                }
            }
        }
    }
}
