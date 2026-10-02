use std::{collections::hash_map::DefaultHasher, env, hash::Hasher, io, path::PathBuf};

use anyhow::{Context as _, Result};

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
    // 本 build.rs 已发射多条 rerun 指令（i18n 生成器），embed-resource
    // 依赖的"无指令则扫描全包"默认随之失效，图标资源必须显式追踪；
    // rc 编译失败必须上抛，静默吞掉会拿旧图标出包。
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        println!("cargo:rerun-if-changed=assets/icon.rc");
        println!("cargo:rerun-if-changed=assets/app.ico");
        embed_resource::compile("assets/icon.rc", embed_resource::NONE)
            .manifest_required()
            .context("failed to compile assets/icon.rc")?;
    }

    // generate translations
    let path: PathBuf = env::var("CARGO_MANIFEST_DIR")
        .expect("CARGO_MANIFEST_DIR is not set")
        .into();

    cntp_i18n_gen::generate_default(&path);

    // 翻译词条指纹：tr_load! 是 proc-macro，在宏展开期读盘，stable 的 dep-info
    // 不追踪 proc-macro 的文件读取，而生成器只发射 rerun-if-changed=src——纯翻译
    // 提交原本静默丢失。逐文件发射 rerun-if-changed 只能让 build.rs 重跑，重跑后
    // 输出不变则主 crate 仍 fresh，故再把词条内容哈希注入 rustc-env：值变即失效
    // 主 crate 指纹，tr_load! 才会重新展开读入新词条。meta.json 是生成器产物
    // （含 src 行号引用，随源码变动重写），排除以免无谓重编，且照常不被本段改写。
    // 注意：新增词条文件不自带 rerun 指令，需 touch 现有词条或改动代码触发。
    let mut translations: Vec<(String, PathBuf)> = std::fs::read_dir(path.join("translations"))
        .context("failed to read translations directory")?
        .collect::<io::Result<Vec<_>>>()?
        .into_iter()
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (name, entry.path())
        })
        .filter(|(name, file)| file.is_file() && name.ends_with(".json") && name != "meta.json")
        .collect();
    translations.sort();
    let mut stamp = DefaultHasher::new();
    for (name, file) in &translations {
        println!("cargo:rerun-if-changed=translations/{name}");
        stamp.write(
            &std::fs::read(file).with_context(|| format!("failed to read translation '{name}'"))?,
        );
    }
    println!("cargo:rustc-env=MELIORA_I18N_STAMP={:016x}", stamp.finish());

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
