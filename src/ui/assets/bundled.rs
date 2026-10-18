use std::borrow::Cow;

use gpui::SharedString;
use rust_embed::RustEmbed;
use url::Url;

/// `assets/` 在编译期内嵌的那个子集（fonts、icons、images），
/// 经 `!bundled://` scheme 提供。
#[derive(RustEmbed)]
#[folder = "./assets"]
#[include = "fonts/*"]
#[include = "icons/*"]
#[include = "images/*"]
#[exclude = "*.DS_Store"]
#[exclude = "icons/LICENSE"]
#[exclude = "icons/LICENSE-meliora-icons"]
pub struct BundledAssets;

impl BundledAssets {
    /// `url` 路径处内嵌资产的字节；那里没有内嵌内容时为 `None`。
    pub fn load(url: Url) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        let path = url.path().trim_start_matches('/');
        Ok(Self::get(path).map(|f| f.data))
    }

    /// 以 `path` 开头的内嵌资产路径（前缀为 `!bundled:`）。
    pub fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(Self::iter()
            .map(|p| format!("!bundled:{p}"))
            .filter(|p| p.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}
