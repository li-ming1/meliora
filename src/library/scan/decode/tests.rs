use super::*;
use crate::test_support::{TestDir, register_test_media_providers};
use std::fs;

#[test]
fn resolve_lyrics_prefers_sidecar() {
    let dir = TestDir::new("decode-lyrics-test");
    let track = dir.utf8_join("track.flac");
    fs::write(&track, b"").unwrap();
    fs::write(dir.join("track.lrc"), "[00:00.00] sidecar lyrics").unwrap();

    let result = resolve_lyrics(&track, Some("[00:00.00] embedded lyrics".to_string()));
    assert_eq!(result.as_deref(), Some("[00:00.00] sidecar lyrics"));
}

#[test]
fn resolve_lyrics_falls_back_to_embedded() {
    let dir = TestDir::new("decode-lyrics-test");
    let track = dir.utf8_join("track.flac");
    fs::write(&track, b"").unwrap();

    let result = resolve_lyrics(&track, Some("[00:00.00] embedded lyrics".to_string()));
    assert_eq!(result.as_deref(), Some("[00:00.00] embedded lyrics"));
}

#[test]
fn resolve_lyrics_ignores_empty_sidecar() {
    let dir = TestDir::new("decode-lyrics-test");
    let track = dir.utf8_join("track.flac");
    fs::write(&track, b"").unwrap();
    fs::write(dir.join("track.lrc"), "   \n").unwrap();

    let result = resolve_lyrics(&track, Some("[00:00.00] embedded lyrics".to_string()));
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
fn process_album_art_creates_thumbnail() {
    let image = crate::test_support::audio_fixtures::fixture("cover.jpg");
    let (full, thumb) = process_album_art(&image).unwrap();
    assert!(!full.is_empty());
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
    // capture the pointer after boxing: `into_boxed_slice` shrinks spare
    // capacity and may move the allocation, while the reuse branch below must
    // hand back exactly this buffer
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
    fs::write(&track, crate::test_support::audio_fixtures::fixture("fixture.flac")).unwrap();
    fs::write(dir.join("track.lrc"), "[00:00.00] override lyrics").unwrap();

    let info = read_metadata_for_path(&track).unwrap();
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

    let err = read_metadata_for_path(&track).unwrap_err();
    assert_eq!(err, ScanReadError::Missing);
}

#[test]
fn read_metadata_for_garbage_file_is_corrupt() {
    register_test_media_providers();
    let dir = TestDir::new("decode-corrupt-test");
    let track = dir.utf8_join("garbage.flac");
    fs::write(&track, b"this is definitely not a flac stream").unwrap();

    let err = read_metadata_for_path(&track).unwrap_err();
    assert_eq!(err, ScanReadError::Corrupt);
}

#[test]
fn read_metadata_for_truncated_file_is_corrupt() {
    register_test_media_providers();
    let dir = TestDir::new("decode-truncated-test");
    let track = dir.utf8_join("truncated.flac");
    // a truncated stream is corrupt, not temporary
    let bytes = crate::test_support::audio_fixtures::fixture("fixture.flac");
    fs::write(&track, &bytes[..bytes.len() / 4]).unwrap();

    let err = read_metadata_for_path(&track).unwrap_err();
    assert_eq!(err, ScanReadError::Corrupt);
}

#[cfg(unix)]
#[test]
fn read_metadata_for_unreadable_file_is_transient() {
    use std::os::unix::fs::PermissionsExt;
    register_test_media_providers();
    let dir = TestDir::new("decode-transient-test");
    let track = dir.utf8_join("locked.flac");
    fs::write(&track, crate::test_support::audio_fixtures::fixture("fixture.flac")).unwrap();
    fs::set_permissions(&track, fs::Permissions::from_mode(0o000)).unwrap();

    // skip if we can still open it (e.g. running as root)
    if std::fs::File::open(&track).is_ok() {
        return;
    }

    let err = read_metadata_for_path(&track).unwrap_err();
    assert_eq!(err, ScanReadError::Transient);
}

/// Cover-art rescaling benchmarks, hand-run with:
/// `cargo test --release --features kugou -- bench_album_art --ignored --nocapture`
///
/// Evidence for the "which `FilterType`?" decision at `decode.rs::process_source_image`:
/// production resizes the >1024px main image with `Lanczos3` and builds the 70x70
/// thumbnail with `imageops::thumbnail` (integer area average, not a `FilterType`).
/// Switch filter only if CatmullRom/Triangle beats Lanczos3 by >=30% (GPUI
/// HARDCORE rule §1/§39: no measurement, no optimization).
///
/// Measured 2026-09-12, release build, median of 20 interleaved rounds after 2
/// warmup rounds, deterministic xorshift noise input (identical every run).
/// Two runs agreed within ~10%; second run quoted:
///
/// ```text
/// source -> target         Lanczos3    CatmullRom   Triangle    thumbnail() (production thumb path)
/// 1024x1024 -> 70x70       14.109 ms    9.890 ms    4.500 ms    1.949 ms
/// 3000x3000 -> 70x70      140.647 ms   90.652 ms   35.217 ms   10.270 ms
/// 3000x3000 -> 1024x1024  152.339 ms  116.527 ms   87.504 ms   (production main-image site)
/// 1024x1024 -> 1024x1024   ~0.29 ms for every filter: image 0.25.10
///                          short-circuits same-size resize to a plain copy
///                          (imageops/sample.rs:985) - no filter involved;
///                          production keeps <=1024 images as-is anyway.
/// ```
///
/// Decision: keep Lanczos3. CatmullRom misses the >=30% bar at the only
/// production site (3000->1024: -23.5%). Triangle clears it (-42.6%), but the
/// switch is still rejected:
/// 1. The resize runs once per >1024px cover in a background scan worker
///    (never the UI/audio path), overlapped with DB work, and the result is
///    cached in the DB. No profiling evidence marks cover resize as a scan
///    bottleneck, so the ~65 ms/oversized-cover saving is not a proven
///    end-to-end win (§1/§39).
/// 2. Quality basis, from the image 0.25.10 sources: `resize` scales the
///    kernel window by the downscale ratio and normalizes the weights
///    (sample.rs:259-261 and 295-297), so at the 2.93x downscale Triangle
///    averages a ~+/-2.9 px window with a tent kernel while Lanczos3 averages
///    ~+/-8.8 px with a 3-lobe sinc. Lanczos3 therefore has the best stopband
///    suppression - sharper detail and the least moire/aliasing leakage on
///    fine text and dense patterns, which is exactly the content album covers
///    carry. The main image is the most-seen artwork in the app; trading that
///    for a background-only microbenchmark win fails the §39 test.
/// 3. The 70x70 thumbnail path never uses `FilterType` resize: production
///    uses `imageops::thumbnail` (true box area average), measured 7-14x
///    faster than the fastest filter at those sizes - nothing to switch there.
mod bench {
    use super::*;
    use std::hint::black_box;
    use std::time::Instant;

    const WARMUP_ROUNDS: usize = 2;
    const ROUNDS: usize = 20;

    /// Deterministic xorshift64* noise so every run measures the same input.
    /// Generated as RGBA per the audit spec, then converted to rgb8 exactly
    /// like the production decode path (`decode().into_rgb8()`).
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
                // last sample slot is the production thumbnail() reference (70x70 only)
                let with_thumb_ref = th == 70;
                let mut samples: Vec<Vec<f64>> = vec![Vec::new(); filters.len() + usize::from(with_thumb_ref)];

                // interleave rounds so every variant sees the same machine state
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
                println!("  Lanczos3    median {lanczos:8.3} ms  (production filter for the >1024 main image)");
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
                // image 0.25.10 short-circuits same-size resize to a plain
                // copy (imageops/sample.rs:985), so that row measures a
                // memcpy, not the filters.
                if src.width() == tw && src.height() == th {
                    println!("  note: same-size resize is short-circuited to a copy in image 0.25 - no filter work involved");
                }
            }
        }
    }
}
