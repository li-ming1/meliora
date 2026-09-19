use std::{env, path::PathBuf};

use anyhow::{Context as _, Result};
use vergen::{Build, Emitter};

fn option_env(var: &str) -> Option<Result<String>> {
    println!("cargo:rerun-if-env-changed={var}");
    Some(std::env::var(var))
        .filter(|res| !matches!(res, Err(std::env::VarError::NotPresent)))
        .map(|res| res.with_context(|| format!("invalid environment variable '{var}'")))
}

fn version_id_from_git() -> Result<String> {
    println!("cargo:rerun-if-changed=.git/logs/HEAD");
    let git = std::process::Command::new("git")
        .arg("--git-dir=.git")
        .args(["rev-parse", "HEAD"])
        .output()
        .context("failed to run git: is it installed?")?;
    if git.status.success()
        && let output = git.stdout.trim_ascii_end()
        && output.iter().all(u8::is_ascii_hexdigit)
        && let Ok(sha) = std::str::from_utf8(output)
    {
        Ok(sha[..7].to_owned())
    } else {
        anyhow::bail!(
            "git returned an error: `git rev-parse HEAD` exited with {}\
            \n== stdout ==\n{}\n== stderr ==\n{}",
            git.status,
            String::from_utf8_lossy(&git.stdout),
            String::from_utf8_lossy(&git.stderr),
        );
    }
}

fn main() -> Result<()> {
    // The [mem] probe calls mimalloc's statistics API (mi_process_info /
    // mi_collect) via a bare extern block; the symbols live in the statically
    // linked mimalloc.lib, whose objects carry DLL-style export decorations,
    // so MSVC remarks LNK4217/LNK4286 on every reference. The import is
    // exactly what we want — silence the remark instead of the feature.
    // MSVC-only flags: other linkers parse them as file paths and fail the
    // build (observed on the macOS/Linux CI legs).
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/IGNORE:4217");
        println!("cargo:rustc-link-arg-bins=/IGNORE:4286");
    }

    // set build time information
    let build = Build::builder()
        .build_timestamp(true)
        .use_local(false)
        .build();

    Emitter::default().add_instructions(&build)?.emit()?;

    // read env from an optional local .env file (never created automatically)
    if let Ok(envfile) = std::fs::read_to_string(".env") {
        println!("cargo:rerun-if-changed=.env");
        dotenvy::from_read(envfile.as_bytes())?;
        let mut vars = dotenvy::from_read_iter(envfile.as_bytes());
        while let Some((key, _)) = vars.next().transpose()? {
            println!("cargo:rustc-env={key}={}", std::env::var(&key)?);
        }
    }

    // embed the application icon into the Windows executable
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        let _ = embed_resource::compile("assets/icon.rc", embed_resource::NONE);
    }

    // generate translations
    let path: PathBuf = env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR is not set")
        .into();

    cntp_i18n_gen::generate_default(&path);

    // prefer env over file, default to `stable`
    let channel = option_env("MELIORA_RELEASE_CHANNEL").unwrap_or_else(|| {
        match std::fs::read_to_string("package/RELEASE_CHANNEL") {
            Ok(channel) => {
                println!("cargo:rerun-if-changed=package/RELEASE_CHANNEL");
                Ok(channel.trim_ascii_end().to_owned())
            }
            Err(_) => Ok("stable".to_owned()),
        }
    });

    // get parenthesized version id based on channel kind
    channel.and_then(|kind| {
        let id = option_env("MELIORA_VERSION_ID");
        let (suffix, mut id) = match &*kind {
            "" | "dev" => ("-dev", id.unwrap_or_else(version_id_from_git)?),
            "stable" => ("", id.unwrap_or_else(|| Ok("release".to_owned()))?),
            "flake" => ("-flake", id.context("nix didn't provide git sha")??),
            other => anyhow::bail!("invalid release channel '{other}'"),
        };
        if !id.is_empty() {
            id = format!(" ({id})");
        }
        println!(
            "cargo:rustc-env=MELIORA_VERSION_STRING={}{suffix}{id}",
            std::env::var("CARGO_PKG_VERSION")?,
        );
        Ok(())
    })
}
