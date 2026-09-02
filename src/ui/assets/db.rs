use std::borrow::Cow;

use anyhow::anyhow;
use image::{ExtendedColorType, ImageEncoder, Rgba, RgbaImage, codecs::png::PngEncoder};
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

            let row: Option<(Option<Vec<u8>>,)> =
                crate::RUNTIME.block_on(sqlx::query_as(query).bind(id).fetch_optional(pool))?;

            match row {
                Some((Some(image),)) if !image.is_empty() => Ok(Some(Cow::Owned(image))),
                // no artwork stored → transparent placeholder, not `None`
                _ => Ok(Some(placeholder_png())),
            }
        }
        _ => Ok(None),
    }
}
