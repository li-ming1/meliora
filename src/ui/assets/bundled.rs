use std::borrow::Cow;

use gpui::SharedString;
use rust_embed::RustEmbed;
use url::Url;

/// The compile-time embedded subset of `assets/` (fonts, icons, images),
/// served through the `!bundled://` scheme.
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
    /// Bytes of the embedded asset at `url`'s path; `None` when nothing is
    /// embedded there.
    pub fn load(url: Url) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        let path = url.path().trim_start_matches('/');
        Ok(Self::get(path).map(|f| f.data))
    }

    /// Embedded asset paths (prefixed with `!bundled:`) that start with
    /// `path`.
    pub fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(Self::iter()
            .map(|p| format!("!bundled:{p}"))
            .filter(|p| p.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}
