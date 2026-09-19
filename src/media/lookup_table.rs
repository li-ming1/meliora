use std::{fs::File, path::Path, sync::OnceLock};

use tracing::info;

use crate::media::traits::{MediaProvider, MediaProviderFeatures, MediaStream};

static PROVIDERS: OnceLock<Vec<Box<dyn MediaProvider>>> = OnceLock::new();

/// Registers the media providers. Called once at startup before any reads.
pub fn register_providers(providers: Vec<Box<dyn MediaProvider>>) {
    info!("Registering {} media provider(s)", providers.len());
    match PROVIDERS.set(providers) {
        Ok(()) => {}
        Err(_) => panic!("media providers registered twice"),
    }
}

fn providers() -> &'static [Box<dyn MediaProvider>] {
    PROVIDERS.get().expect("media providers not registered")
}

#[allow(clippy::borrowed_box)]
fn provider_can_read(
    path: &Path,
    required_features: MediaProviderFeatures,
    provider: &Box<dyn MediaProvider>,
) -> anyhow::Result<bool> {
    // mime-types are more reliable but windows is too slow to use them
    // so now we only use extensions
    if let Some(ext) = path.extension().and_then(|v| v.to_str())
        && provider
            .supported_extensions()
            .iter()
            .any(|v| v.eq_ignore_ascii_case(ext))
    {
        return Ok(provider.supported_features() & required_features == required_features);
    }

    Ok(false)
}

pub fn can_be_read(path: &Path, required_features: MediaProviderFeatures) -> anyhow::Result<bool> {
    for provider in providers() {
        if provider_can_read(path, required_features, provider)? {
            return Ok(true);
        }
    }

    Ok(false)
}

pub fn try_open_media(
    path: &Path,
    required_features: MediaProviderFeatures,
) -> anyhow::Result<Option<Box<dyn MediaStream>>> {
    let mut last_error = None;

    for provider in providers() {
        if provider_can_read(path, required_features, provider)? {
            let file = File::open(path)?;
            match provider.open(file, path.extension()) {
                Ok(stream) => return Ok(Some(stream)),
                Err(e) => last_error = Some(e),
            }
        }
    }

    if let Some(e) = last_error {
        Err(e.into())
    } else {
        Ok(None)
    }
}
