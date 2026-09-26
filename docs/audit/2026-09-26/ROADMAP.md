# 长远布局与改进清单

- **日期**：2026-09-26 · **基线**：`358f9ec3c`（main）
- **配套文档**：[REPORT.md](REPORT.md)（本轮全面审计主报告：34 条发现的证据与处置、5 项实施明细、校验记录）
- **数据来源**：2026-09-26 全面审计给定数据（synthesis / areas / findings / items / outcomes / checks）；发现编号沿用 REPORT.md（A-架构 / P-性能 / AL-算法 / U-UI / S-体积依赖 / Q-质量 / PR-产品）。

---

## 1. 愿景与原则

### 1.1 愿景：现象级产品

Meliora 的底层工程已达到一流播放器水准：gapless 预开轨（playback/thread/audio_engine.rs:269）、SMTC 全量控制（含 seek/shuffle/repeat/封面）、最多 16 段参数 EQ + 实时频谱、KRC/YRC 逐字卡拉OK歌词、带防作弊钳制的听歌统计（stats/recorder.rs）、音频回调 realtime-safe、常驻测量网（[mem] / [db] 探针 + alloc_guard 稳态零分配测试）。

当前的结构性短板只有一个方向：**在线音源层零抽象**（156 处 cfg 门、四对平行复制的千行文件、playback/stats/library/settings 反向依赖 `crate::ui`，REPORT.md A-1）——它决定了"多音源、插件化、远程控制"路线的边际成本。

产品层面的差距在"最后一公里"的用户价值暴露：

- 对标 **foobar2000 / MusicBee**：差距在曲库利用——最近添加/流派/播放次数视图、标签编辑、可配置列；
- 对标 **YesPlayMusic / Listen1**：差距在本地歌与在线服务的联动——本地歌补歌词/封面；
- 横切缺口：键盘操作（快捷键体系）、预设（EQ）、空态引导（首启）、库内智能视图——低成本高感知。

### 1.2 原则（不可让步）

1. **先测量后优化**（`GPUI_HARDCORE_PERFORMANCE.md`）：禁止凭感觉优化，结论必须由 profiling/benchmark 支撑；没有数据证明收益的优化视为失败并回滚。本清单所有"需先测量"条目即此纪律的执行位。
2. **热路径纪律**：render / audio callback / scroll 禁止分配、锁与 IO；数据生命周期最小化；有界队列与缓存；虚拟化大列表。实时音频路径的安全性优先于一切——本清单任何条目都不得触碰音频回调行为（本轮 5 项实施的硬性门槛之一）。
3. **改动门槛标尺**（本轮 5 项验证有效）：单事项 ≤4 文件、零新增依赖、纯编译+读码即可建立信心、不碰音频回调行为。满足即"可直接做"；超出者默认进入分期规划，仅两类豁免可留在近期清单且**必须注明豁免类型**——豁免 a：纯机械、无行为改动的多文件批量修改（如常量提取）；豁免 b：改动本身编译可验、但行为验证依赖本机不具备的外部环境（须注明环境缺口与顺延条件）。
4. **编译纪律**（AGENTS.md）：改动批量完成后一次编译验证；一律 release 模式走 `build-release.cmd`；禁止 debug 产物。

---

## 2. 分期规划

### 2.1 近期（低风险可落地，下批首选）

均满足 §1.2.3 改动门槛标尺，**#12 适用豁免 a、#13 适用豁免 b**（行内已注明）；按建议优先级排序：

| # | 事项 | 来源发现 | 规模 | 说明 |
|---|---|---|---|---|
| 1 | token_key 上移，消除 library ⇄ media 模块环 | A-2 | 2-3 文件，纯移动函数 | 上移到 media（或独立 text 工具模块），library 反向引用；media 层从此可独立复用/拆分 |
| 2 | auto_height 专辑网格消除实体 churn | P-1 | 1-2 文件 | 最小改法：auto_height 分支跳过 prune_views（艺人专辑数通常几十）；或改可滚动虚拟网格 |
| 3 | Lyrics 补 observe queue_width | U-2 | 1 处观察 + diff 守卫 | 修复拖分隔条后歌词行宽滞后；notify 只在宽度真变时发，避免拖动期逐帧重渲 |
| 4 | 播放线程离线日志 helper | Q-1 | 1 文件（interface.rs） | send_cmd helper + 21 处替换；send Err 时限频 warn"播放子系统已离线"，消除假死零痕迹 |
| 5 | 命令面板 unknown action 降级 | Q-3 | 1 文件 | build_action 失败 error!+跳过条目；JSON 解析 expect 保留（硬损坏应 fail fast） |
| 6 | zh-CN 补齐缺失 3 键 | U-4 | 1 文件 3 行 | EXPORT_PLAYLIST_FAILED / QUEUE_REMOVE_CANCELLED_STALE / STATS_TOP_TITLE；meta.json 随构建重写属正常 diff |
| 7 | kugou upgrade_vip_reward 失败日志 | Q-5 | 1 行 | 至少 warn!；勿动领取守卫语义（bc53afa060/b4ae5ccba6） |
| 8 | build.rs 三处小修 | S-4 | 1 文件 | 补 rerun-if-changed=translations；.env 合并单次读取；删 package/ 回退分支 |
| 9 | 顶栏搜索占位文案与实现对齐 | PR-6 | 1 行 + 多语言键同步 | "Search songs, artists or lyrics" → 与索引实际范围一致 |
| 10 | [mem] 探针扩展 | P-2 | main.rs + managed_image.rs 小改 | 活动瓦片数按窗口去重计数；covers 求和降频到每 10 tick |
| 11 | 续体回收两前提注释固化 | P-3 | 纯注释 | strong_count==1 与"unmount 即无重放"两前提写进代码 + 同步 patches/UPSTREAM_NOTES.md |
| 12 | 拖拽高亮色常量化（视觉一致性第一批） | U-5 | design.rs + 9 处引用（约 6 文件） | rgba(0x88888822) 提为 DRAG_OVER_BG 常量。**豁免 a**：约 6 文件 >4 文件上限，纯机械常量化、无行为改动 |
| 13 | MPRIS unwrap 降级 | Q-4 | 1 文件 | map_err 成默认属性 + debug!。**豁免 b**：改动本身编译可验，行为验证依赖 Linux 环境（本机 Windows 无此环境）；无 Linux 反馈可无限期顺延 |

### 2.2 中期（需独立一轮设计，或先测量）

| # | 事项 | 来源发现 | 前置条件 | 说明 |
|---|---|---|---|---|
| 1 | 快捷键体系·第一期 | PR-1 | 无 | 注册 VolumeUp/Down、SeekForward/Backward、ToggleQueue/ToggleLyrics、Mute 等 action + keybinds.json 默认键位 + commands.json 收录 |
| 2 | 快捷键体系·第二期 | PR-1 | 第一期 | load_default_keymap 扩为"默认表 + data 目录用户 keymap.json 合并"（解析器现成） |
| 3 | EQ 预设与导入导出 | PR-2 | 频段模板设计 | 内置 8-10 经典预设 + 保存为我的预设 + JSON 导入导出（复用 OpenThemeFolder 模式；dropdown.rs 现成） |
| 4 | 首启空态引导 | PR-3 | UI 设计与文案 | 扫描中复用 ScanProgress；完成后仍 0 则"添加音乐文件夹"深链设置 Library 分区（open_settings_window_with_section 已支持） |
| 5 | 曲库智能视图·第一期 | PR-4 | 无 | 侧栏 Recently Added（created_at 倒序复用 TrackView）+ 流派/添加日期可显示列 |
| 6 | 曲库智能视图·第二期（Most Played） | PR-4 | **先测量** listen_event 聚合查询在大库的表现 | 智能歌单 + 时间窗筛选 |
| 7 | 本地歌词在线补全 | PR-5 | 搜索 API 选择 + 写回策略 | 标题+艺人调酷狗/网易搜索，用户选择后写 sidecar 或 lyrics 表；封面补全二期；注意 feature 门控与网络请求生命周期 |
| 8 | 在线下载流式化 | PR-7 | 取消语义设计 | bytes_stream 逐块写临时文件 + 进度节流 + 取消 token + 原子 rename（与 session_storage 样板同构） |
| 9 | Track 预取预热 album_cache + FIFO 上界 | AL-3 | **先测量**：冷滚动一次的 [db] 曲线（改前基线已有常驻探针） | 后台 IN 查询回填（参照 Album::prefetch_rows generation 写法）+ RowCache 同款上限（table.rs:78 先例）；后台回填涉及 Entity 跨线程更新细节 |
| 10 | icu 伞依赖 granular 化 | S-1 | **先实测基线**：exe 体积（"当前 49MB" 为 [断言]——给定数据断言、无来源命令）与全量构建时长 | 上游 PR（周期不受本仓控制）或 [patch.crates-io] fork（仓库已有 vendor patch 先例）；教义第 22 条：改动前后必须实测 |
| 11 | Models 字段域分组约定 | A-5 | 无（工程约定） | 新字段按域归组（UI 布局态/库缓存态/播放镜像态），冻结无序膨胀；写进 AGENTS.md 或代码注释 |
| 12 | 设置变更定向 refresh | A-4 | Entity 拆分复杂度权衡 | Settings 子结构拆独立 Entity 按分区订阅，或 diff 后按脏分区定向 refresh |

### 2.3 远期（大重构，分批推进）

| # | 事项 | 来源发现 | 分步策略 |
|---|---|---|---|
| 1 | **在线音源层 trait + 注册表**（全清单第一优先级） | A-1 | 三步走，每步独立可回退：①流注册表、liked 缓存、refresh_online_url 下沉到 media 同级 online_sources 层——**本步交付为普通模块函数/类型（不含 trait 抽象）**，playback/stats 只依赖该层，ui 保留登录/设置页，**可先行**；②引入 OnlineSourceProvider trait（identity 解析/refresh_url/search/lyrics/like/playlists/ranks）+ 注册表，**把①下沉的函数收编为该 trait 的默认实现**，以 media::MediaProvider + lookup_table 为模板，替换 OnlineIdentity 枚举匹配与 ui/online.rs 的 match；③视图层平行文件最后迁移。**反向依赖消解边界**（REPORT §3.1 A-1 附注逐点核对）：crate::ui 反向引用实测 17 处，第①步可直接消解 4 处（thread.rs×2、stats/mod.rs×2 的 matching/refresh 调用），其余 13 处（app::Pool×2、Models/Queue/PlaylistEvent/SettingsHealth 状态类型×4、managed_image 漏斗排水×2、main.rs 入口/探针×7）需②及配套的 Pool/状态类型/漏斗排水下沉——"消掉全部反向依赖"是①+②+配套下沉的合计终态，非①单独可达。规模：预计 >30 文件；"每加一个音源从 16 文件 4-5k 行变为注册一行"中 16 文件/4-5k 行为给定数据断言（未附清单，见 REPORT A-1 附注） |
| 2 | LibraryAccess 去 block_on 化 | A-3 | 统一为"后台 spawn + Entity 快照回填"单一惯例（ui/library/files_view/loader.rs 已是样板），LibraryAccess 收缩到低频路径；顺带把 Pool 从 crate::ui::app 移到 library 模块；30+ 调用点分批进行 |
| 3 | 视觉一致性全量归一 | U-5 | 40 处 raw text_size(px) 归一 text_* 档位（先 stats.rs 19 处）+ controls.rs 非主题色改 theme 字段；11+ 文件；归一后 ±1-2px 变化需逐页过观感 |
| 4 | windows 0.61/0.62 双主版本统一 | S-3 | **不主动做**（已否决主动降版）：随 gpui-ce 升级到 windows 0.62 自然统一；仅影响冷构建时间 |

---

## 3. 全量 backlog 表

34 条编号发现全量收录；"本轮实施"5 项已完成（2026-09-26，明细见 REPORT.md §4）。

**收益列数字出处分级**（"先测量后优化"纪律的执行要求）：**[实测]**＝本轮或仓库既有常驻探针的实测；**[估计]**＝原始审计的机制推算，未经计时测量；**[断言]**＝给定数据的断言，无来源命令或清单；**[历史实测]**＝AGENTS.md / patches/ 记载的既往实测。[估计]/[断言]数字在动手前必须重新实测，不得作为优化依据直接引用。

| 编号 | 标题 | 维度 | 收益 | 代价 | 风险 | 处置与理由 |
|---|---|---|---|---|---|---|
| A-1 | 在线音源层零抽象（trait+注册表） | 架构 | 消掉全部 crate::ui 反向依赖（逐点边界见 REPORT A-1 附注：①步覆盖 4/17 处，其余需②+配套下沉）；每加音源 16 文件 4-5k 行→注册一行 [断言]；跨源修复只做一遍 | >30 文件长线重构，三步走 | 中：触碰 156 处 cfg 门与平行复制文件，需分步可回退 | 留作建议→远期第一优先级；第一步（状态下沉）可先做 |
| A-2 | token_key 上移消模块环 | 架构 | media 层可独立复用/拆 crate | 2-3 文件纯移动 | 低 | 留作建议；近期下批首选 refactor |
| A-3 | LibraryAccess 去 UI 线程 block_on | 架构 | 大库/慢盘不卡 UI；终结双模式并存 | 30+ 调用点分批；Pool 迁移 | 中：逐点回归 UI 交互 | 留作建议；远期分批 |
| A-4 | 设置变更定向 refresh | 架构 | 改设置不再整窗 rebuild | Settings 拆 Entity 或 diff 定向 | 中：Entity 拆分复杂度 | 留作建议；低频路径，收益有限 |
| A-5 | Models 字段域分组 | 架构 | 降低演进摩擦 | 纯工程约定 | 极低 | 留作建议；写进工程约定即可 |
| P-1 | auto_height 网格实体 churn | 性能 | 消除 >129 专辑页每帧约 N-129 个实体的建剪循环 [估计]（机制推算，未计时/未计数实测） | 跳过 prune_views 一处或改虚拟网格 | 低-中：渲染路径需观感验证 | 留作建议；近期下批首选（触发面窄：需艺人专辑数>129） |
| P-2 | [mem] 探针扩展归因 | 性能 | 卡顿/内存曲线可日志内直接归因 | main.rs 小改 | 低 | 留作建议；测量工具扩展，随排查需要兑现 |
| P-3 | 续体回收前提注释固化 | 性能 | 防未来改动破坏安全性论证 | 纯注释 | 极低 | 留作建议；随任意后续提交 |
| AL-1 | 搜索索引免全量重建 | 算法 | 打字/退格路径从 O(全库×format!) 降为 O(在线结果数≤30)（结构性论证）；"100k 库每防抖键 30-80ms 主线程卡顿" [估计]（原始审计推算，**未经计时测量**） | 已完成（3 文件） | 低（编译校验通过；在线搜索手动回归未做） | **本轮实施 ✅** |
| AL-2 | SQL 笛卡尔积改线性 | 算法 | 中间行数乘法→加法 [实测]（合成数据集探针 7183/6215/6213→30，四方输出一致）；"真实大库秒级→毫秒级" [估计]（行数比≠墙钟时间，**未经计时测量**） | 已完成（2 文件） | 低（最小数据集四方逐行一致验证） | **本轮实施 ✅** |
| AL-3 | Track 预取预热 album_cache | 算法 | 冷滚动每新专辑 1 次 UI 线程 DB 查询消除；cache 有界 | 后台 IN 回填 + FIFO 上限 | 中：Entity 跨线程更新细节 | **需先测量**：[db] 冷滚动基线；教义 §1 纪律 |
| U-1 | 歌词空转帧循环 | UI | 消除交互后 2s×刷新率整窗重绘（空闲功耗） | 已完成（1 文件） | 低（恢复闭环已读码论证；GPU/CPU 归零手动验证未做） | **本轮实施 ✅**（负条件形态，见 REPORT §4.3 偏离说明） |
| U-2 | queue_width observe 缺失 | UI | 拖分隔条行宽即时正确 | 1 行 observe+diff 守卫 | 低 | 留作建议；近期 |
| U-3 | 居中目标 emphasis 起点定格 | UI | 消除激活行数像素停驻偏差 | 每帧重算 target 或结束帧 snap | 中：重锚定可能引入抖动，需实测观感 | 留作建议；观感验证后 |
| U-4 | i18n 覆盖率低 | UI | zh-CN 三处英文硬回退消除；es/ja 用户近半英文改善 | 3 键 1 文件起；逐语言补词条量大 | 低（分批） | 留作建议；3 键近期顺带，es/ja 中期，<60% 语言选择器标注 |
| U-5 | 视觉一致性散落 | UI | 同类信息字号统一；拖拽色/主题色一处调整 | 11+ 文件；±1-2px 观感复核 | 中：纯视觉 | 留作建议；DRAG_OVER_BG 常量化近期先行，全量远期 |
| S-1 | icu 伞依赖 granular 化 | 体积 | "分钟级编译时间 + CLDR 数据段" [估计]；icu_experimental rlib 82-92MB×3、data 61MB+ [实测]（target/release/deps 产物体积） | 上游 PR 周期或 fork 维护成本 | 中：fork 漂移 | **需先测量**：先实测构建时长与 exe 体积基线（"exe 49MB" 为 [断言]：给定数据断言、无来源命令） |
| S-2 | 依赖清单纠偏（itertools/windows/sysinfo） | 体积 | 编译面缩小；对上游 feature 变化免疫（Threading/SystemServices 不再靠 cpal 透传） | 已完成（Cargo.toml + thread.rs） | 低（两处前提证伪已按实修正） | **本轮实施 ✅** |
| S-3 | windows 0.61/0.62 双主版本 | 体积 | 冷构建时间收益小 | 主动降版需 API 适配 | 高 | **已否决**：高风险无收益，随 gpui-ce 升级自然统一 |
| S-4 | build.rs 三处健壮性细节 | 体积/工程 | 消翻译再生成盲区；去冗余 IO 与死代码 | 1 文件 | 低 | 留作建议；近期顺带 |
| S-5 | release.yml 四个流程缺口 | 工程 | 可复现性 + 供应链加固（钉版/sha 追溯/checksum/timeout） | 改 CI 无法本机验证 | 低-中：需下次发布实测 | 留作建议；工程化待办（§5.1） |
| Q-1 | 播放线程 panic 后静默吞命令 | 质量 | "应用假死"可排查（可观测性） | 1 文件 helper + 21 处替换 | 低：仅增日志 | 留作建议；近期下批候选 |
| Q-2 | 凭据文件非原子落盘 | 质量 | 扫码登录态防截断丢失；读错误可查 | 已完成（2 文件） | 低（读码自查完成；登录态手动验证未做） | **本轮实施 ✅** |
| Q-3 | 命令面板 unknown action panic | 质量 | 资源不同步不再放大为启动崩溃 | 1 文件 | 低 | 留作建议；近期下批候选 |
| Q-4 | Linux MPRIS 13 处 unwrap | 质量 | D-Bus 异常时属性降级而非任务崩溃 | map_err + debug! | 低；需 Linux 回归环境 | 留作建议；有 Linux 反馈时首批修复 |
| Q-5 | Kugou VIP 升级奖励失败被吞 | 质量 | 失败可查、可重试决策 | 1 行 warn! | 极低 | 留作建议；随 kugou 提交顺带 |
| PR-1 | 快捷键缺核心动作且不可自定义 | 产品 | 键盘党盲操作音量/进度/面板（每天感知） | 一期多文件；二期 keymap 合并 | 中：分派与提示面 | 留作建议；近期高优先功能，分两期 |
| PR-2 | EQ 无预设无导入导出 | 产品 | EQ 从空白画布变为可用（使用前提） | 频段模板 + UI 入口设计 | 中 | 留作建议；中期 |
| PR-3 | 首启零引导 | 产品 | 首启漏斗补全 | 空态页设计+文案 | 中 | 留作建议；中期 |
| PR-4 | 统计与曲库脱节 | 产品 | 对标 MusicBee/foobar 的库利用差距；"最近添加"是使用频次仅次于搜索的视图 | 新查询 + 侧栏 + 列 | 中；Most Played 需先测量聚合 | 留作建议；分两期（REPORT PR-4） |
| PR-5 | 本地歌无在线歌词/封面补全 | 产品 | 盘活整个本地库的歌词体验（管道大部分现成：kugou/download.rs:120 fetch_lyrics） | 搜索 API 选择 + 写回策略 | 中：feature 门控与网络请求生命周期 | 留作建议；独立设计 |
| PR-6 | 搜索占位承诺"搜歌词" | 产品 | 消除文案过度承诺 | 1 行 + 多语言键同步 | 低 | 留作建议；顺带 |
| PR-7 | 在线下载整文件进内存 | 产品 | HiRes 下载有进度可取消、内存平稳 | bytes_stream + 临时文件 + 取消 token | 中：取消语义设计 | 留作建议；独立一轮 |
| PR-8 | 自动更新设计储备 | 产品 | 分发闭环（远期） | 依赖评审 + 签名/渠道/校验体系 | 高：**AGENTS.md 禁令在先** | 留作建议：禁令未撤销前绝不实施；设计储备见 §4 |

---

## 4. 自动更新功能设计建议（专节·设计储备）

> **状态声明（必读）**：AGENTS.md 明令"**自动更新功能已完全移除……不要再引入或引用**"。本节仅为路线图设计储备，**不是实施授权**。任何实施发生前必须：①用户正式拍板撤销 AGENTS.md 的禁令；②走依赖评审重新引入被删依赖（minisign-verify、semver、winreg 等）及对应 feature/设置项/UI。在此之前，任何代理不得把本节当实施依据。

### 4.1 现存地基盘点（2026-09-26 已核实，REPORT PR-8）

| 地基 | 位置 | 现状 |
|---|---|---|
| 多平台发布矩阵 | `.github/workflows/release.yml` | cross-compile 发布矩阵保留在仓（本轮已读）；但有 4 个缺口（见 §5.1：工具链浮动、版本号无 sha 追溯、无 checksum、release-distro 死配置） |
| 渠道机制 | `build.rs:80-99`（MELIORA_RELEASE_CHANNEL） | 渠道默认 stable、可环境变量覆盖，stable 分支 id 回退 "release" 逻辑在位 |
| HTTP 客户端与下载落盘管道 | zed-reqwest；`src/ui/kugou/download.rs` | HTTP(S) 下载→内存/落盘→toast 的管道模式已有现成先例 |

### 4.2 设计要点（仅收录审计数据明示项，不虚构细节）

1. **版本可追溯**：build 步注入 `MELIORA_VERSION_ID: ${{ github.sha }}`（当前缺失，产物版本号无 sha 追溯）——这是更新检查"哪个版本是哪个提交"的前提，也是 S-5 缺口之一，应最先补。
2. **产物可校验**：release job 生成 sha256sums.txt 随产物上传（当前缺失）——更新包下载后先验哈希再落盘。
3. **签名校验**：minisign 类签名 + 内置公钥（被删依赖 re-introduce 时走依赖评审）；先校验后安装。
4. **下载与落盘**：流式写临时文件 + 原子 rename（与 session_storage.rs / 本轮凭据落盘 Q-2 同构）；进度反馈 + 取消。
5. **渠道隔离**：stable/beta 由 MELIORA_RELEASE_CHANNEL 决定，更新检查只查同渠道。
6. **崩溃符号**：[profile.release-distro]（Cargo.toml:233，当前全仓唯一引用的死配置）去留在此一并拍板——若为崩溃符号/更新诊断接线则在 CI 接线，否则删除。
7. **完整设计**：原始审计材料中的完整设计（roadmapNotes 第 6 条）未随本次给定数据提供，其余细节（更新清单格式、灰度策略、失败回滚等）留待禁令撤销后的专项设计，**本节不做虚构**。

---

## 5. 工程化与流程

### 5.1 CI（release.yml 四个缺口 → 工程化待办）

| 缺口 | 现状（已核实） | 动作 |
|---|---|---|
| 工具链浮动 | :43-50 删 rust-toolchain.toml 后用 dtolnay/rust-toolchain@stable | 矩阵钉 Rust minor 版本，定期手动升级 |
| 版本号无 sha 追溯 | Build release 步 env 仅 CFLAGS_aarch64_pc_windows_msvc（:70-75），无 MELIORA_RELEASE_CHANNEL/MELIORA_VERSION_ID；build.rs:92-99 stable 分支 id 回退 "release" | 注入 `MELIORA_VERSION_ID: ${{ github.sha }}` |
| 产物无校验和 | 上传 zip/tar.gz 无 sha256 清单 | release job 生成 sha256sums.txt |
| release-distro 死配置 | [profile.release-distro]（Cargo.toml:233）全仓仅此一处，release.yml 无引用 | 拍板去留：需要崩溃符号支持则在 CI 接线，否则删除 |
| （附加）防悬挂 | — | job 加 timeout-minutes |

约束：CI 改动本机无法验证，需下次发布实测；不阻塞任何功能。

### 5.2 bench 与测量体系

**已有常驻测量网（全部经读码复核，REPORT §3.2/§3.3）**：

- [mem] periodic 探针：private/working/heap_commit/non_heap/render_cache/img_cache/covers/回收漏斗五计数 + step 告警附 mimalloc dump（src/main.rs:263-381）；
- [db] UI 线程查询探针：UI_QUERY_SLOW_MICROS=2000 慢查询告警 + 250ms 步进总量（src/library/db.rs:982-1028）；
- alloc_guard 稳态零分配测试常驻把关 EQ 流式路径；
- record.rs:214-243 checkpoint bench、db.rs:984-997 探针（决策附带实测数据的先例）。

**缺口与动作（按"先测量后优化"纪律）**：

| 缺口 | 动作 | 服务于 |
|---|---|---|
| 帧时间无归因（set_trace_enabled(false)） | 按需 samply/ETW，不给 uniform_list 加常驻打点 | 滚动卡顿归因（U-3/P-1 观感验证的前置） |
| 活动瓦片数缺失 | ManagedImage::paint 成功路径按窗口去重计数随 [mem] 输出 | non_heap 曲线与"每瓦片 ~2.3MB 驱动提交"预算对账 [历史实测]（AGENTS.md / patches/UPSTREAM_NOTES.md 记载，非本轮测量） |
| covers 求和每 30s 全量 readdir | 降频到每 10 tick 或启动扫一次+增量累计 | 万级封面文件的常驻周期 IO |
| 冷滚动 [db] 曲线基线 | 用现有探针实测改前基线 | AL-3（album_cache 预热）的实施依据 |
| Most Played 聚合查询大库表现 | listen_event 聚合 + 时间窗实测 | PR-4 第二期 |
| icu/windows/sysinfo 收窄的体积与构建时长 | 改前实测基线（exe 体积——"49MB" 为 [断言]、全量构建时长），改后对比 | S-1/S-2 收益确认（教义第 22 条） |

纪律重申：Cargo.toml [profile] 既有决策有 bench 注释支撑，未发现推翻依据，不动。

### 5.3 发布与构建约定

- 构建唯一入口 `build-release.cmd`（默认带 `--features kugou`；自动杀死运行中的 Meliora 进程）；禁止 debug 产物。
- 代码格式统一由 `cargo fmt --all` 管理；全仓 fmt 用 `style:` 前缀单独提交，不与功能混合。
- `translations/meta.json` 由 build.rs 的 i18n 生成器构建期重写，其 diff 属正常，随改动一起提交。
- 编译期红线目录（`migrations/`、`queries/`、`assets/`、`translations/`）被宏引用，绝不能删。
- 发布渠道默认 `stable`（build.rs 内置默认值，MELIORA_RELEASE_CHANNEL 可覆盖）。
- Git 约定：main 分支、conventional commits（chore/fix/refactor/feat/style/perf）。
