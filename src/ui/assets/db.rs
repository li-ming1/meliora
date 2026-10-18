use crate::sync::RecoveredMutex;
use std::{
    borrow::Cow,
    collections::VecDeque,
    sync::{Mutex, OnceLock},
};

use anyhow::anyhow;
use image::{ExtendedColorType, ImageEncoder, Rgba, RgbaImage, codecs::png::PngEncoder};
use rustc_hash::FxHashMap;
use sqlx::SqlitePool;
use url::Url;

/// 全透明 1×1 PNG。专辑没有封面时返回它而不是 `None`，好让 gpui 的资产加载器留在
/// 成功路径上——这样缺封面不会每次重渲染都被记成 "asset not found"（也不必每次都
/// 重查 DB）。视觉上完全一致：透明图会透出那一行的背景块。
fn placeholder_png() -> Cow<'static, [u8]> {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    Cow::Borrowed(BYTES.get_or_init(|| {
        let image = RgbaImage::from_pixel(1, 1, Rgba([0, 0, 0, 0]));
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(image.as_raw(), 1, 1, ExtendedColorType::Rgba8)
            .expect("encoding a 1x1 png cannot fail");
        png
    }))
}

/// 载入 `!db://<table>/<id>/<thumb|full>` 形式的封面资产，其中 `table` 是 `album`
/// 或 `track`。行里没有封面时解析为透明占位图；其他 host 一律返回 `Ok(None)`。
pub fn load(pool: &SqlitePool, url: Url) -> gpui::Result<Option<Cow<'static, [u8]>>> {
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("missing table name"))?;
    match host {
        "album" => load_table_asset(pool, "album", url),
        "track" => load_table_asset(pool, "track", url),
        // 其他表不存封面
        _ => Ok(None),
    }
}

/// 载入两张存封面的表之一。`table` 同时作为缩略图缓存与下面这些查询的键。
fn load_table_asset(
    pool: &SqlitePool,
    table: &'static str,
    url: Url,
) -> gpui::Result<Option<Cow<'static, [u8]>>> {
    let mut segments = url.path_segments().ok_or_else(|| anyhow!("missing path"))?;
    let id: i64 = segments
        .next()
        .ok_or_else(|| anyhow!("missing id"))?
        .parse()?;
    let image_type = segments
        .next()
        .ok_or_else(|| anyhow!("missing image type"))?;

    // 缩略图缓存命中：连数据库都不用查，也就不再有 gpui 后台
    // 线程上的 block_on 等待。
    if image_type == "thumb"
        && let Some(bytes) = thumb_cache_get(table, id)
    {
        return Ok(Some(Cow::Owned(bytes)));
    }

    let query = match (table, image_type) {
        ("album", "thumb") => include_str!("../../../queries/assets/find_album_thumb.sql"),
        ("album", "full") => include_str!("../../../queries/assets/find_album_art.sql"),
        ("track", "thumb") => include_str!("../../../queries/assets/find_track_thumb.sql"),
        ("track", "full") => include_str!("../../../queries/assets/find_track_art.sql"),
        // 未知图像类型 = 没有资产
        _ => return Ok(Some(placeholder_png())),
    };

    // gpui 的资产加载器在本函数的调用点运行在后台线程上
    // （img.rs 的 ImageAssetLoader 被 spawn 到 background_executor），
    // 而 `AssetSource::load` 是同步接口：调用线程必须等结果，无法
    // 把整个加载改成 async（那需要改 assets.rs 的调用方签名）。
    // sqlx-sqlite 的实际查询工作在其连接线程上执行，这里的
    // block_on 只是等待；配合上面的缓存，热路径上的重复等待已被
    // 消除。
    let row: Option<(Option<Vec<u8>>,)> =
        crate::RUNTIME.block_on(sqlx::query_as(query).bind(id).fetch_optional(pool))?;

    match row {
        Some((Some(image),)) if !image.is_empty() => {
            if image_type == "thumb" {
                // 缩略图按列表/网格的尺寸渲染（约 ≤200px）却以接近全尺寸入库，于是按
                // 全尺寸解码会让几十 KB 的 RGBA 缓冲卡在每一个有界图像缓存槽位里
                // （表缓存 200 项，全局缓存 12）。返回前先缩小，让缓存保持小。
                if let Some(shrunken) = shrink_thumb_cached(table, id, &image) {
                    return Ok(Some(Cow::Owned(shrunken)));
                }
            }
            Ok(Some(Cow::Owned(image)))
        }
        // 没存封面 → 透明占位图，而不是 `None`
        _ => Ok(Some(placeholder_png())),
    }
}

/// 缩略图资产缩小之后的上界。UI 从不绘制比这更大的缩略图瓦片，再大的都只是浪费
/// 工作集。
///
/// 2026-10-17 实测校准（`library.db` 全 20 条 `artwork.thumb`）：当代扫描器写的是
/// `THUMBNAIL_SIZE = 70` 的 BMP（scan/decode.rs:269，实测 70×70 / 19,722 B），
/// 所以对当代产物这一步**不改尺寸**（`image::thumbnail` 不放大，70px 源下几何恒等），
/// 整条函数等于"BMP 解码 + PNG 重编码"。**实测它连"转 PNG 会更小"也不成立**
/// （`bench_thumb_miss_costs`，他库 20 张真实 thumb）：产物 PNG 25,332 B/张 > 输入 BMP
/// 19,722 B（**大 1.30×**），代价 382µs/次 miss。尺寸上限只对历史代或外部写入的较大
/// thumb 生效。原来那句"stored full-ish / multi-hundred-KB RGBA buffers"是按更旧的存储
/// 形态写的，留着会让后人以为这里有一条大的降幅可拿。
///
/// 另一件由同一次实测暴露、**尚未处理**的事：专辑网格单元最小 192 CSS px
/// （`uniform_grid.rs:10`，1.25 DPR 下 ≥240 设备像素）却用 `/thumb`
/// （`table.rs:421`）= 70px 源，等于 3.4× 放大。修法应照圆盘那条
/// `fetch_display_cover_bytes_capped`（显示×4 封顶）给网格档换成 `/full`
/// （库里是 256-1024px JPEG），属可见质量改动，未擅自改。
const THUMB_MAX_PX: u32 = 128;

/// 缩略图资产缓存条目上限。当代扫描器写的 thumb 是 70×70 BMP（实测 19,722 B/张），
/// 不大于 `THUMB_MAX_PX` 时原样缓存 ⇒ 256 条约 5 MB；只有历史代/外部写入的较大 thumb
/// 才会被缩小并重编码成 PNG。写满后按最近使用 LRU 逐出，绝不无界增长。
const SHRUNK_THUMB_CACHE_MAX: usize = 256;

/// 已缩到 128px 的 thumb 资产内存缓存，键为（表, id），LRU 逐出（命中即
/// 续命）。`!db://` 资产在滚动中会被 gpui 的资产加载器反复请求，无缓存时
/// 每次都要在后台线程上 block_on 查库 + decode（+ 仅对大于上限的源做缩放重编码）；
/// 缓存命中后连数据库查询都省掉。"写满即止"会让第 257 个起的资产永远走这条
/// 慢路径（永久性能悬崖），所以满时必须逐出——FIFO 会把大库网格往返
/// 滚动中仍在屏的热条目按插入顺序挤掉（每 miss 重走整条慢路径），LRU
/// 让最近访问的可见集驻留。thumb 来源（扫描器的 70×70 BMP）在扫描后
/// 不再变化，且扫描完成时整体清空（见 `clear_shrunk_thumb_cache`），不
/// 存在陈旧条目问题。
struct ShrunkThumbCache {
    entries: FxHashMap<(&'static str, i64), Vec<u8>>,
    /// 由旧到新的访问序，供 LRU 逐出；键集合与 `entries` 保持一致。
    order: VecDeque<(&'static str, i64)>,
}

static SHRUNK_THUMBS: OnceLock<Mutex<ShrunkThumbCache>> = OnceLock::new();

fn shrunk_thumbs() -> &'static Mutex<ShrunkThumbCache> {
    SHRUNK_THUMBS.get_or_init(|| {
        Mutex::new(ShrunkThumbCache {
            entries: FxHashMap::default(),
            order: VecDeque::new(),
        })
    })
}

type ThumbCacheGuard = std::sync::MutexGuard<'static, ShrunkThumbCache>;

/// 锁定缓存；毒化时直接恢复守卫数据——它只是纯内存缓存，无值得传播
/// panic 的不变量。
fn lock_thumb_cache() -> ThumbCacheGuard {
    shrunk_thumbs().lock_recovered()
}

/// 读缓存；命中时把键提升为最近使用（LRU 续命）。临界区无 await（本函数
/// 是同步接口，运行在 gpui 后台线程）；提升是 O(n) 队列操作，n ≤ 上限
/// 256，相比它省掉的 decode+PNG 编码可忽略。
fn thumb_cache_get(table: &'static str, id: i64) -> Option<Vec<u8>> {
    let mut cache = lock_thumb_cache();
    let bytes = cache.entries.get(&(table, id)).cloned()?;
    promote_thumb_key(&mut cache.order, (table, id));
    Some(bytes)
}

/// 把键移到访问序末尾（最近使用）；键不在队列时按新使用追加（与 entries
/// 的插入配套）。
fn promote_thumb_key(order: &mut VecDeque<(&'static str, i64)>, key: (&'static str, i64)) {
    if let Some(idx) = order.iter().position(|k| *k == key) {
        order.remove(idx);
    }
    order.push_back(key);
}

/// 写缓存；已存在的键提升为最近使用（字节不变——thumb 来源在扫描代内
/// 不变），超出上限时逐出最久未用条目（纯内存操作，无 IO）。
fn thumb_cache_put(table: &'static str, id: i64, bytes: Vec<u8>) {
    let mut cache = lock_thumb_cache();
    if cache.entries.contains_key(&(table, id)) {
        promote_thumb_key(&mut cache.order, (table, id));
        return;
    }
    cache.entries.insert((table, id), bytes);
    cache.order.push_back((table, id));
    while cache.entries.len() > SHRUNK_THUMB_CACHE_MAX
        && let Some(oldest) = cache.order.pop_front()
    {
        cache.entries.remove(&oldest);
    }
}

/// 清空缩略图缓存。库扫描是 thumb 资产唯一的写入者，扫描完成时整体丢弃
/// （总量至多几 MB）是最便宜且最正确的失效方式；否则先期占满缓存的旧
/// 资产会把后续新资产永久挡在 DB 慢路径上。
pub(crate) fn clear_shrunk_thumb_cache() {
    let mut cache = lock_thumb_cache();
    cache.entries.clear();
    cache.order.clear();
}

/// 尽力把缩略图资产降到最大 128×128，并在有界的 `SHRUNK_THUMBS` 缓存里记忆化，于是
/// 对同一资产的重复请求完全跳过解码 + 缩小 + PNG 编码。任何（意料之外的）失败都返回
/// `None`，让调用方保留原始字节而不是把封面渲染弄坏。
fn shrink_thumb_cached(table: &'static str, id: i64, data: &[u8]) -> Option<Vec<u8>> {
    if let Some(cached) = thumb_cache_get(table, id) {
        return Some(cached);
    }
    let shrunken = shrink_thumb(data)?;
    thumb_cache_put(table, id, shrunken.clone());
    Some(shrunken)
}

/// 尽力把缩略图资产降到最大 128×128。任何（意料之外的）编码失败都返回 `None`，
/// 让调用方保留原始字节而不是把封面渲染弄坏。
fn shrink_thumb(data: &[u8]) -> Option<Vec<u8>> {
    let image = image::load_from_memory(data).ok()?;
    // 源已经不大于显示上限时**原样返回**，不再重编码。
    //
    // 为什么这是净赢而不是省事（实测，他库 20 张真实 thumb，`bench_thumb_miss_costs`）：
    // `image::thumbnail` 只做缩小，70×70 当代产物在 128 上限下几何运算是恒等；
    // 而把它重编码成 PNG 后**字节反而变大**——BMP 输入 19,722 B/张，PNG 输出 25,332 B/张（1.30×），
    // 每次缓存 miss 还多付一次"BMP 解码 + PNG 编码"（382.4µs/张，对只解码的 8.25µs 是 46×）。
    // 原样返回的字节 gpui 能直接渲染：`img.rs` 用 `image::guess_format` 按内容判格式，
    // 非 GIF/WebP 都走通用解码分支，而本仓 vendored `image-trim` 的
    // `default = ["bmp", "gif", "jpeg", "png", "webp"]` 里带着 bmp；
    // `!db://<table>/<id>/thumb` 这类 URI 也没有扩展名可被误信。像素与旧路径完全一致（无损同图）。
    if image.width() <= THUMB_MAX_PX && image.height() <= THUMB_MAX_PX {
        return Some(data.to_vec());
    }
    let thumb = image.thumbnail(THUMB_MAX_PX, THUMB_MAX_PX);
    let mut out = Vec::new();
    thumb
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 读侧闸门的契约：不大于 `THUMB_MAX_PX` 的源**原样返回**（字节相等、一次转码都不付），
    /// 大于上限的源仍然缩小并重编码（输出可再解码、尺寸不超过上限）。
    /// 当代扫描器写的就是 70×70 BMP ⇒ 走的是第一条分支。
    #[test]
    fn shrink_thumb_passes_through_small_sources_and_shrinks_large_ones() {
        fn encode_bmp(w: u32, h: u32) -> Vec<u8> {
            let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                w,
                h,
                image::Rgb([10, 120, 200]),
            ));
            let mut buf = Vec::new();
            img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Bmp)
                .unwrap();
            buf
        }

        let small = encode_bmp(70, 70);
        let out = shrink_thumb(&small).expect("small thumb");
        assert_eq!(
            out, small,
            "不大于上限的源必须原样返回（重编码只会把字节变大、多付一次转码）"
        );

        let big = encode_bmp(400, 400);
        let out = shrink_thumb(&big).expect("large thumb");
        assert!(out.len() < big.len(), "较大源缩小后字节应小于原 BMP");
        let decoded = image::load_from_memory(&out).expect("re-decode shrunk thumb");
        assert!(
            decoded.width() <= THUMB_MAX_PX && decoded.height() <= THUMB_MAX_PX,
            "缩放结果不得超过显示上限"
        );
    }

    /// 判"扫描器直存 JPEG + 读侧闸门"这条待拍板项的每 miss 单价（GPUI_HARDCORE_PERFORMANCE.md
    /// §44 第二十九轮留的缺口：转码那步值多少毫秒）。输入是他库里 20 张**真实**
    /// `artwork.thumb` BMP 字节的逐字节副本（先导出到临时目录，避免测试碰运行时库文件）。
    ///
    /// 三条对照就是三个候选形态在"缓存 miss 一次"上的成本：
    /// - `bmp_to_png`：今日现状（decode BMP → thumbnail(128) 恒等 → PNG 编码）。
    /// - `bmp_decode_only`：若加"源已 ≤ THUMB_MAX_PX 即原样返回"闸门，剩下的成本。
    /// - `jpg_to_png`：若扫描器改存 JPEG 但**不加**闸门（decode JPEG → 恒等 → PNG 编码）。
    ///
    /// `ignored`：依赖外部导出的样本文件，不参与默认 `cargo test`。
    #[test]
    #[ignore]
    fn bench_thumb_miss_costs() {
        let dir = std::env::var("MELIORA_THUMB_BENCH_DIR")
            .unwrap_or_else(|_| r"C:\Users\86185\AppData\Local\Temp\meliora_thumbs".to_string());
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .map(|e| e.path())
                    .filter(|p| p.extension().map(|x| x == "bmp").unwrap_or(false))
                    .collect()
            })
            .unwrap_or_default();
        if files.is_empty() {
            eprintln!("no thumb samples under {dir}; skipping");
            return;
        }
        files.sort();

        const REPS: usize = 9;
        let (mut t_full, mut t_decode, mut t_jpg) = (0u128, 0u128, 0u128);
        let (mut png_bytes, mut jpg_bytes, mut bmp_bytes) = (0usize, 0usize, 0usize);
        let mut per_item = Vec::new();

        for path in &files {
            let raw = std::fs::read(path).unwrap();
            bmp_bytes += raw.len();

            let mut best_full = u128::MAX;
            let mut best_decode = u128::MAX;
            let mut best_jpg = u128::MAX;
            let mut jpeg_buf = Vec::new();
            let mut png_len = 0usize;
            for _ in 0..REPS {
                let t0 = std::time::Instant::now();
                let out = shrink_thumb(&raw).expect("shrink");
                best_full = best_full.min(t0.elapsed().as_micros());
                png_len = out.len();

                let t1 = std::time::Instant::now();
                let img = image::load_from_memory(&raw).expect("decode");
                best_decode = best_decode.min(t1.elapsed().as_micros());

                if jpeg_buf.is_empty() {
                    img.clone()
                        .write_to(
                            &mut std::io::Cursor::new(&mut jpeg_buf),
                            image::ImageFormat::Jpeg,
                        )
                        .expect("jpeg encode");
                    jpg_bytes += jpeg_buf.len();
                }
                let t2 = std::time::Instant::now();
                let _ = shrink_thumb(&jpeg_buf).expect("shrink jpg");
                best_jpg = best_jpg.min(t2.elapsed().as_micros());
            }
            // 输出字节是确定性值，只能在整条 item 上计一次（早先写在 REP 循环里 ⇒ 被乘了 REPS）。
            png_bytes += png_len;
            t_full += best_full;
            t_decode += best_decode;
            t_jpg += best_jpg;
            per_item.push((best_full, best_decode, best_jpg));
        }

        let n = files.len();
        println!("thumb samples: {n}  (min-of-{REPS} per item, summed)");
        println!("stored bytes: bmp={bmp_bytes}  jpeg_default={jpg_bytes}  png_out={png_bytes}");
        println!(
            "MISS COST (total over {n} items / avg per item):  bmp_to_png={t_full}us/{}  decode_only={t_decode}us/{}  jpg_to_png={t_jpg}us/{}",
            t_full as f64 / n as f64,
            t_decode as f64 / n as f64,
            t_jpg as f64 / n as f64
        );
        let slowest = per_item.iter().map(|p| p.0).max().unwrap_or_default();
        println!("slowest single bmp_to_png miss: {slowest}us");
    }
}
