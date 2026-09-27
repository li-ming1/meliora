pub mod bundled;
pub mod db;

use std::borrow::Cow;

use gpui::AssetSource;
use sqlx::SqlitePool;
use url::Url;

use crate::ui::assets::bundled::BundledAssets;

/// gpui 的 `AssetSource` 实现，按 URL scheme 分发：`!db://` 资产查库表，
/// `!bundled://` 资产取自编译期内嵌资源；其余 scheme 一律视为资产缺失。
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
