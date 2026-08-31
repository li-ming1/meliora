# Meliora

一个用 Rust 编写、基于 GPUI 的现代桌面音乐播放器。

Meliora 是 [hummingbird](https://github.com/hummingbird-player/hummingbird) 的延续与重构，在保留本地音乐库与播放能力的基础上，集成了网易云音乐与酷狗音乐的在线服务（在线曲库、榜单、歌单、二维码登录、在线试听与下载）。

> **⚠️ 免责声明**
>
> 本项目是基于公开 API 接口开发的第三方音乐客户端，仅供个人学习和技术研究使用。
>
> - **数据来源**：所有音乐数据通过公开接口获取，本项目不存储、不传播任何音频文件
> - **版权声明**：音乐内容版权归原平台及版权方所有，请尊重知识产权，支持正版音乐
> - **使用限制**：禁止将本项目用于任何商业用途或违法行为
> - **责任声明**：因使用本项目产生的任何法律纠纷或损失，均由使用者自行承担
> - **争议处理**：如果官方音乐平台觉得本项目不妥，请通过 Issues 联系

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
- 酷狗音乐（`kugou`）：扫码登录、搜索、歌单、榜单、在线播放与下载

### 界面与系统

- 原生渲染 UI（GPUI），响应式布局，深浅主题
- 系统媒体控制：Windows SMTC、macOS Now Playing、Linux MPRIS
- 支持 Windows / macOS / Linux

## 技术栈

- Rust 2024 edition
- [GPUI](https://github.com/gpui-ce/gpui-ce)（gpui-ce / gpui-unofficial 分支）
- SQLx + SQLite（本地数据库）
- Symphonia / lofty / cpal / rubato / realfft

## 构建

### 前提

- Rust 工具链 `stable-x86_64-pc-windows-msvc`（见 `rust-toolchain.toml`）
- VS2022 MSVC、Windows SDK 与 CMake（`opusic-sys` 依赖）

标准安装的工具链可直接构建。本机工具链为非标准布局时，参考 [.cargo/config.toml.example](.cargo/config.toml.example) 与 [build-release.cmd.example](build-release.cmd.example) 自行配置（含真实路径，勿提交）。

### Windows

```powershell
cargo build --release --features kugou,netease
```

本机维护者可直接运行本地 `build-release.cmd`（已 gitignore，默认启用 `--features kugou,netease` 并嵌入应用图标）。

### 自定义 feature

```bash
cargo build --release --features kugou,netease
```

可用 features：`kugou`、`netease`、`online`、`console`、`runtime_shaders`。

### 数据与日志

- 数据目录：`%APPDATA%\meliora\data\`
- 日志文件：`%LOCALAPPDATA%\meliora\data\meliora.log`

## 致谢

本项目在开发过程中参考、借鉴并致谢以下开源项目：

| 项目 | 用途 |
|---|---|
| [NeteaseCloudMusicApiEnhanced/api-enhanced](https://github.com/NeteaseCloudMusicApiEnhanced/api-enhanced) | 网易云音乐 API 接口参考实现，`src/netease/` 中的端点与之对应 |
| [MakcRe/KuGouMusicApi](https://github.com/MakcRe/KuGouMusicApi) | 酷狗音乐 API 接口参考实现，`src/kugou/` 中的端点与之对应 |
| [hummingbird-player/hummingbird](https://github.com/hummingbird-player/hummingbird) | 本项目的前身，Meliora 基于其代码库延续与重构 |

此外，界面图标来自 [Tabler Icons](https://tabler.io/icons)（MIT License，详见 `assets/icons/LICENSE`）。

## 许可证

本项目基于 **Apache License 2.0** 发布，详见 [LICENSE](LICENSE)。

在使用在线服务（网易云音乐 / 酷狗音乐）时，请遵守相关平台的条款与适用法律，本项目仅供学习交流。
