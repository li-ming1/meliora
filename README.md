<div align="center">

<img src="assets/images/logo.png" width="110" alt="Meliora">

# Meliora

轻快流畅的桌面音乐播放器，本地与在线畅听。

  **[下载](https://github.com/li-ming1/meliora/releases)**  ·  [English](README_EN.md)  

[![License](https://img.shields.io/badge/License-Apache%202.0-blue)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-stable-orange)](https://www.rust-lang.org)
[![Platform](https://img.shields.io/badge/Platform-Windows%20%7C%20macOS%20%7C%20Linux-4a90d9)]()

</div>

***

Meliora 是 [hummingbird](https://github.com/hummingbird-player/hummingbird) 的延续与重构，在保留本地音乐库与播放能力的基础上，集成了网易云音乐与酷狗音乐的在线服务（在线曲库、榜单、歌单、二维码登录、在线试听与下载）。

> **⚠️ 免责声明**
>
> 本项目是基于公开 API 接口开发的第三方音乐客户端，仅供个人学习和技术研究使用。
>
> - **数据来源**：所有音乐数据通过公开接口获取，本项目不存储、不传播任何音频文件
>
> - **版权声明**：音乐内容版权归原平台及版权方所有，请尊重知识产权，支持正版音乐
>
> - **使用限制**：禁止将本项目用于任何商业用途或违法行为
>
> - **责任声明**：因使用本项目产生的任何法律纠纷或损失，均由使用者自行承担
>
> - **争议处理**：如果官方音乐平台觉得本项目不妥，可联系本项目更改或移除。

## 功能特性

### 本地音乐

- 扫描与管理本地音乐库，自动监听目录变化并增量更新

- 丰富的元数据解析（基于 [lofty](https://crates.io/crates/lofty)）：ID3v2、FLAC、MP4 等

- 内嵌封面提取与缓存，专辑 / 艺术家 / 曲目三级浏览

- 歌单管理：创建、排序、拖拽、导入导出

- ReplayGain 响度归一化

- 歌词支持：LRC / KRC / YRC

### 播放

- 多格式解码（基于 [Symphonia](https://crates.io/crates/symphonia)）：MP3、FLAC、AAC、ALAC、OGG、WAV 等

- 10 段均衡器与实时频谱可视化

- 高质量重采样（[Rubato](https://crates.io/crates/rubato)）

- 播放队列管理与会话恢复

### 在线服务（feature）

- 网易云音乐（`netease`）：扫码登录、搜索、歌单、榜单、在线播放与下载

- 酷狗音乐（`kugou`）：扫码登录即自动获取 VIP 权益；搜索、歌单、榜单、在线播放与下载

### 界面与系统

- 原生渲染 UI（GPUI），响应式布局，深浅主题

- 系统媒体控制：Windows SMTC、macOS Now Playing、Linux MPRIS

- 支持 Windows / macOS / Linux

## 截图

![浅色主题](screenshots/light.png)

![深色主题](screenshots/dark.png)

![设置界面](screenshots/settings.png)

## 构建

### 前置条件

- Rust 工具链 `stable`（Windows 分支固定为 `stable-x86_64-pc-windows-msvc`，见 `rust-toolchain.toml`）
- **Windows**：VS2026 MSVC C++ 工具链、Windows SDK、CMake（`opusic-sys` 链接原生音频处理库需要，图标嵌入还需要 SDK 中的 `rc.exe`）
- **Linux / macOS**：GPUI 的平台依赖（X11/Wayland 或 AppKit、系统字体库等），详见 [gpui-ce](https://github.com/gpui-ce/gpui-ce)

标准安装的工具链可直接编译。若本机工具链为非标准布局（如 VS2022 未注册进 vswhere、SDK/CMake 不在默认位置），参考 [.cargo/config.toml.example](.cargo/config.toml.example) 与 [build-release.cmd.example](build-release.cmd.example) 写入本机真实路径（这两个文件含私密路径，不会提交）。

### 基础构建（本地播放）

默认构建不启用任何在线功能，适合纯本地库播放：

```bash
cargo build --release
```

### 启用在线服务

在线服务按 feature 提供，追加在你的构建命令上即可：

```bash
# 酷狗 + 网易云（推荐）
cargo build --release --features kugou,netease

# 只启用其中一家
cargo build --release --features kugou
cargo build --release --features netease
```

### Feature 说明

| feature | 作用 |
|---|---|
| `kugou` | 酷狗音乐在线服务（扫码登录、搜索、歌单、榜单、在线播放与下载），聚合 `online` 与 `online_sources` |
| `netease` | 网易云音乐在线服务，同上聚合 |
| `online` | 在线服务基础依赖（HTTP 客户端） |
| `online_sources` | 在线曲目公共管道：HTTP-range 流式播放、封面缓存、在线队列项，由上面的 provider feature 自动开启 |
| `console` | [tokio-console](https://github.com/tokio-rs/console) 运行时诊断 |
| `runtime_shaders` | GPUI 着色器改为运行时编译（调试着色器用） |

注意：`default = []`，默认构建不包含以上任何 feature。

### Windows 一键构建

Windows 下推荐使用仓库内的 `build-release.cmd`，它自动把本机 MSVC / Windows SDK / CMake 路径加入 PATH，再构建 release（默认已带 `--features kugou,netease`）：

```powershell
.\build-release.cmd
```

### 运行与数据

- 数据目录：`%APPDATA%\meliora\data\`（settings.json、library.db、播放会话等）
- 日志文件：`%LOCALAPPDATA%\meliora\data\meliora.log`
- 完全重置：退出程序后删除以上两个 `meliora` 文件夹

## 致谢

本项目在开发过程中参考了以下开源项目：

| 项目                                                                                                        | 用途                                       |
| --------------------------------------------------------------------------------------------------------- | ---------------------------------------- |
| [NeteaseCloudMusicApiEnhanced/api-enhanced](https://github.com/NeteaseCloudMusicApiEnhanced/api-enhanced) | 网易云音乐 API 接口参考实现，`src/netease/` 中的端点与之对应 |
| [MakcRe/KuGouMusicApi](https://github.com/MakcRe/KuGouMusicApi)                                           | 酷狗音乐 API 接口参考实现，`src/kugou/` 中的端点与之对应    |
| [hummingbird-player/hummingbird](https://github.com/hummingbird-player/hummingbird)                       | 本项目的前身，Meliora 基于其代码库延续与重构               |

此外，界面图标来自 [Tabler Icons](https://tabler.io/icons)（MIT License，详见 `assets/icons/LICENSE`）。

## ⭐ 支持项目

如果您觉得这个项目对您有帮助，欢迎给我们一个 Star！您的支持是我们持续改进的动力。

[![GitHub stars](https://img.shields.io/github/stars/li-ming1/meliora?style=social)](https://github.com/li-ming1/meliora)

## ✅ 反馈

如有任何问题或建议，欢迎提交 [issue](https://github.com/li-ming1/meliora/issues) 或 [pull request](https://github.com/li-ming1/meliora/pulls)。

## 许可证

本项目基于 **Apache License 2.0** 发布，详见 [LICENSE](LICENSE)。

在使用在线服务（网易云音乐 / 酷狗音乐）时，请遵守相关平台的条款与适用法律，本项目仅供学习交流。
