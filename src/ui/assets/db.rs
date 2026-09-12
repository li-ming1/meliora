use std::{
    borrow::Cow,
    sync::{Mutex, OnceLock},
};

use anyhow::anyhow;
use image::{ExtendedColorType, ImageEncoder, Rgba, RgbaImage, codecs::png::PngEncoder};
use rustc_hash::FxHashMap;
use sqlx::SqlitePool;
use url::Url;

/// Transparent 1×1 PNG. Returning this instead of `None` for albums without
/// artwork keeps gpui's asset loader on the success path, so a missing cover
/// is not logged as "asset not found" on every re-render (and does not
/// re-query the DB each time). Visually identical: the row's background block
/// shows through the transparent image.
fn placeholder_png() -> std::borrow::Cow<'static, [u8]> {
    use std::sync::OnceLock;

    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    std::borrow::Cow::Borrowed(BYTES.get_or_init(|| {
        let image = RgbaImage::from_pixel(1, 1, Rgba([0, 0, 0, 0]));
        let mut png = Vec::new();
        PngEncoder::new(&mut png)
            .write_image(image.as_raw(), 1, 1, ExtendedColorType::Rgba8)
            .expect("encoding a 1x1 png cannot fail");
        png
    }))
}

pub fn load(pool: &SqlitePool, url: Url) -> gpui::Result<Option<Cow<'static, [u8]>>> {
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("missing table name"))?;
    match host {
        "album" | "track" => {
            let mut segments = url.path_segments().ok_or_else(|| anyhow!("missing path"))?;
            let id: i64 = segments
                .next()
                .ok_or_else(|| anyhow!("missing id"))?
                .parse()?;
            let image_type = segments
                .next()
                .ok_or_else(|| anyhow!("missing image type"))?;

            let table: &'static str = if host == "album" { "album" } else { "track" };

            // 缩略图缓存命中：连数据库都不用查，也就不再有 gpui 后台
            // 线程上的 block_on 等待。
            if image_type == "thumb"
                && let Some(bytes) = thumb_cache_get(table, id)
            {
                return Ok(Some(Cow::Owned(bytes)));
            }

            let query = match (host, image_type) {
                ("album", "thumb") => include_str!("../../../queries/assets/find_album_thumb.sql"),
                ("album", "full") => include_str!("../../../queries/assets/find_album_art.sql"),
                ("track", "thumb") => {
                    include_str!("../../../queries/assets/find_track_thumb.sql")
                }
                ("track", "full") => include_str!("../../../queries/assets/find_track_art.sql"),
                // unknown image type = no asset
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
                        // Thumbnails are rendered at list/grid sizes (≤ ~200px)
                        // yet stored full-ish, so decoding them at full size
                        // leaves multi-hundred-KB RGBA buffers stuck in every
                        // bounded image cache slot (tables cache 200 items,
                        // the global cache 12). Shrink before returning so the
                        // caches stay small.
                        if let Some(shrunken) = shrink_thumb_cached(table, id, &image) {
                            return Ok(Some(Cow::Owned(shrunken)));
                        }
                    }
                    Ok(Some(Cow::Owned(image)))
                }
                // no artwork stored → transparent placeholder, not `None`
                _ => Ok(Some(placeholder_png())),
            }
        }
        _ => Ok(None),
    }
}

/// Upper bound for thumb assets after shrinking. The UI never paints thumb
/// tiles larger than this, so anything bigger is wasted working set.
const THUMB_MAX_PX: u32 = 128;

/// 缩略图资产缓存条目上限。256 条 ≤128px 的 PNG（thumb 列存的是扫描器
/// 的 70×70 BMP，缩后每条约 20–60KB）总量在几 MB 量级；写满后不再新增，
/// 绝不无界增长。
const SHRUNK_THUMB_CACHE_MAX: usize = 256;

/// 已缩到 128px 的 thumb 资产内存缓存，键为（表, id）。`!db://` 资产在
/// 滚动中会被 gpui 的资产加载器反复请求，无缓存时每次都要在后台线程上
/// block_on 查库 + decode + shrink + PNG 编码。缓存命中后连数据库查询
/// 都省掉。注意：条目不淘汰，同一 (表, id) 的 artwork 在重新扫描后被
/// 更新时，缩略图会到下次启动才刷新——thumb 来源（70×70 BMP）在扫描后
/// 不再变化，这个取舍可以接受。
static SHRUNK_THUMBS: OnceLock<Mutex<FxHashMap<(&'static str, i64), Vec<u8>>>> = OnceLock::new();

fn shrunk_thumbs() -> &'static Mutex<FxHashMap<(&'static str, i64), Vec<u8>>> {
    SHRUNK_THUMBS.get_or_init(|| Mutex::new(FxHashMap::default()))
}

/// 读缓存；临界区无 await（本函数是同步接口）。
fn thumb_cache_get(table: &'static str, id: i64) -> Option<Vec<u8>> {
    shrunk_thumbs()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&(table, id))
        .cloned()
}

/// 写缓存；写满即止，不淘汰既有条目。
fn thumb_cache_put(table: &'static str, id: i64, bytes: Vec<u8>) {
    let mut cache = shrunk_thumbs()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if cache.len() >= SHRUNK_THUMB_CACHE_MAX || cache.contains_key(&(table, id)) {
        return;
    }
    cache.insert((table, id), bytes);
}

/// Best-effort downscale of a thumb asset down to at most 128×128, memoized
/// in the bounded `SHRUNK_THUMBS` cache so a repeated request for the same
/// asset skips the decode + shrink + PNG-encode entirely. Returns `None` on
/// any (unexpected) failure so the caller keeps the raw bytes rather than
/// breaking art rendering.
fn shrink_thumb_cached(table: &'static str, id: i64, data: &[u8]) -> Option<Vec<u8>> {
    if let Some(cached) = thumb_cache_get(table, id) {
        return Some(cached);
    }
    let shrunken = shrink_thumb(data)?;
    thumb_cache_put(table, id, shrunken.clone());
    Some(shrunken)
}

/// Best-effort downscale of a thumb asset down to at most 128×128. Returns
/// `None` on any (unexpected) encode failure so the caller keeps the raw
/// bytes rather than breaking art rendering.
fn shrink_thumb(data: &[u8]) -> Option<Vec<u8>> {
    let image = image::load_from_memory(data).ok()?;
    let thumb = image.thumbnail(THUMB_MAX_PX, THUMB_MAX_PX);
    let mut out = Vec::new();
    thumb
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .ok()?;
    Some(out)
}
