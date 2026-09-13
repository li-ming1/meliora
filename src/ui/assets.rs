pub mod bundled;
pub mod db;

use std::borrow::Cow;

use gpui::AssetSource;
use sqlx::SqlitePool;
use url::Url;

use crate::ui::assets::bundled::BundledAssets;

pub struct MelioraAssetSource {
    pool: SqlitePool,
}

impl MelioraAssetSource {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

impl AssetSource for MelioraAssetSource {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        // Malformed paths and unknown schemes resolve to a missing asset
        // instead of panicking the asset task.
        let Some(rest) = path.get(1..) else {
            return Ok(None);
        };
        let url = Url::parse(rest)?;

        match url.scheme() {
            "db" => db::load(&self.pool, url),
            "bundled" => BundledAssets::load(url),
            _ => Ok(None),
        }
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<gpui::SharedString>> {
        BundledAssets.list(path)
    }
}
