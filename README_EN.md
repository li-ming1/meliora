# Meliora

[简体中文](README.md) · **English**

[![License](https://img.shields.io/badge/License-Apache%202.0-blue)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-stable-orange)](https://www.rust-lang.org)
[![Platform](https://img.shields.io/badge/Platform-Windows%20%7C%20macOS%20%7C%20Linux-4a90d9)]()

A lightweight third-party desktop music player written in Rust, built on GPUI — shipped as a single file, with low memory usage and fast startup.

Meliora is a continuation and rewrite of [hummingbird](https://github.com/hummingbird-player/hummingbird). It keeps the local music library and playback capabilities, and adds online services for NetEase Cloud Music and KuGou Music (online catalog, charts, playlists, QR-code login, online streaming and downloading).

> **⚠️ Disclaimer**
>
> This project is a third-party music client developed based on publicly available APIs. It is intended solely for personal learning and technical research.
>
> - **Data source**: All music data is obtained through public interfaces; this project does not store or distribute any audio files
> - **Copyright**: Music content belongs to the original platforms and copyright holders. Please respect intellectual property and support licensed music
> - **Usage restrictions**: Commercial use or any illegal activity with this project is prohibited
> - **Liability**: Any legal disputes or losses arising from the use of this project are the sole responsibility of the user
> - **Dispute resolution**: If the official music platforms consider this project inappropriate, please contact us via Issues

## Features

### Local Library

- Scan and manage your local music library, with automatic folder watching and incremental updates
- Rich metadata parsing (via [lofty](https://crates.io/crates/lofty)): ID3v2, FLAC, MP4, and more
- Embedded cover-art extraction and caching, with Album / Artist / Track browsing
- Playlist management: create, reorder, drag-and-drop, import/export
- ReplayGain loudness normalization
- Lyrics support: LRC / KRC / YRC

### Playback

- Multi-format decoding (via [Symphonia](https://crates.io/crates/symphonia)): MP3, FLAC, AAC, ALAC, OGG, WAV, and more
- 10-band equalizer with real-time spectrum visualization
- High-quality resampling ([Rubato](https://crates.io/crates/rubato))
- Play queue management and session persistence

### Online Services (features)

- NetEase Cloud Music (`netease`): QR-code login, search, playlists, charts, online streaming and downloading
- KuGou Music (`kugou`): QR-code login, search, playlists, charts, online streaming and downloading

### UI & System

- Natively rendered UI (GPUI), responsive layout, light and dark themes
- System media controls: Windows SMTC, macOS Now Playing, Linux MPRIS
- Supports Windows / macOS / Linux

## Screenshots

![Light theme](screenshots/light.png)

![Dark theme](screenshots/dark.png)

![Settings](screenshots/settings.png)

## Tech Stack

- Rust 2024 edition
- [GPUI](https://github.com/gpui-ce/gpui-ce) (gpui-ce / gpui-unofficial fork)
- SQLx + SQLite (local database)
- Symphonia / lofty / cpal / rubato / realfft

## Building

### Prerequisites

- Rust toolchain `stable-x86_64-pc-windows-msvc` (see `rust-toolchain.toml`)
- VS2022 MSVC, Windows SDK and CMake (required by `opusic-sys`)

A standard toolchain installation builds out of the box. If your toolchain has a non-standard layout, follow [.cargo/config.toml.example](.cargo/config.toml.example) and [build-release.cmd.example](build-release.cmd.example) (contains real paths — do not commit).

### Windows

```powershell
cargo build --release --features kugou,netease
```

### Custom Features

```bash
cargo build --release --features kugou,netease
```

Available features: `kugou`, `netease`, `online`, `console`, `runtime_shaders`.

### Data & Logs

- Data directory: `%APPDATA%\meliora\data\`
- Log file: `%LOCALAPPDATA%\meliora\data\meliora.log`

## Acknowledgments

This project references the following open-source projects:

| Project | Purpose |
|---|---|
| [NeteaseCloudMusicApiEnhanced/api-enhanced](https://github.com/NeteaseCloudMusicApiEnhanced/api-enhanced) | Reference implementation for the NetEase Cloud Music API; the endpoints in `src/netease/` mirror it |
| [MakcRe/KuGouMusicApi](https://github.com/MakcRe/KuGouMusicApi) | Reference implementation for the KuGou Music API; the endpoints in `src/kugou/` mirror it |
| [hummingbird-player/hummingbird](https://github.com/hummingbird-player/hummingbird) | The predecessor of this project; Meliora is a continuation and rewrite of its codebase |

In addition, the UI icons come from [Tabler Icons](https://tabler.io/icons) (MIT License, see `assets/icons/LICENSE`).

## License

This project is released under the **Apache License 2.0**. See [LICENSE](LICENSE).

When using the online services (NetEase Cloud Music / KuGou Music), please comply with the terms of the respective platforms and applicable law. This project is for learning and communication purposes only.
