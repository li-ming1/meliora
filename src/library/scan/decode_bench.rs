//! 扫描热环的耗时探针：补上 Cargo.toml `[profile.release]` 注释里"decode/
//! checkpoint benches 是临时脚本、从未入仓，复测不要去找它们"的缺口。三个
//! 计时段各覆盖一条依赖热环，是 BUILD-1（依赖 crate 退出 z 档）A/B 的出数
//! 载体：
//!
//! 1. `png_decode+scale`：封面 PNG 解码 + 封面尺寸重缩放——png / fdeflate /
//!    simd-adler32 的热环（Cargo.toml 原话："simd-adler32 本轮不动：动它前
//!    需先 A/B"）。
//! 2. `checkpoint_zlib`：扫描检查点 zlib 压缩 + 解压——flate2 → miniz_oxide
//!    热环。压缩侧与 record.rs `compress_write_rename` 同 API 同参（仅把文
//!    件承口换成 Vec 免磁盘 IO，record.rs bench 同口径）；解压侧与
//!    `load_scan_record` 同一 async 后端（async_compression →
//!    compression-codecs → flate2/miniz_oxide，lock 图无 libz-sys），仅去掉
//!    磁盘 IO 与 64MiB take 安全阀。
//! 3. `xxh3_dedup`：封面去重键 `xxh3_64`——decode.rs `ScannedArt` 对原始封
//!    面字节的同一调用形态，xxhash-rust 载体。
//!
//! BUILD-1 A/B 方法：改 Cargo.toml 里目标包的 `opt-level`（xxhash-rust /
//! simd-adler32 目前落在 `[profile.release.package."*"]` 的 z 档，无逐名回
//! 3 条目），重跑本测试对比各段 mean。判读：mean 差异 >10%（record.rs bench
//! 的运行间相符界）才算真实差异，再对照 exe 体积增量定去留；对照必须同环
//! 境、同序列、同进程（sharpen_rgba_with 文档记录的跨进程不可比教训）。
//!
//! 已从常规测试运行中排除——耗时依赖环境，只作为给人读的报告才有意义：
//!
//! ```text
//! cargo test --release --features kugou decode_bench -- --ignored --nocapture
//! ```

use std::{
    hint::black_box,
    io::{Cursor, Write},
};

use async_compression::tokio::bufread::ZlibDecoder;
use flate2::{Compression, write::ZlibEncoder};
use tokio::io::{AsyncReadExt, BufReader};
use xxhash_rust::xxh3::xxh3_64;

use super::decode::MAX_ART_DIMENSION;
use crate::test_support::bench_summarize_n;

/// 合成载荷大小：4MiB 取 record.rs 实测检查点载荷档位的中位（50k 条
/// postcard 3.72MiB、100k 条 7.44MiB）。
const PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

/// 合成封面尺寸：长边 3000 > 1024 封顶，必然触发 decode.rs 的 Lanczos3 重
/// 缩放路径；原始 RGB8 18MB 是扫描会遇到的大封面量级。
const ART_W: u32 = 3000;
const ART_H: u32 = 2000;

/// PNG 段轮数：~18MB IDAT 全量解压 + 6MP→1024 Lanczos3 单轮估 50-150ms，
/// 2 轮预热 + 6 轮计时累计 300-1200ms，稳过 200ms 稳定性下限，墙钟 ~1s。
const PNG_WARMUP: usize = 2;
const PNG_RUNS: usize = 6;

/// zlib 段轮数：4MiB 噪声（近似不可压 = miniz 的最坏吞吐形态）单轮
/// compress+decompress 估 60-150ms，2 轮预热 + 8 轮计时累计 ~0.5-1.2s。
const ZLIB_WARMUP: usize = 2;
const ZLIB_RUNS: usize = 8;

/// xxh3 段轮数：xxh3_64 大缓冲吞吐通常 >5GB/s，4MiB 单轮 <1ms，取 1000 轮
/// 计时保证累计 ≥200ms；runs ≥10 还能拿到百分位（量级同
/// `test_support::BENCH_ITERATIONS` 的微探针口径）。
const XXH3_WARMUP: usize = 20;
const XXH3_RUNS: usize = 1000;

/// 固定种子（任意常数，只求跨运行逐字节一致；禁止运行时随机）。
const SEED_PAYLOAD: u64 = 0x9E37_79B9_7F4A_7C15;
const SEED_ART: u64 = 0xBF58_476D_1CE4_E5B9;

/// 固定种子 LCG（Knuth MMIX 常数）生成的确定性字节序列。
fn synthetic_bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

#[test]
#[ignore = "timing report, run with --ignored --nocapture"]
fn report_scan_decode_hot_loops() {
    // ---- setup（不计时）----
    let payload = synthetic_bytes(PAYLOAD_BYTES, SEED_PAYLOAD);
    let png_bytes = {
        let rgb = image::RgbImage::from_raw(
            ART_W,
            ART_H,
            synthetic_bytes(ART_W as usize * ART_H as usize * 3, SEED_ART),
        )
        .unwrap();
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(rgb)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        png
    };
    println!(
        "synthetic art: {ART_W}x{ART_H} RGB8 noise -> PNG {:.1} MiB；payload {:.1} MiB",
        png_bytes.len() as f64 / (1024.0 * 1024.0),
        PAYLOAD_BYTES as f64 / (1024.0 * 1024.0),
    );

    // 解压侧是 tokio 后端（load_scan_record 同款）；current-thread 运行时与
    // record.rs 的 bench_checkpoint_serialization 同款。
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // zlib 段的一次性往返校验（不计时）：确认参数下无损，并给出压缩比供判
    // 读这 4MiB 噪声的 zlib 输出形态。
    {
        let mut encoder = ZlibEncoder::new(
            Vec::with_capacity(payload.len() / 4),
            Compression::default(),
        );
        encoder.write_all(&payload).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut out = Vec::with_capacity(payload.len());
        rt.block_on(async {
            ZlibDecoder::new(BufReader::new(&compressed[..]))
                .read_to_end(&mut out)
                .await
        })
        .unwrap();
        assert_eq!(out, payload, "zlib roundtrip must be lossless");
        println!(
            "checkpoint_zlib payload: {:.2} MiB -> {:.2} MiB zlib（近似不可压噪声，最坏吞吐形态；真实形态的 postcard+zlib 读数见 record.rs bench）",
            payload.len() as f64 / (1024.0 * 1024.0),
            compressed.len() as f64 / (1024.0 * 1024.0),
        );
    }

    // ---- 段 1：PNG 解码 + 封面尺寸重缩放 ----
    // 镜像 decode.rs process_source_image 的解码与缩放两步（with_guessed_format
    // → decode → into_rgb8 → imageops::resize Lanczos3，长边封顶
    // MAX_ART_DIMENSION）。70px 缩略图 BMP 与 JPEG 重编码刻意不在段内——它
    // 们是独立成本；本段的裁决对象只有 png/fdeflate/simd-adler32。black_box
    // 整体吞掉输出，防优化器把"结果未使用"的整条解码链折叠掉。
    bench_summarize_n(
        "png_decode+scale 3000x2000->1024 cap",
        PNG_WARMUP,
        PNG_RUNS,
        || {
            let decoded = image::ImageReader::new(Cursor::new(&png_bytes))
                .with_guessed_format()
                .unwrap()
                .decode()
                .unwrap()
                .into_rgb8();
            // decode.rs process_source_image 的同一缩放式与同一滤波器
            let (w, h) = decoded.dimensions();
            let scale = MAX_ART_DIMENSION as f32 / (w.max(h) as f32);
            let new_w = (w as f32 * scale).round().max(1.0) as u32;
            let new_h = (h as f32 * scale).round().max(1.0) as u32;
            let resized = image::imageops::resize(
                &decoded,
                new_w,
                new_h,
                image::imageops::FilterType::Lanczos3,
            );
            black_box(resized);
        },
    );

    // ---- 段 2：检查点 zlib 压缩 + 解压 ----
    // 压缩侧：record.rs compress_write_rename 的同 API 同参（flate2
    // write::ZlibEncoder + Compression::default()；容量 1/4 与 record.rs
    // bench 同款，生产承口是文件句柄，此处 Vec 承接免磁盘 IO）。解压侧：
    // load_scan_record 的同一后端，输入改内存字节；64MiB take 上限是防不可
    // 信输入的安全阀，不在解压热环内，故不带。
    bench_summarize_n(
        "checkpoint_zlib 4MiB comp+decomp",
        ZLIB_WARMUP,
        ZLIB_RUNS,
        || {
            let mut encoder = ZlibEncoder::new(
                Vec::with_capacity(payload.len() / 4),
                Compression::default(),
            );
            encoder.write_all(&payload).unwrap();
            let compressed = encoder.finish().unwrap();
            let mut out = Vec::with_capacity(payload.len());
            let n = rt
                .block_on(async {
                    ZlibDecoder::new(BufReader::new(&compressed[..]))
                        .read_to_end(&mut out)
                        .await
                })
                .unwrap();
            black_box((compressed.len(), n));
        },
    );

    // ---- 段 3：xxh3 去重键 ----
    // decode.rs ScannedArt::embedded/folder 对原始封面字节取 xxh3_64 的同一
    // 调用；本段对同一 4MiB 载荷计时。xxh3_64 吞吐与内容基本无关、随时长线
    // 性，per-MB 读数可直接外推到任意封面字节量。
    let witness = xxh3_64(&payload);
    println!("payload xxh3_64 = {witness:#018x}（确定性见证值，跨运行必须一致）");
    let mut sink = 0u64;
    bench_summarize_n("xxh3_dedup 4MiB payload", XXH3_WARMUP, XXH3_RUNS, || {
        sink = sink.wrapping_add(xxh3_64(&payload));
    });
    // 循环后黑盒消费累计值：没有这一步，"结果未使用"可以让整段哈希循环被
    // 优化器折叠。
    black_box(sink);
}
