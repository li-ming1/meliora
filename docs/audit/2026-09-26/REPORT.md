# Meliora 项目全面审计报告（2026-09-26）

- **审计对象**：Meliora 音乐播放器（Rust + GPUI / SQLx+SQLite）
- **基线提交**：`358f9ec3c`（main 分支 HEAD，`style: cargo fmt 全仓整理`）
- **报告路径**：`docs/audit/2026-09-26/REPORT.md`（主交付物；长远规划见同目录 [ROADMAP.md](ROADMAP.md)）
- **数据来源与置信度声明**：本报告全部事实以审计工作流给定的结构化审计数据为准（synthesis 总览 / 7 个维度概览 / 34 条编号发现 / 5 项实施事项与实施结果 / 校验记录）。发现中的代码位置与证据由审计各阶段读码复核得出；**报告撰写者本人另行完成的只读实测**在 §5.2 单独列出命令与结果，未编译。
- **矛盾裁决规则（全文统一，仅此一套）**：①**给定数据内部的矛盾**——以 synthesis 总览与实施结果（outcomes）为准，实施结果晚于发现、更接近最终事实；②**给定数据与报告撰写者实测的矛盾**——以实测为准，逐条列于 §5.3。§4 各 notes 与 §3 各附注均引用这两条，不再另有第三套。

---

## 1. 执行摘要

### 1.1 总裁决

Meliora 核心栈质量确实很高：音频回调、队列、扫描、歌词、图集漏斗等教义条款经逐条读码复核全部属实。去伪存真后，7 个维度 40 条原始发现经合并与证据修正，形成本报告的 **34 条编号发现（其中 1 条最终否决，即 S-3）**；4 处原始证据的机制细节已修正——synthesis 原文按原始审计编号记为 P1/Q1/U5/A6，对应本报告的 P-1（剪窗算术）、Q-1（send 处数）、U-5（拖拽色处数）、A-5（字段数）——修正后结论全部成立，**没有一条虚构**。账目说明：给定数据的合并标注共涉及 **7 条原始编号**（A1+A2、P2+P3、S4+S2+S5），按此算术 40−7+3=36，与 34 相差 2-3 条；给定数据未交代其余原始发现的去向，原始 40 条完整对照清单亦不在数据内，本报告如实留此缺口、不虚构补齐（见 §2"编号与账目"、§6.6）。

最大的结构性债务是唯一方向：**在线音源层零抽象**——156 处 cfg 门、两份逐字节平行的千行文件、核心模块反向依赖 `crate::ui`。它决定了多音源路线的边际成本，属长线重构而非本轮动作。

本轮可直接做的 5 项已全部实施：两处算法/数据结构升级（SQL 笛卡尔积改线性、搜索索引免全量重建）、一处热路径修复（歌词空转帧循环）、一处依赖清理（itertools 移除——**实施中修正**：其并非纯死依赖，thread.rs:1063 存在 `Itertools::format` 活调用，先以 std 等价写法替换后删除；windows/sysinfo features 纠偏——删补清单的最终依据是逐方法核对 windows-0.62.2 生成源码的 cfg，rg 零引用与 cargo tree 透传关系仅是初筛证据）、一处数据丢失防护（在线凭据原子落盘）——全部满足硬性门槛（改动 ≤4 文件/项、零新增依赖、纯编译+读码建立信心、不碰音频回调行为）。

### 1.2 最重要的结论（5 条）

1. **教义条款复核全部属实；泄漏计数为零，但"无泄漏"结论受证据窗口限制。** 音频回调仅 ring 读取+原子量+增益 ramp（零分配零锁，`src/devices/cpal.rs:217-234`），EQ 流式路径有稳态零分配 alloc_guard 测试常驻把关；本机日志（937 行）中两个泄漏计数为零（`tiles_leaked=0`、`funnel_pending=0`）。**证据边界**：给定数据引用的 private 数字（2970s→3870s 窗口内 481→637MB）本身只有上涨；"会话期爬升部分回落"与"heap +49MB 且 30MB 可被强制回收"为给定数据的定性描述，未附回收后实测数据，日志覆盖总时长亦未给出。据此能支持的结论是"**该窗口内未出现泄漏计数非零的证据**"，全称"无泄漏"超出所引窗口的证据范围。
2. **结构性债务只有一条主线：在线音源层零抽象。** kugou/netease cfg 门全仓 156 处（82+74）；两文件各持逐字节相同的 STREAM_MAP/persist_stream_map_if_changed；非 UI 模块直接引用 `crate::ui`（给定数据记 10 处，报告撰写者实测全量为 17 处——给定清单未含 `src/main.rs` 的 7 处入口/探针引用，逐点清单见 §3.1 A-1 附注）。两个量化主张的出处等级：'每加一个在线音源需在 16 个文件复制全链路约 4-5k 行'为**给定数据的断言**（未附文件清单与推导，本轮未复核）；"第①步状态下沉即消掉全部反向依赖"经逐点核对**高估了第①步的覆盖面**（详见 A-1 附注）。这是路线图第一优先级（见 ROADMAP.md §2.3），本轮不动。
3. **本轮 5 项改进全部落盘，统一校验通过（注意归因边界）。** 基线 check / cargo fmt / 最终 check / build-release.cmd 均通过（见 §5）；但校验针对的是当时完整工作区树，实测树中除 5 项改动外还含 3 个计划外改动文件（§5.3.2），故"通过"不能单独归因于 5 项改动（详见 §5.1 归因边界）。SQL 改写经最小数据集"旧 SQL vs 新 SQL vs Python 参考 vs 手算"四方逐行一致验证，中间行数探针 7183/6215/6213 → 30（合成小集即约 207–239 倍，真实大库差距随曲目数二次增长；**行数比不等于墙钟时间，未做计时测量**）。
4. **实施中有两处给定前提被读码证伪，已按正确方向修正（以实施结果为准）：**
   - itertools 并非纯死依赖——`src/playback/thread.rs:1063`（原行号）的 `paths.iter().format(":")` 是 `Itertools::format` 活调用，先以 std 等价写法替换再删依赖；
   - windows features "5 项零引用"实为 4 项——`Foundation_Collections` 被 `StorageLibrary::Folders()` 的双 feature cfg 依赖（windows-0.62.2 Storage/mod.rs:5220），保留；`Storage_Search` 同理未收窄。
5. **下批候选已经清晰排队**（均达标、仅因本轮 5 项名额排满而顺延）：token_key 上移消 media⇄library 模块环、auto_height 网格实体 churn、Lyrics 补 observe queue_width、播放线程离线日志 helper、命令面板 unknown action 降级、zh-CN 缺失 3 键等——完整清单见 ROADMAP.md。

### 1.3 本轮实施 5 项

| # | 标题 | 类型 | 文件 | 建议提交信息 |
|---|---|---|---|---|
| 1 | 改写 find_artists_tracks 排序 SQL 消除 JOIN 笛卡尔积 | perf | `queries/library/find_artists_tracks_asc.sql`、`queries/library/find_artists_tracks_desc.sql` | `perf(library): find_artists_tracks 排序改写为预聚合子查询，消除双腿 JOIN 笛卡尔积` |
| 2 | 在线搜索结果更新不再全量重建 nucleo 索引 | perf | `src/ui/search/model.rs`、`src/ui/components/palette/finder.rs`、`src/ui/search/search_item.rs` | `perf(search): 在线搜索结果移出 nucleo 索引并预计算搜索文本，结果刷新/退格不再全量重建索引` |
| 3 | 歌词面板交互后停止空转帧循环 | fix | `src/ui/lyrics.rs` | `fix(ui): 歌词面板交互窗口内不再空转帧循环，重跟随由过期后首帧消费` |
| 4 | 清理 itertools 死依赖并纠偏 windows/sysinfo features | chore | `Cargo.toml`、`src/playback/thread.rs` | `chore: 清理 itertools 死依赖并纠偏 windows/sysinfo features` |
| 5 | 在线凭据文件改原子落盘并记录读取失败 | fix | `src/kugou/client.rs`、`src/netease/client.rs` | `fix(online): 酷狗/网易凭据文件改 tmp+rename 原子落盘，读取失败分级记日志` |

明细（摘要/验收/偏离/notes）见 §4。

### 1.4 校验结果

| 校验项 | 结果 |
|---|---|
| 基线 check | 通过 |
| cargo fmt | 已运行 |
| 最终 check | 通过 |
| build-release.cmd | 通过 |
| 提交状态 | 记载为"所有改动仅本地提交，未推送任何远端"；**报告撰写时实测 HEAD 仍为 `358f9ec3c3`、改动在工作区未提交**——矛盾以实测为准，详见 §5.3 |

> 注：四项校验的**归因边界**（工作区含 3 个计划外改动文件，通过不能单独归因于 5 项改动）见 §5.1。

---

## 2. 审计方法与标注体系

- **维度**：architecture / performance / algorithm / ui / size / quality / product，共 7 个。
- **复核方式**：审计各阶段以只读命令（rg / sed / cat 等）+ 逐条读码复核；发现与实施阶段均禁止编译；编译类验收由收尾校验阶段统一执行（§5）。
- **发现编号与账目**：本报告按维度自编号（A-架构 / P-性能 / AL-算法 / U-UI / S-体积依赖 / Q-质量 / PR-产品），共 34 条。给定数据的合并标注采用**原始审计的无连字符编号**（A1+A2、P2+P3、S4+S2+S5），与本报告编号（A-1 等）分属两套体系——"A1+A2"指原始审计的 2 条，合并为本报告 A-1，勿混淆。原始 40 条的完整对照清单不在给定数据内：7 条原始编号（2+2+3 条）并入 3 条合并发现后，按算术应余 36 条，与 34 相差 2-3 条，其去向数据未交代，本报告如实留缺口、不虚构补齐（§6.6 同）。
- **复核标注（是否复核列）**：34 条发现的**条目级**复核均为已读码复核（confirmed=true），无整条未经读码复核的发现。但条目级复核≠条目内每一证据均已复核——U-4 的全量 423 键逐语言覆盖率统计、PR-4 的迁移文件内容两处，为条目内**明示的采信**（"未在本轮复算/未逐条复核"），已在对应条目注明；这类条目内证据级采信与条目级 confirmed 并存，读者引用这两条的相应数字时应视为转引。
- **处置标签**：
  - **本轮实施**——已随本轮 5 项落盘并通过校验；
  - **留作建议**——达标或值得做，但不在本轮实施（附理由：名额排序 / 需独立一轮 / 观感验证 / 环境限制等）；
  - **需先测量**——收益存在但量化依赖实测，按 `GPUI_HARDCORE_PERFORMANCE.md` 纪律先测量后动手；
  - **已否决**——经论证不值得做（本轮仅 1 条：windows 双主版本主动降版）。
- **证据修正标注**：4 处原始证据的机制细节在复核中被修正（结论不变），条目内以"证据修正"字样标明旧值→新值。

**处置汇总**：34 条 = 本轮实施 5 / 留作建议 26 / 需先测量 2 / 已否决 1。按严重度：high 1 / medium 21 / low 12。按维度：架构 5、性能 3、算法 3、UI 5、体积依赖 5、质量 5、产品 8。

---

## 3. 七个维度详评

### 3.1 架构（architecture）

**概览**：Meliora 的核心栈质量很高：播放线程拆成 audio_engine/media_controller/device_controller/queue_manager 四个深模块，队列用 COW+undo+防抖快照单写者管理，media 解码层有真正的 trait 注册制（main.rs 一行接新解码器），scan 走干净的命令/事件接口。最大的结构性债务只有一个方向：在线音源层完全按"每个 provider 整套复制"的方式长在 UI 层里（156 处 cfg 门、四对平行复制的千行文件），并因此把 playback/stats/library/settings 四个核心模块反向拖进对 `crate::ui` 的依赖。面向"多音源、插件化、远程控制"的长远目标，最值得做的就是把在线 provider 收敛为与 `media::MediaProvider` 对等的 trait + 注册表，把流注册表状态从 UI 模块下沉到 online_sources 层。

| 编号 | 标题 | 位置 | 严重度 | 证据（要点） | 建议（要点） | 处置 | 是否复核 |
|---|---|---|---|---|---|---|---|
| A-1 | 在线音源层零抽象：provider 状态与逻辑长在 UI 层，playback/stats/settings/library 反向依赖 crate::ui（合并 A1+A2） | `src/ui/online.rs:35`; `src/playback/thread.rs:377`; `src/ui/kugou.rs:171`; `src/ui/netease.rs:140` | **high** | 非 UI 模块共 10 处直接引用 crate::ui（playback/queue.rs:14、thread.rs:377,403、interface.rs:355,369、stats/mod.rs:10,95,105、library/playlist.rs:11、settings.rs:247）；播放线程 refresh_expired_online_url 直接调 `crate::ui::online::online_track_matching_path` 与 `refresh_online_url`；kugou.rs:171 / netease.rs:140 各持一份平行 STREAM_MAP OnceLock，persist_stream_map_if_changed 两文件逐字节相同（kugou.rs:197-209 / netease.rs:179-191 已对照）；ui/online.rs:35 按 OnlineIdentity 枚举（playback/queue.rs:52-61，每源一变体）逐臂分派直呼 crate::ui::kugou/netease；cfg 门全仓 156 处（82+74）。对照媒体解码层注册制（media/traits.rs + main.rs 一行注册）成立 | 分两步：①流注册表、liked 缓存、refresh_online_url 下沉到 media 同级 online_sources 层（provider trait 默认实现），playback/stats 只依赖该层，ui 保留登录/设置页——此步消掉全部 crate::ui 反向依赖；②定义 OnlineSourceProvider trait（identity 解析/refresh_url/search/lyrics/like/playlists/ranks）+ 注册表，以 media::MediaProvider + lookup_table 为模板；视图层四对平行文件最后迁移 | 留作建议：大重构（预计 >30 文件），超 implement-now 门槛；路线图第一优先级，第一步（状态下沉）可先做 | ✔ 已读码复核 |
| A-2 | library ⇄ media 模块级循环依赖：token_key 是放错层的通用文本工具 | `src/media/lofty.rs:14`; `src/library/scan/artist_match.rs:4-11` | medium | media/lofty.rs:14 `use crate::library::scan::artist_match::token_key`，而 token_key（artist_match.rs:4-11）是纯字符串小写化/分词/排序函数（无任何库状态），被用于归一化 artist 字段；library→media（scan/decode.rs 用 try_open_media）+ media→library 成环 | 把 token_key 上移到 media（或独立 text 工具模块），library/scan/artist_match 反向引用；2-3 文件纯移动函数 | 留作建议：达标但本轮名额已满，**下批首选 refactor** | ✔ |
| A-3 | UI 线程 block_on 数据库查询固化在 LibraryAccess for App（30 处调用点） | `src/library/db.rs:938-1029` | medium | impl LibraryAccess for App 共 24 个方法，全部经 blocking_query（db.rs:1004-1029，`crate::RUNTIME.block_on`）在 UI 线程同步查库；代码注释自认 "those block_on's are debt"（db.rs:962-975 区段），挂有 UI_QUERY_SLOW_MICROS=2000 与 250ms 步进总量探针（982-1028）；UI 调用点抽查（如 playlist_view.rs:1102）证实；补偿缓存侧 models.rs:71-92 与 cached_album 冷未命中同步查（768-777）均已核对 | 统一为"后台 spawn + Entity 快照回填"单一惯例（ui/library/files_view/loader.rs 已是样板），LibraryAccess 收缩到低频路径；顺带把 Pool 从 crate::ui::app 移到 library 模块 | 留作建议：30+ 调用点系统性改造，需分批，放路线图 | ✔ |
| A-4 | 任何设置变更触发整窗 refresh_windows 全局扇出 | `src/ui/app.rs:486-487` | low | app.rs:486-487 `cx.observe(&settings_model, \|_, cx\| cx.refresh_windows()).detach()`；settings.rs:268-273 有同内容 save 回声守卫，但真实变更（哪怕只改在线音质）仍全窗重建；app.rs:328-336 注释表明 sidebar 宽度刻意避免过同样的整窗观察（右侧栏/歌词高度因内联渲染必须观察，属有意设计） | Settings 子结构拆独立 Entity 按分区订阅，或 diff 后按脏分区定向 refresh | 留作建议：低频路径，收益需与 Entity 拆分复杂度权衡 | ✔ |
| A-5 | Models 全局 28 字段状态袋：所有 UI 功能的默认交汇点 | `src/ui/models.rs:62-114` | low | 逐字段清点实数 **28 个字段**（证据修正：原始报告记 26）：播放镜像、库缓存（liked_ids/available_*/album_cache）、队列与扫描、导航、UI 布局态（sidebar_width/queue_width/show_lyrics/split_widths 等）、持久化镜像（table_settings/排序方法等），混杂判断成立 | 不做大手术；约束增量——新字段按域归组（UI 布局态/库缓存态/播放镜像态），冻结无序膨胀，写进工程约定 | 留作建议：纯演进约束，无立即动作 | ✔ |

**A-1 附注（报告撰写者读码核对，2026-09-26；命令与输出见 §5.2）**：给定数据对 A-1 的三个量化表述逐点核对如下——

1. **反向依赖清单**：`rg -n "crate::ui" src --glob "!src/ui/**"` 实测 **17 处**，多于给定清单的 10 处——给定清单未含 `src/main.rs` 的 7 处（:245,250,277-280 为 [mem] 探针读 `crate::ui::caching` / `managed_image` 的测量函数；:432 为 `crate::ui::app::run()` 启动入口）。main.rs 的引用性质（入口+探针）可解释审计为何未计入，但"rg 全量命中 10 处"的字面表述与实测不符，以实测为准。逐点归属（按给定建议的"第①步下沉流注册表/liked 缓存/refresh_online_url"口径）：
   - **第①步可直接消解（4 处）**：thread.rs:376（online_track_matching_path）、thread.rs:402（refresh_online_url）、stats/mod.rs:95 与 :105（online/netease_online_track_matching_path）；
   - **需后续步骤消解（其余 13 处）**：playback/queue.rs:14 与 library/playlist.rs:12 引用 `app::Pool`（A-3 建议"Pool 移到 library"）；stats/mod.rs:10 引用 `models::Queue`、settings.rs:247 引用 `models::{Models, SettingsHealth}`、playlist.rs:13 引用 `models::{Models, PlaylistEvent}`（UI 状态类型依赖）；interface.rs:355,369 调 `managed_image::drain_pending_tile_drops`（图集漏斗排水位于 UI 组件层）；main.rs 7 处（入口/探针）。
   - **结论**："第①步消掉全部 crate::ui 反向依赖"**高估了第①步覆盖面**（4/17）；"全部消掉"是第①步 + 第②步（trait 化）+ Pool/状态类型/漏斗排水下沉的组合终态。给定建议原文按其原样保留在上表，此处为逐点核对结果。
2. **"每加一个音源需在 16 个文件复制 4-5k 行"**：给定数据断言，未附 16 个文件的清单与行数推导，本轮未复核，报告不重建（避免虚构）。
3. **"四对平行复制的千行文件"**：给定数据未列举是哪四对。目录结构可证实的平行对（实测 `ls` + `wc -l`）：UI 层 `src/ui/kugou.rs`(1452 行)↔`src/ui/netease.rs`(1206 行)、`src/ui/kugou/download.rs`(316)↔`src/ui/netease/download.rs`(326)；provider 层 `src/kugou/api.rs`(681)↔`src/netease/api.rs`(349)、`src/kugou/client.rs`(609)↔`src/netease/client.rs`(688)、`src/kugou/crypto.rs`(111)↔`src/netease/crypto.rs`(198)、mod.rs 对。"千行"按实测仅 ui/kugou.rs↔netease.rs 一对成立；"四对"的原始口径无从重建，以本清单为准。



### 3.2 性能（performance）

**概览**：热路径纪律执行水平很高，教义条款大多有实现+测量双重落地——音频回调仅 ring 读取+原子量+增益 ramp（零分配零锁），EQ 流式路径无分配且有稳态零分配的 alloc_guard 测试常驻把关，渲染侧 uniform_list/uniform_grid 虚拟化、行视图缓存+异步预取、右键菜单惰性构建、歌词虚拟化+可见性门控全部到位；单一图集回收漏斗+60s 年龄门经读码复核无结构性悬空窗口，[mem] 探针+[db] UI 线程查询计数构成常驻测量网。本机日志（937 行，只读查看）显示：tiles_leaked=0、funnel_pending=0，在所引窗口（2970s→3870s，private 481→637MB）内无泄漏计数非零的证据；"会话期爬升部分回落"为给定数据的定性描述，窗口内数字本身只有上涨，全称"无泄漏"超出所引窗口的证据范围（证据边界见 §1.2.1）。最大的两个机会：①艺人详情页 auto_height 专辑网格是全仓唯一无虚拟化的大集合渲染点；②[mem] 探针缺帧时间与"活动瓦片数"两项归因。Cargo.toml [profile] 既有决策有 bench 注释支撑，未发现推翻依据，不动。

| 编号 | 标题 | 位置 | 严重度 | 证据（要点） | 建议（要点） | 处置 | 是否复核 |
|---|---|---|---|---|---|---|---|
| P-1 | 艺人详情页专辑网格 auto_height 全量渲染，>129 项时每次重渲产生数百实体建剪循环 | `src/ui/components/uniform_grid.rs:284-308`; `src/ui/library/artist_detail_view.rs:767-832`; `src/ui/util.rs:16-57` | medium | auto_height 分支 `for row in 0..metrics.row_count` 无视口裁剪构建全部条目（ContentMask 只裁 paint），render_item 逐项调 prune_views（VIEW_KEEP_AROUND=128）。**证据修正**（原报告窗口算术有误，实际更直接）：prune_views 的 lower = last.min(current)-128（saturating），首帧 current=0 时窗口实为 [0, N+128) 全保留；current=1 起上界收缩至 current+129，idx≥129 的视图在本帧 idx=1 调用中即被剪掉、随后逐个 miss 重建（GridItem::new + 插缓存），下一帧又被剪再重建——稳态每帧 churn 约 N-129 个实体。auto_height 全仓唯一使用点 artist_detail_view.rs:832（实际路径为 src/ui/components/uniform_grid.rs，非审计所写 components/table/ 下） | 最小改法：auto_height 分支跳过 prune_views（全部实体进缓存，艺人专辑数通常几十）；或改走可滚动虚拟网格 | 留作建议：达标但触发面窄（需艺人专辑数>129），名额已满，**下批首选** | ✔ |
| P-2 | [mem] 探针缺帧时间与活动瓦片数归因，covers 求和每 30s 全量 readdir（合并 P2+P3） | `src/main.rs:263-381` | low | periodic 采样 private/working/heap_commit/non_heap/render_cache/img_cache/covers/回收漏斗五计数（main.rs:332-352），step 告警附 mimalloc dump（318-329）；`set_trace_enabled(false)` 在 main.rs:405（帧时间无测量：对 src/ 以 frame_time、frametime 两模式交替检索零命中，报告撰写者复跑证实，命令见 §5.2）；disk_cover_cache_mb（370-381）每 30s tick 对 image-cache 目录 read_dir + 逐文件 metadata 求和（tokio-rt-worker 线程，非 UI） | ①ManagedImage::paint 成功路径按窗口去重计数，随 [mem] 输出活动瓦片数；②帧时间按教义按需 samply/ETW，不给 uniform_list 加常驻打点；③covers 求和降频到每 10 tick 或启动扫一次+增量累计 | 留作建议：测量工具扩展，价值在下次排查时兑现，不紧急 | ✔ |
| P-3 | 续体直接回收绕过 60s 年龄门：行为无缺陷，安全性论证未固化在代码里 | `src/ui/components/managed_image.rs:869-873`; `src/ui/util.rs:92-119` | low | 任务续体在 `this.update(cx,...).is_err()`（元素已释放）且 `Arc::strong_count(&image)==1` 时直接调 reclaim_images_from_app（managed_image.rs:866-875），绕过 RECLAIM_DELAY（同文件 248-269 有完整存在理由注释，引用 2026-09-16 漏斗审计与 2026-09-08 崩溃类）；reclaim_images_from_app 内有 catch_unwind 兜底（util.rs:100-119）；依赖的两个前提（strong_count==1 ⇒ 缓存未持有；unmount 必由重渲染触发 ⇒ 重放 sprite 已不在场景）无注释固化 | 不改行为，在续体分支补注释固化两前提论证，并同步 patches/UPSTREAM_NOTES.md 或漏斗审计记录；纯文档动作，可并入任意后续提交 | 留作建议：纯注释补全，不构成独立实施项 | ✔ |

### 3.3 算法（algorithm）

**概览**：整体算法/数据结构质量非常高：扫描管线（mtime 指纹 + 大小写折叠索引 + 2s checkpoint 门控）、队列（Fisher-Yates + undo 栈 + 持久化防抖）、歌词（partition_point 二分对齐 + 虚拟化列表 + 16 条 FIFO 缓存）、realfft 频谱（预分配 + 观众门控 + 空闲 1s 驻留）、封面缓存（LRU 项数上界 + 磁盘 256MB 清扫 + 128px 降采样）与会话存储（watch + Arc + 后台序列化 + 原子改名）均无可动之处，且大量决策附带实测数据（record.rs:214-243 checkpoint bench、db.rs:984-997 [db] 探针）。索引设计也在持续迭代（20260912/20260916 迁移各有逐条服务于哪个查询的注释）。最大的剩余机会集中在搜索注入路径与一个艺术家计数 SQL 的连接扇出；二者修复风险都很低——**两条均已在本轮实施**。

| 编号 | 标题 | 位置 | 严重度 | 证据（要点） | 建议（要点） | 处置 | 是否复核 |
|---|---|---|---|---|---|---|---|
| AL-1 | 在线搜索结果每次更新都在 UI 线程全量 restart + 重注入 nucleo 索引（含每条 format!） | `src/ui/components/palette/finder.rs:444-478`; `src/ui/search/model.rs:108-127,241-248,322-329,356-364` | medium | set_items 用 Arc::ptr_eq 公共前缀判 append-only（`append_only = common == self.injected.len()`）；kugou_items 更新后 merged=[本地N+新在线K]、旧 injected=[本地N+旧在线K']，common=N 而 len=N+K' → append_only=false → `matcher.restart(false)` 后对全部 N+K 条逐条 `(get_item_display)(&item, cx)` + injector.push，全在 UI 线程订阅回调内；matcher 闭包（model.rs:108-127）对 Album/Track/KugouTrack/NeteaseTrack 每条做 format! 分配；退格 <2 字符清空在线结果（322-329）与扫描完成刷新（210-233，已自带面板关闭守卫）走同一全量重建路径 | ①后台构造 SearchPaletteItem 时预计算搜索文本存 Arc<str>，matcher 闭包只 clone；②在线结果（≤15+15 条）移出 nucleo——get_matches 时本地走 nucleo snapshot，在线段用 nucleo Pattern + Matcher 对预计算文本线性匹配后合并（零新增依赖），本地索引从此永久 append-only | **本轮实施**（实施明细见 §4.2；打字路径 O(全库×format!) → O(在线数≤30)） | ✔ |
| AL-2 | find_artists_tracks_asc/desc 双腿 JOIN 笛卡尔积，中间行数是乘法而非加法 | `queries/library/find_artists_tracks_asc.sql:1-6`; `queries/library/find_artists_tracks_desc.sql:1-6` | medium | `LEFT JOIN album_artist aa → LEFT JOIN track t ON t.album_id = aa.album_id` 与 `LEFT JOIN track_artist ta` 两条相互独立的 leg 直接 JOIN 成积，仅靠 GROUP BY a.id 的 COUNT(DISTINCT) 收敛；中间行数 = Σ_艺术家(专辑内曲目数 × track_artist 条目数)；对照同库 find_artist_with_counts_by_id.sql（单艺术家版）已正确采用两个独立标量子查询相加的线性形态，证明非刻意写法 | 改写为两个 GROUP BY 子查询（口径与现 SQL 的 COUNT(DISTINCT) 完全一致）LEFT JOIN 回 artist 后相加排序，desc 版仅 ORDER BY 方向不同；最小数据集验证后落盘 | **本轮实施**（明细见 §4.1；O(乘法)→线性） | ✔ |
| AL-3 | Track 行预取不预热专辑元数据：冷缓存滚动每个新专辑 UI 线程 block_on 一次，album_cache 无上界 | `src/library/types/table.rs:723-752`; `src/ui/models.rs:92,209,768-777,381-382` | medium | Track::prefetch_rows（table.rs:723-752，实际路径为 src/library/types/table.rs，非审计所写 ui/library/types/）只批量取 track 行进 track_row_cache；行渲染 Album/Artist 列经 cached_album（models.rs:768-777，注释自述冷未命中走一次 blocking query），即 LibraryAccess 的 UI 线程 block_on（db.rs:1004-1029）；Models.album_cache 是无淘汰 FxHashMap<i64, Arc<Album>>（models.rs:92,209），仅扫描完成整体清空（381-382）；同文件 Album::prefetch_rows（280-302）已有"后台预取+generation 失效"现成样板 | Track::prefetch_rows 收集窗口内 album_id，对 album_cache 缺失专辑追加一次后台 IN 查询回填（参照 generation 失效写法）；给 album_cache 套 RowCache 同款 FIFO 上限（对齐 table.rs:78 先例） | **需先测量**：冷滚动一次的 [db] 曲线（改前基线已有常驻探针）；后台回填涉及 Entity 跨线程更新细节，实测确认收益后再做 | ✔ |

### 3.4 UI

**概览**：整体架构质量高：表格/队列/命令面板/歌词列表均已虚拟化且带实体缓存+窗口裁剪，图片解码全部走 spawn_blocking+信号量+LRU，position 在播放线程 33/250ms 合并广播、UI 侧观察者粒度细分（Scrubber/ScanStatus 独立实体），键盘流有 !TextInput 守卫与 TextInput 内嵌拦截，专项抽查未发现 UI 线程同步 IO。最大的问题集中在歌词面板：交互后 2 秒空转帧循环（**已在本轮修复**）、queue_width 失效粒度缺口、居中目标在 emphasis 动画起点定格；另有 i18n 可选语言覆盖率低与设计常量散落两处一致性债。

| 编号 | 标题 | 位置 | 严重度 | 证据（要点） | 建议（要点） | 处置 | 是否复核 |
|---|---|---|---|---|---|---|---|
| U-1 | 歌词面板交互后 2 秒内空转帧循环：无任何动画变化仍逐帧整窗重绘 | `src/ui/lyrics.rs:1028-1033,835-837,827-831,727-757` | medium | needs_animation_frame()（lyrics.rs:1028-1033）把 has_recent_user_interaction()（2s 超时，常量在 :40）计入"需要帧"；advance_animations 每帧结尾无条件续帧（835-837），而交互窗口内 scroll_follow 被 cancel（827-831）、emphasis 已结束、changed=false 无 notify——纯交互门让 cx.on_next_frame 持续自我续帧；触发点已核对（滚轮/点击 727-738、滚动条 on_interaction 无条件 750-757） | 把 has_recent_user_interaction() 从 needs_animation_frame() 移除，其余逻辑保留；闭环论证：交互期间帧由滚轮/点击事件本身驱动，交互结束后由 position tick（33/250ms 广播，Lyrics 已 observe position）驱动的下一帧恢复 follow；**暂停态消费路径的补论证见 §4.3 附注** | **本轮实施**（明细见 §4.3；实施形态有偏离：负条件而非纯删除，前提经读码证伪后修正） | ✔ |
| U-2 | Lyrics 读 queue_width 却未 observe，且被 AnyView::cached 隔离：拖队列分隔条后歌词行宽滞后 | `src/ui/lyrics.rs:545,652`; `src/ui/right_sidebar.rs:58-59`; `src/ui/app.rs:333-336` | medium | render 里读 queue_width（lyrics.rs:545）用于每行宽度换算（:652）；Lyrics::new 仅 4 个 observe（current_track/position/playback_state/scan_state），无 queue_width；父级 app.rs:333-336 observe → 根视图 notify 被 `AnyView::from(...).cached(...)`（right_sidebar.rs:58-59）隔离；gpui-ce 源 view.rs:475 证实 cached 视图未重渲时走 window.reuse_paint 重放，不重跑 render | Lyrics::new 补 observe queue_width 且仅值变化时 notify（1 行观察 + diff 守卫）；或行宽改布局表达删掉全局读；顺带审视"行宽∝队列面板宽"这一初版继承的设计意图 | 留作建议：低风险小修复，名额已满；注意 notify 只在宽度真变时发，避免拖动期间逐帧重渲歌词 | ✔ |
| U-3 | 歌词居中目标在 emphasis 动画起点定格，激活行膨胀后停驻偏离中心数像素 | `src/ui/lyrics.rs:845-871,39-46,665-675,930` | low | advance_follow_animation 在 follow_pending 时 compute_follow_target() 一次算出固定 target 后 animate_to，随后每帧只 scroll_follow.advance()（845-871）；emphasis 动画（180ms，:39）同步插值激活行 py 7→9px、字号 22→25px、行高 1.5→1.65rem（常量 :39-46 与插值 :665-675），行中心漂移约 6px，但 target 不再重算（follow_pending 已置 false，0.1px 死区 :930 不再触发），直到下一行切换自愈 | follow 动画进行中每帧用最新 bounds 重算 target（animate_to 重锚定起点），或 emphasis 结束帧对当前行再 snap 一次居中；moderate 风险：重锚定可能引入抖动 | 留作建议：视觉行为改动需人工验证观感，不宜盲改 | ✔ |
| U-4 | i18n：代码侧词条完备，但设置页 10 种可选语言中 8 种覆盖率仅 30-57%，zh-CN 仅缺 3 键 | `translations/zh-CN.json`; `src/ui/settings/interface.rs:48-97` | medium | zh-CN.json 中查 EXPORT_PLAYLIST_FAILED / QUEUE_REMOVE_CANCELLED_STALE / STATS_TOP_TITLE 三键均无命中（缺失），三键代码引用点确认存在（queue.rs:808、library/sidebar/playlists.rs:672、settings/stats.rs:908）；语言选择器列出 cs/de/el/en/es/fi/ja/sk/vi/zh-CN（interface.rs:48-97）；**全量 423 键逐语言对账未在本轮复算**，采信原始审计的 rg+python 统计（cs=198、es=182、el=128、ja=193、fi=193、sk=198、vi=223、de=241；hr/sr/pt-BR 大量空串回退英文） | 先补齐 zh-CN 缺的 3 键（1 文件 3 行；translations/meta.json 会随构建重写属正常 diff）；再按使用频率为 es/ja 等主推语言补词条，或对覆盖率 <60% 的语言在选择器中标注 | 留作建议：3 键改动极小但名额让给收益更大的项，可随任意后续提交顺带 | ✔ |
| U-5 | 视觉一致性：design.rs 明文规范字号档位，但全 ui 仍有 40 处 raw text_size(px)；拖拽高亮色重复硬编码 9 处；controls.rs 时长文本用非主题色 | `src/ui/settings/stats.rs:579`; `src/ui/queue.rs:444`; `src/ui/controls.rs:1246-1249`; `src/ui/design.rs:22-25` | medium | design.rs:22-25 规则原文明确"only GPUI's text_xs()/…/text_xl() … a raw text_size(px(…)) does not [follow rem]"；src/ui/ 下 text_size(px( 出现 **40 处**（stats.rs 19 处；计数命令见 §5.2，报告撰写者复跑证实）；rgba(0x88888822) 实数 **9 处**（证据修正：原报告记 7 处——9 处位置经报告撰写者复跑逐一证实，见 §5.2：queue.rs:444、library/playlist_view.rs:294、library/sidebar/playlists.rs:438/440/441、components/table/table_item.rs:164/171、components/table/grid_item.rs:143/151）；controls.rs:1246/1249 时长文本用 rgb(0x4b5563)/rgb(0xcbd5e1) 硬编码而非 theme 字段 | 40 处 text_size(px) 归一到 text_* 档位（先从 stats.rs 19 处开刀）；拖拽高亮色提为 design 常量（如 DRAG_OVER_BG）供 9 处引用；controls.rs 两色改读 theme 字段；归一后视觉尺寸会有 ±1-2px 变化，需过一眼各页观感 | 留作建议：改动面 11+ 文件超门槛；纯视觉风格，需观感验证 | ✔ |

### 3.5 体积与依赖（size）

**概览**：依赖面已相当克制：66 个直接依赖逐一做了 `rg '\b<crate>::'` 调用点普查与 cargo tree 统一化核查，未发现"可整删而不动代码"的死依赖；itertools 最终以"一处活调用（thread.rs:1063 `Itertools::format`，实施时发现）替换为 std 等价后删除"的方式移除（§4.4），并非审计初判的纯死依赖；palette/derive_more/unicode-segmentation/same-file/tokio-stream/audioadapter-buffers/flate2 等「低调用点」依赖全部经版本/feature 统一与 gpui/sqlx/rubato/lofty 共享编译产物（cargo tree -i 证实零边际成本），lofty 0.25 的 features 仅剩 id3v2_compression_support（格式支持非 feature 化）、sqlx 'macros' 因 6 处 FromRow/Type derive 必须保留、symphonia feature 集即功能面——现有裁剪之外没有第二个「可删直接依赖」。真正剩下的杠杆是 cntp_i18n 栈的 icu 伞依赖（17 个组件 + CLDR 数据、build-dep 双份编译）与 windows crate 双主版本各编一遍。工程侧 windows features 清单已在本轮纠偏（详见 S-2），CI 存在工具链浮动与版本号无 sha 追溯的小缺口。注意：审计发现阶段被禁止 build/check/test，所有 feature 增删与二进制收益数字需构建实测验证（本轮收尾已跑 check/build，体积收益未测）。

| 编号 | 标题 | 位置 | 严重度 | 证据（要点） | 建议（要点） | 处置 | 是否复核 |
|---|---|---|---|---|---|---|---|
| S-1 | cntp_i18n 栈的 icu 伞依赖把 17 个 ICU4X 组件与 CLDR 数据拉进编译图，是剩余最大编译/体积杠杆 | `Cargo.toml:49`; `~/.cargo/registry cntp_i18n_core-0.3.0` | medium | Cargo.toml:49 `cntp_i18n = { version = "0.3", features = ["gpui"] }`；registry 源码 cntp_i18n_core-0.3.0/Cargo.toml:55 `[dependencies.icu]`（伞依赖），其 src 实际仅用 icu::plurals（lib.rs:73）与 icu::locale（lib.rs:195）；target/release/deps 中 libicu_experimental rlib 82-92MB ×3、libicu_experimental_data 61MB+；icu 以普通依赖（带 compiled_data）与 build-dependency（cntp_i18n_gen，Cargo.toml:165，不带 compiled_data）双份编译 | 向 cntp_i18n 上游提 PR 把伞依赖换成 granular 组件（icu_locale + icu_plurals + icu_decimal/icu_time/icu_experimental with compiled_data）；或临时 [patch.crates-io] 维护 fork（仓库已有 vendor patch 先例）；按教义第 22 条改动前后必须实测 exe 体积与构建时间 | **需先测量**：收益全在编译时间与二进制体积，先实测基线（当前 exe 49MB、全量构建时长）再决定是否付 fork 维护成本；上游 PR 周期不受本仓控制 | ✔ |
| S-2 | 依赖清单纠偏：itertools 可删；windows features 零引用可删、代码在用项需显式声明；sysinfo 未收窄 default features（合并 S4+S2+S5） | `Cargo.toml:64,107,171-189`; `src/playback/thread.rs:12`; `src/power.rs:12-25` | medium | ①审计时点 `rg Itertools src/` 仅命中 thread.rs:12 匿名导入（豁免 unused_imports lint），itertools 特有方法 grep 零命中（**实施修正**：thread.rs:1063 的 `.format(":")` 实为 Itertools::format 活调用，先 std join 替换后删，见 §4.4；报告撰写者现状复跑 src/ 下 itertools/Itertools 已零命中——删除完成后的现状，命令见 §5.2）；②windows features（Cargo.toml:171-189）中 Devices_Enumeration/Media_Audio/Media_MediaProperties/Media_Render 四项 src 全量 rg 零类型引用（Media 本体在 controllers/windows.rs:8 在用需保留），power.rs:12-25 实际导入 Win32::System::{Threading, SystemServices}（POWER_REQUEST_CONTEXT_VERSION/REASON_CONTEXT）而 Cargo.toml 未声明，cargo tree -e features 证实此前由 cpal v0.18.2 透传启用（上游换版本即 E0432）；③sysinfo = "0.39"（Cargo.toml:107）未收窄，实际调用点仅 3 处：scan/disk.rs:8（Disks）、troubleshooting.rs:2 与 main.rs:166-169 非 Windows 进程内存，component/network/user 子系统零使用 | 一次提交：删 itertools 两行；windows 删零引用 feature、补 Win32_System_Threading 与 Win32_System_SystemServices；sysinfo 改 `default-features = false, features = ["system", "disk"]`；逐项删补后 cargo check 全绿，任何 E0432/E0433 即回退对应单项 | **本轮实施**（明细见 §4.4；Foundation_Collections 与 Storage_Search 经生成代码逐方法 cfg 证伪保留，实删 4 项补 2 项） | ✔ |
| S-3 | windows crate 0.62（meliora）与 0.61（gpui-ce pin）双主版本各编一遍 | `Cargo.toml:171`; `Cargo.toml:60` | medium | cargo tree -i windows@0.61.3 显示 0.61.3 经 gpui-ce rev ae7c411 引入，meliora 直依 0.62；deps 目录现存多份 libwindows rlib（147-155MB 级 ×3 hash） | 短期不动（gpui-ce 升级到 windows 0.62 后自然统一）；主动降到 0.61 需 API 适配与实测，性价比低 | **已否决**：原始报告自身结论即"短期不动、不推荐主动做"；主动降版属高风险无收益动作 | ✔ |
| S-4 | build.rs 三处健壮性细节：rerun 清单未覆盖 translations/、.env 双读冗余、package/RELEASE_CHANNEL 残留回退 | `build.rs:59-66,80-88` | low | ①rerun-if-changed 清单仅 .git/logs/HEAD(:14)、.env(:60)、package/RELEASE_CHANNEL(:84)，而 cntp_i18n_gen-0.3.0/src/lib.rs:684 确为 `println!("cargo::rerun-if-changed=src")`（registry 源码已核）——translations/ 不在任何清单，只改翻译 JSON 不动 src 时再生成滞后到下次 src 变更；②:61-65 对 .env 先 dotenvy::from_read 再 from_read_iter 读两遍；③:82-88 在 MELIORA_RELEASE_CHANNEL 未设时仍回退读 package/RELEASE_CHANNEL，与 AGENTS.md"不再读 package/"表述矛盾（目录已删，读到 Err 后默认 stable，行为正确但代码过时） | 补一行 `println!("cargo:rerun-if-changed=translations")`；.env 合并为单次 from_read_iter；删除 package/ 回退分支只留 env→默认 stable；不要动 option_env 的 rerun-if-env-changed 语义 | 留作建议：工程化小修，价值实但优先级最低档；1 文件可随任意后续提交顺带 | ✔ |
| S-5 | release.yml 四个流程缺口：工具链浮动、发布版本号无 git sha 追溯、产物无校验和、release-distro 死配置 | `.github/workflows/release.yml:43-50,70-75`; `Cargo.toml:233` | low | release.yml:43-50 删 rust-toolchain.toml 后用 dtolnay/rust-toolchain@stable 浮动 stable；Build release 步 env 仅 CFLAGS_aarch64_pc_windows_msvc（70-75），无 MELIORA_RELEASE_CHANNEL/MELIORA_VERSION_ID——结合 build.rs:92-99 的 stable 分支逻辑（id 回退 "release"），产物版本号无 sha 追溯；release job 上传 zip/tar.gz 无 sha256 清单；[profile.release-distro]（Cargo.toml:233）全仓 rg 仅此一处、release.yml 无引用，确为死配置或未接线 | 矩阵钉 Rust minor 版本并定期手动升级；build 步注入 MELIORA_VERSION_ID: ${{ github.sha }}；release job 生成 sha256sums.txt；明确 release-distro 去留；job 加 timeout-minutes 防交叉编译悬挂 | 留作建议：CI 文件本机无法验证，且不阻塞任何功能；放入工程化待办（ROADMAP §5） | ✔ |

### 3.6 质量（quality）

**概览**：总体健壮性水平高：cpal 音频回调 realtime-safe（错误回调用 try_lock 不阻塞、无分配无 IO，src/devices/cpal.rs:217-234）；queue_manager 等所有非测试锁统一 `unwrap_or_else(|e| e.into_inner())` 中毒恢复（src/playback/thread/queue_manager.rs:288 等 25+ 处）；扫描写入有大事务边界且 begin 失败降级计数而非 panic（src/library/scan/execution.rs:127-142）；playback_session.json 用 tmp+rename 原子落盘（src/playback/session_storage.rs:66-104）；DB 迁移对 Windows 行尾导致的 VersionMismatch 有修复重试（src/library/db.rs:39-77）；unsafe 全部为平台 FFI（mimalloc/Win32/objc2）且错误路径完备。点名的重灾区逐一核查后大多受局部不变量守护：symphonia.rs:543/599 的 unwrap 紧跟 `find(is_some)` 过滤、resample.rs:288/356 的 rubato adapter 由 `input_available()` 取 min 保证各通道等长、queue.rs:323/430 由 ensure_entity 双检锁保证 Some、crypto.rs 的 expect 均为硬编码常量、歌词解析器全 Option 化、artist_match.rs:152/164 在 load() 后成立。真正值得修的是：播放线程死亡后的静默失效链（通道吞错），和在线账号凭据文件的非原子落盘——**后者已在本轮修复**。

| 编号 | 标题 | 位置 | 严重度 | 证据（要点） | 建议（要点） | 处置 | 是否复核 |
|---|---|---|---|---|---|---|---|
| Q-1 | 播放线程一旦 panic 退出，UI 全部播放命令被静默吞掉且无任何日志 | `src/playback/interface.rs:58-160`; `src/playback/thread.rs:188` | medium | thread.rs:188 播放线程主循环 `loop { self.main_loop(); }`（detached spawn，仅 panic 才退出）；UI→播放命令走 tokio UnboundedSender，interface.rs 中 `let _ = self.cmd_tx.send(...)` 实数 **21 处**（证据修正：原报告记 25）——UnboundedSender::send 仅在接收端 drop 时返回 Err，线程 panic 死亡后全部吞掉 SendError 零日志；事件侧 interface.rs:185-192 收到 recv None 仅退出循环 | interface.rs 加私有 helper `fn send_cmd(&self, cmd)`：send 返回 Err 时 warn!（限频，避免刷屏）记录"播放子系统已离线"，21 处调用点替换为 helper；可选经事件通道向 UI 置一次离线提示；改动集中 1 文件，行为仅增加日志 | 留作建议：改动小价值实（可观测性），但属防御性日志而非当前缺陷；**下批候选** | ✔ |
| Q-2 | 酷狗/网易账号凭据文件用 std::fs::write 原地覆盖写，非原子；读取 IO 失败无日志 | `src/kugou/client.rs:112-136`; `src/netease/client.rs:101-125,196-198` | medium | kugou save（client.rs:121-133）与 netease save（client.rs:108-122，save_session:196）均以 `std::fs::write(path, json)` 原地覆盖会话 JSON；而项目自己的 playback/session_storage.rs:70-104 对 playback_session.json 用 tmp+rename 原子替换并注释了"truncate+rewrite 会在进程中途死亡时留下截断文件"（样板已核）；load（kugou:114-118 / netease:101-105）`std::fs::read_to_string(path).ok()?` 把读取 IO 错误静默转"未登录"（serde 解码失败反而有 warn） | 两处 save 改用与 session_storage.rs 相同的 tmp+rename 模式（write 失败清理 tmp、rename 失败记日志清理）；load 的 read 失败补日志——NotFound 是首次使用无凭据的正常路径应 debug!/静默，其他错误才 warn | **本轮实施**（明细见 §4.5） | ✔ |
| Q-3 | 命令面板加载内嵌 commands.json 时，未注册 action 直接 panic 使启动崩溃 | `src/ui/command_palette.rs:273,277` | low | :277 `cx.build_action(&e.action, None).unwrap_or_else(\|err\| panic!("unknown action {}: {err}", e.action))`——commands.json 是 include_str 编译期嵌入，action 改名/删项与 JSON 未同步时启动即 panic；:273 JSON 解析失败也是 expect | build_action 失败改记录 error! 并跳过该条目（收集 built actions 而非 panic）；或在构建期/测试中校验 commands.json 的 action 全部可解析；expect 的 JSON 解析可保留（那是硬损坏，理应 fail fast） | 留作建议：低风险小修，触发条件是重构期不同步；**下批候选** | ✔ |
| Q-4 | Linux MPRIS 属性查询路径 13 处 .await.unwrap() | `src/controllers/mpris.rs:407-517` | low | `.await.unwrap()` 计数 **13 处**（metadata_int/loop_status_int/playback_status_int/can_pause_int/can_play_int/can_seek_int 等；报告撰写者复跑 `rg -c "\.await\.unwrap\(\)"` = 13，命令见 §5.2），全部在 cfg(target_os="linux") 代码内，主平台 Windows 不受影响 | map_err 成默认属性值（空 metadata、CanPause=true 等）+ debug! 日志；本机 Windows 无法运行验证，改动需 Linux 环境回归 | 留作建议：非主平台且本机无法验证，留作 Linux 用户反馈时的首批修复 | ✔ |
| Q-5 | Kugou VIP 升级奖励失败被 `let _ =` 吞掉，outcome 仍报 Claimed | `src/kugou/api.rs:650-676` | low | :651/:671/:674 区段 `let _ = self.refresh_vip_detail().await;`（best-effort 合理）与 `let _ = self.upgrade_vip_reward().await;`（:664/:673）——upgrade 是"基础 tvip → 正式奖励"的实际升级步骤，其 Err 无日志也不影响返回值，随后无条件 VipClaimOutcome::Claimed | 至少对 upgrade_vip_reward 的 Err 记 warn!（1 行）；或失败并入 outcome（如 ClaimedWithUpgradeFailed）供 UI 提示；注意与近期 kugou 领取守卫提交（bc53afa060/b4ae5ccba6）的"今日生效才跳过"逻辑保持一致，勿误改守卫语义 | 留作建议：1 行日志改动，可随 kugou 相关后续提交顺带 | ✔ |

### 3.7 产品（product）

**概览**：Meliora 的底层工程已达到一流播放器水准：gapless 预开轨（playback/thread/audio_engine.rs:269）、SMTC 全量控制（含 seek/shuffle/repeat/封面）、最多 16 段参数 EQ + 实时频谱、KRC/YRC 逐字卡拉OK歌词、带防作弊钳制的听歌统计（stats/recorder.rs）。但"最后一公里"的用户价值暴露不足——键盘操作、预设、空态引导、库内智能视图这类低成本高感知的缺口明显，对标 foobar2000/MusicBee 的差距主要在曲库利用（最近添加/流派/播放次数视图、标签编辑、可配置列），对标 YesPlayMusic/Listen1 的差距主要在本地歌与在线服务的联动（本地歌补歌词/封面）。审计发现阶段为只读（rg/sed/cat 等只读命令），未运行任何编译或测试。

| 编号 | 标题 | 位置 | 严重度 | 证据（要点） | 建议（要点） | 处置 | 是否复核 |
|---|---|---|---|---|---|---|---|
| PR-1 | 快捷键体系缺音量/进度/面板开关等核心动作，且键位不可自定义 | `src/ui/global_actions.rs:25-26`; `assets/keybinds.json`; `src/ui/keymap.rs:4,55` | medium | global_actions.rs 仅注册 player::{PlayPause, Next, Previous, ShuffleAll, StopAfterCurrent}；keybinds.json 以 volume、seek、lyrics、queue 四词交替检索仅命中 queue::Undo 一行（报告撰写者复跑证实，命令见 §5.2），无任何音量/seek/面板开关绑定；keymap.rs:4 仅 include_str! 内嵌默认表，load_default_keymap（:55）无用户键位文件加载；playback_interface 的 set_volume 等接口已存在 | 第一期：注册 VolumeUp/VolumeDown/SeekForward/SeekBackward/ToggleQueue/ToggleLyrics/Mute 等 action（调既有接口）+ keybinds.json 默认键位 + commands.json 收录使命令面板可发现；第二期：load_default_keymap 扩为"默认表 + data 目录用户 keymap.json 合并"（解析器现成） | 留作建议：高价值功能但涉及 action 注册/分派/UI 提示多处，需独立一轮；排在 5 项之后 | ✔ |
| PR-2 | EQ 完全没有预设，也没有导入导出，空曲线起步劝退普通用户 | `src/settings/equalizer.rs:62-69`; `src/ui/equalizer/view.rs` | medium | EqualizerSettings 仅 enabled/volume_compensation/bands 三字段（:62-69），Default 派生 bands 为空；rg -ni preset 全 src 仅命中 netease/crypto.rs 的加密常量，UI 层无任何预设/导入导出入口 | 内置 8-10 个经典预设（Flat/Bass/Vocal/Rock/Electronic/Classical 等频段模板）+ 保存为我的预设 + JSON 导入导出（复用 OpenThemeFolder 模式）；组件库已有 dropdown.rs | 留作建议：功能规划项，需独立设计频段模板与 UI 入口 | ✔ |
| PR-3 | 首次启动零引导：默认目录扫空后用户面对空表无任何行动指引 | `src/ui/app.rs:496-514`; `src/ui/settings/library.rs:289`; `src/ui/components/table.rs` | medium | 启动延迟 1s 自动扫描（app.rs:502-514，有 SettingsHealth 守卫，损坏时跳过并 warn）；table.rs 无空态提示（对 items.is_empty、NO_ITEMS、empty_message 三模式交替检索零命中，报告撰写者复跑证实，命令见 §5.2）；侧栏分区仅 Albums/Artists/Tracks 等（sidebar.rs:264-300 起已核），无添加目录入口 | 曲目表空态改为引导页：扫描中显示进度（复用 header.rs 的 ScanProgress），完成后仍为 0 则显示"添加音乐文件夹"按钮直接深链设置页 Library 分区（open_settings_window_with_section 已支持） | 留作建议：功能规划项，需 UI 设计与文案 | ✔ |
| PR-4 | 统计与曲库脱节：有 created_at/genres/play_stats 数据资产，却无"最近添加/流派/最多播放"任何入口 | `src/library/types/table.rs:462-468`; `src/ui/library/sidebar.rs:264-323` | medium | TrackColumn 枚举仅 TrackNumber/Title/Album/Artist/Length（:462-468），无流派/添加日期列；侧栏分区无 Recently Added/Most Played；created_at 列与 listen_event 表存在（迁移文件，原审计引用未逐条复核迁移内容） | 第一期：侧栏加 Recently Added（created_at 倒序复用 TrackView）+ 流派/添加日期可显示表格列；第二期：Most Played 智能歌单（listen_event 聚合 + 时间窗筛选，需先测量聚合查询在大库的表现） | 留作建议：第一期实现路径清晰但涉及新查询与侧栏改动，独立一轮 | ✔ |
| PR-5 | 本地歌曲无在线歌词/封面补全，在线歌词能力只服务在线流 | `src/ui/lyrics.rs:126-168,320-358` | medium | 歌词加载双路——在线流按 provider 注册表逐个查询（lyrics.rs:126-168，仅 is_online_path 分支内 dispatch），本地曲目走 load_lyrics_off_thread（:320-358）只查 sidecar .krc/.yrc + DB（get_track_by_path→lyrics_for_track），无按标题/艺人在线搜索兜底；而 kugou/download.rs:120 fetch_lyrics 证明取词 API 链路现成 | 歌词面板空态加"在线搜索歌词"：标题+艺人调酷狗/网易搜索接口，命中列表用户选择后写入 sidecar 或 lyrics 表；封面补全二期；注意写回涉及 feature 门控与网络请求生命周期 | 留作建议：功能规划项，涉及搜索 API 选择与写回策略，独立设计 | ✔ |
| PR-6 | 顶栏搜索占位文案承诺"搜歌词"，但搜索索引根本不含歌词 | `src/ui/header.rs:306`; `src/ui/search/model.rs:108-127` | low | header.rs:306 占位 `tr!("SEARCH_PLACEHOLDER", "Search songs, artists or lyrics")`；matcher 闭包（model.rs:108-127，本轮已读）Album/Artist/Track/KugouTrack/NeteaseTrack 五臂均无歌词字段 | 短期把占位文案改为与实现一致（"Search songs, artists or albums"，同步 translations 各语言该键）；歌词检索作为可选项需先对 100k 库做索引内存与首开延迟实测再决定 | 留作建议：1 行文案修复+多语言键同步，可随任意后续提交顺带 | ✔ |
| PR-7 | 在线下载把整个文件读进内存、无进度无取消，FLAC/HiRes 场景体验差 | `src/ui/kugou/download.rs:67-86,207-232`; `src/netease/download.rs:84` | medium | http_get_bytes 注释自述 "Fetches url into memory"，`response.bytes().await.map(\|b\| b.to_vec())` 一次性缓冲（:69-86）；download_track 全量下载后才写盘（:207-232），仅完成/失败 toast，fire-and-forget 无进度无取消 | 改流式写临时文件（bytes_stream + 逐块写盘），进度经 toast 通道节流上报，加取消 token，完成后原子 rename 到目标文件名（与 session_storage 样板同构） | 留作建议：行为改动+取消语义设计，独立一轮；收益与在线使用频率挂钩 | ✔ |
| PR-8 | 自动更新：仅作路线图设计储备，遵守 AGENTS.md 禁令绝不实施 | `AGENTS.md:64`; `build.rs:80-99`; `.github/workflows/release.yml` | low | AGENTS.md:64 明令"自动更新功能已完全移除……不要再引入或引用"；地基仍在：多平台发布矩阵 release.yml（本轮已读）、MELIORA_RELEASE_CHANNEL 渠道机制（build.rs:80-99 本轮已读，含 stable 分支 id 逻辑）、zed-reqwest HTTP 客户端与下载落盘管道（kugou/download.rs 本轮已读） | 完整设计见 ROADMAP.md §4；实施前提是先正式撤销 AGENTS.md:64 禁令并重新引入被删依赖（minisign-verify 等，届时走依赖评审），在此之前任何代码级实现都不应发生 | 留作建议：AGENTS.md 明令禁止实施；设计全文归入 ROADMAP.md §4 | ✔ |

---

## 4. 已实施改进明细（5 项）

> 本节以给定实施结果（outcomes）为准。实施各事项阶段均被硬性禁止运行 cargo 命令，各事项的 cargo 验收延后至收尾校验阶段统一执行（结果见 §5）；无法在本环境完成的验证在各项 notes 中如实标明。本轮 5 项 files_changed 均非空，无"留作建议"回流项。

### 4.1 改写 find_artists_tracks 排序 SQL 消除 JOIN 笛卡尔积（perf）

- **涉及文件**：`queries/library/find_artists_tracks_asc.sql`、`queries/library/find_artists_tracks_desc.sql`
- **建议提交信息**：`perf(library): find_artists_tracks 排序改写为预聚合子查询，消除双腿 JOIN 笛卡尔积`
- **摘要**：把双腿直接 JOIN（album_artist→track 与 track_artist 两条独立 leg 在同一 FROM 内成笛卡尔积，中间行数为 Σ(专辑曲目数×credits 数) 的乘法）改写为两个预聚合 GROUP BY 子查询 LEFT JOIN 回 artist 后按 COALESCE(n,0) 相加排序。两个子查询保留 `COUNT(DISTINCT t.id)` / `COUNT(DISTINCT ta.track_id)` 与原口径完全一致；COALESCE(…,0) 保住无曲目/无 credits 艺术家的零计数行（LEFT JOIN 语义不丢行）；外层 GROUP BY a.id 与 `name_sortable COLLATE NOCASE` 次级排序键原样保留，desc 版仅把计数排序翻为 DESC。唯一调用方 src/library/db.rs:290-295 的 list_artists 仍返回单列 (i64,)，无绑定参数，接口不变。无新增用户可见文案，translations 无需改动。
- **验收（已执行）**：python 3.13 sqlite3 (3.45.3) 按真实 schema（migrations 20240730163128/20240730163200/20260805085130/20260813000000 的 artist/track/album_artist/track_artist DDL 及两个索引）造最小数据集（多 artist 合辑且两腿重叠、无 credits、纯 credits、全空、credits 挂在 album_id 为 NULL 的曲目上、零曲目专辑、计数并列），旧(HEAD) vs 新(工作区) vs 独立 Python 参考实现 vs 手算期望四种输出逐行一致（asc 与 desc，含顺序），三个固定种子随机数据集同样全部一致，零计数艺术家均保留；EXPLAIN QUERY PLAN 显示两个子查询分别 MATERIALIZE 后按索引 LEFT JOIN，无 track×track_artist 嵌套循环积；中间行数探针 old-shape 7183/6215/6213 行 vs new-shape 30 行（合成小集即约 207–239 倍，真实大库差距随曲目数二次增长）。
- **验收（延后）**：cargo check 未在实施阶段运行（该阶段硬性禁止 cargo），由收尾校验统一执行（§5 已通过）；该验收对本改动本就无信息量：SQL 经 include_str! 在运行时作为字符串使用，编译不校验其内容（基线编译已通过，且未改任何 Rust 代码）。
- **notes**：最小数据集的 desc 手算期望曾写错一次（合辑两位 artist 各为 6 曲目+3 credits=9 并列，NOCASE 次级键应把 Guest Two 排在 Zed Compilation 前），修正后 SQL/Python 参考/手算三方一致；临时验证脚本已删除，未留任何多余文件（工作区 untracked 的 nul/tr_keys.tmp/trn_keys.tmp 为实施前已存在，未触碰）。

### 4.2 在线搜索结果更新不再全量重建 nucleo 索引（perf）

- **涉及文件**：`src/ui/search/model.rs`、`src/ui/components/palette/finder.rs`、`src/ui/search/search_item.rs`
- **建议提交信息**：`perf(search): 在线搜索结果移出 nucleo 索引并预计算搜索文本，结果刷新/退格不再全量重建索引`
- **摘要**：①搜索文本预计算：SearchPaletteItem 五个变体各增 `search_text: Arc<str>` 字段（Kugou/Netease 元组变体改为结构体变体以携带该字段），本地三项在 from_search_results（search_item.rs:70，后台加载任务内）按原 matcher 公式逐字构造（Artist=name、Album="title artist artists"、Track="title artists"），kugou/netease 在 on_kugou_query/on_netease_query 的 parse→map 构造处计算（model.rs:352-360、426-434，各 ≤15 条）；matcher 闭包（model.rs:111-121）由 5 处 format! 改为直接取 search_text 转 Utf32String，注入路径不再逐条格式化。②在线段移出 nucleo：PaletteItem 新增默认 false 的 `is_volatile()`（finder.rs:39-46），SearchPaletteItem 对 Kugou/Netease 变体覆写为 true（search_item.rs:188-196）；Finder 新增 `dynamic_items: Vec<(Arc<T>, Utf32String)>` 与复用的 `dynamic_matcher`（nucleo-matcher 0.3.1 的 Matcher::new 预分配 ~135KB slab，故存于结构体而非每次调用重建）；set_items（finder.rs:475-531）只把稳定项注入 nucleo，且改用零分配两趟遍历替代原先的切片重组，restart 仅剩稳定集（本地索引）变更这一低频路径；get_matches（finder.rs:533-563）改为 &mut self，在 dynamic_items 非空时用 `Pattern::parse`（Smart, Smart）+ dynamic_matcher 对每条已存 Utf32String 的 slice(..) 打分（无临时缓冲分配），按分数降序稳定排序（同分保持服务端顺序）追加在本地 snapshot 结果之后；limit=100 仍只作用于本地 snapshot，返回类型不变，on_accept 分派逻辑未动。
- **闭环核对（读码）**：三条路径均不再触发 matcher.restart——本地首载（injected 空→append-only 注入全部本地项，文本来自预计算）、在线结果到达（本地 Arc 指针前缀完全吻合→append-only→零 restart 零注入，仅替换 ≤30 条 dynamic_items）、退格 <2 字符清空（清 dynamic_items，稳定前缀不变→完全不触碰 nucleo）。
- **验收与偏离**：①cargo check 未在实施阶段运行（硬性禁止），由收尾校验执行；替代验证为通读三个文件全部改动区、rg 确认 src/ 下旧元组变体语法零残留且无 tests/benches 引用 SearchPaletteItem、并逐一核对所用 nucleo 0.5.0 / nucleo-matcher 0.3.1 API（Pattern::parse/score、Utf32String::from(&str)/slice(..)、Matcher::new、Config: Clone 非 Copy、Injector 无生命周期借用）在本地 registry 源码中的真实签名。②**事项文件清单未含 src/ui/search/search_item.rs，但预计算字段必须落在枚举变体上，故该文件必然改动**（字段新增 + Kugou/Netease 元组变体改结构体变体 + 12 处解构机械更新），on_accept 的变体→事件映射保持原样。③用 Pattern::parse 替代事项所述 Pattern::new：实际 API 的 Pattern::new 需第 4 个 AtomKind 参数，且 Pattern::parse（Atom::parse 语义）正是本地 nucleo 路径 MultiPattern::reparse 的解析方式，行为与本地匹配完全一致。④合并顺序按事项约定为本地在前、在线按分数降序在后；此前本地+在线统一按 nucleo 分数全局排序，故组内/跨组相对顺序与旧实现存在细微差异（事项明确采用此约定）。⑤带 kugou/netease key 的手动在线搜索验证无法在本会话执行，未运行。⑥工作区中 queries/*.sql 的改动与 nul、tr_keys.tmp、trn_keys.tmp 为此前已存在的他人改动，未触碰。
- **notes**：无新增依赖、无新增用户可见文案（tr! 词条未变），translations/meta.json 未手改。

### 4.3 歌词面板交互后停止空转帧循环（fix）

- **涉及文件**：`src/ui/lyrics.rs`
- **建议提交信息**：`fix(ui): 歌词面板交互窗口内不再空转帧循环，重跟随由过期后首帧消费`
- **摘要**：改造 Lyrics::needs_animation_frame（lyrics.rs:1030-1039）：删除 `|| self.has_recent_user_interaction()` 析取项，同时把 follow_pending 项改为 `(self.follow_pending && !self.has_recent_user_interaction())`，并加注释说明理由。效果：交互窗口内帧循环不再自我续期（滚轮/点击事件本身驱动重绘，lyrics.rs:724-757），emphasis 动画（180ms）与 scroll_follow 动画结束后帧循环立即停止；挂起的重跟随由交互窗口过期后的第一个 notify 帧（播放中 position tick 切行 :243/:250 或逐字行 tick :252-260）经 advance_follow_animation（:844-880）消费，完成"交互→事件驱动帧→循环停止→position tick 恢复 follow"闭环。:827-831 的 cancel 分支、:727-757 的 register_user_interaction 触发点、emphasis/follow 动画时序全部未动；无交互路径逐位等价（interaction 为 false 时新表达式退化为原 follow_pending 项）。
- **偏离说明（重要）**：未按 approach 字面"只删一项"实施——读码证伪其前提：register_user_interaction（:1020）在播放中交互后把 follow_pending 置 true，而 advance_animations 交互分支（:827-831）只 cancel scroll_follow（scroll_follow.rs:25-27 仅清 animation）不消费 follow_pending，故删除析取项后 needs_animation_frame 仍因 follow_pending 恒为 true，帧循环照样空转满 2 秒，与改前无差异，主场景无法达成。按任务授权（"发现前提不成立可按正确方向做并在 notes 说明"）改为在续期判定上加负条件，语义即 approach 闭环论证所描述的"follow_pending 由 position tick 驱动的下一帧消费"。
- **已知行为变化（approach 论证已接受）**：交互结束后视图不再在 2s 超时瞬间精确回弹，而是延迟到超时后第一个 notify 帧（下次切行；激活行带逐字歌词时最多延迟一个 33ms tick；长时间间奏的普通行则延迟到下一行激活）——用户滚走后回弹更温和，与主流播放器一致。
- **暂停态行为论证（报告撰写者读码补充，2026-09-26）**：给定 approach 的闭环论证与实施 notes 所列消费驱动（position tick / 逐字行 tick）均为播放态路径，暂停态未覆盖；读码补全如下——
  ①**暂停态下 follow_pending 仍可能被置位**：滚轮/点击仅在 `PlaybackState::Playing` 时调 register_user_interaction（lyrics.rs:727-738），但滚动条 on_interaction 无条件调（:750-757），register 置 `follow_pending = last_active_line.is_some()`（:1017-1021）；
  ②**消费点**：挂起的 follow 在"交互窗口过期后第一个执行 advance_animations 的帧"被消费（:830 → :844-880，Target/NoScrollNeeded 分支置 `follow_pending = false`，:857/:869）。该帧的来源是任何外部 notify（恢复播放时 playback_state observe 触发、切歌、窗口事件等），或挂起期间仍在进行的 PendingLayout 续帧链（:852-855）；`needs_animation_frame` 的 `(follow_pending && !interaction)` 项（:1034-1038）保证这类帧不会中途断链——它不依赖 position tick，因此**暂停态无死路**；
  ③**无空转**：消费后 follow_pending=false，第二项变 false，帧循环自然停止；
  ④**预期行为**：暂停态挂起期间滚动位置保持在用户所滚处（本就不应强行拉回），回弹延后到下一 notify 帧。边界情况：暂停 + 用户滚完歌词 + 此后既不恢复播放也无任何 notify 时，挂起的 follow_pending 保持不消费——滚动位置停在用户所滚处，无视觉异常，属可接受行为。
- **验收与未验证项**：未跑任何 cargo 命令（硬性禁止，编译校验由收尾阶段执行）；本改动为单函数内纯表达式修改，已用 rg 复核全部引用点且 needs_animation_frame 仅 lyrics.rs 使用。任务要求的"手动验证任务管理器 GPU/CPU 归零"无法在本环境执行，未验证。

### 4.4 清理 itertools 死依赖并纠偏 windows/sysinfo features（chore）

- **涉及文件**：`Cargo.toml`、`src/playback/thread.rs`
- **建议提交信息**：`chore: 清理 itertools 死依赖并纠偏 windows/sysinfo features`
- **摘要**：三件事，其中两处前提经读码证伪后按正确方向执行：
  1. **itertools**：删除 Cargo.toml 声明与 thread.rs 的 `use itertools::Itertools as _;`。"死依赖"前提不完全成立——全仓方法名扫描发现 thread.rs:1063（原行号）`paths.iter().format(":")` 是 `Itertools::format`（std Iterator 无此方法），是唯一活调用点（414/449 的 `.flatten()` 确为 std `Option::flatten`）。先替换为 std 等价写法 `map(ToString).collect::<Vec<_>>().join(":")`（QueueItemData 的 Display 在 queue.rs:136 只打印 path，与 itertools Format 输出逐字节一致；冷路径，无性能影响），然后删除依赖。该 trait 全仓仅 thread.rs 一处导入且基线编译通过，可逻辑排除其他文件的 itertools 用法。
  2. **windows features**：删除 Devices_Enumeration、Media_Audio、Media_MediaProperties、Media_Render 四项（逐一核对 windows 0.62.2 生成源码：这四个 feature 只门控各自子模块及 10 个 Meliora 从未调用的方法；导入的 12 个 Media 根类型及其 impl 块均无 cfg；Foundation 根模块经 Media/Storage 层级链保持启用，TypedEventHandler 不受影响）；新增 Win32_System_SystemServices 与 Win32_System_Threading（power.rs:16-17 实际导入，此前靠 cpal 0.18.2 同版本 windows 0.62.2 的 feature 统一透传，Cargo.lock:1573 证实）。**两处偏离**：Foundation_Collections 未删、Storage_Search 未收窄为 Storage——`StorageLibrary::Folders()`（scan.rs:50 调用）在 windows-0.62.2 Storage/mod.rs:5220 带 `#[cfg(all(feature = "Foundation_Collections", feature = "Storage_Search"))]`，两个动作任一都会让它消失（E0599），即"五项零引用"与"收窄为 Storage"两个前提均被生成代码的逐方法 cfg 证伪，故保留这两项。
  3. **sysinfo**：改为 `default-features = false, features = ["system", "disk"]`（附注释说明）。sysinfo 0.39.6 的 default 五项中 component/network/user 全无调用点，component 在 Windows 还会拖入 WMI/OLE/RPC 等 windows 子系统；三个调用点全部落在 system/disk：troubleshooting.rs:2,60-79（System/CPU/内存）、main.rs:166-178（System/Pid，仅非 Windows 分支）、library/scan/disk.rs:8,23（Disks）。
  未新增任何依赖，未动 [profile]/vendor/.cargo/build-release.cmd，未运行任何 cargo 命令与 git commit。
- **notes**：后续 check 应关注剩余四项删除与两项新增是否全绿（收尾校验已通过，见 §5）；thread.rs:1063 替换为多行链式写法（按 rustfmt 默认 chain_width=60 的展开形态手写），实施时未经 fmt 实跑确认（收尾已运行 cargo fmt）；**Cargo.lock 实施时未手改**（仍列 itertools 0.15 等待 cargo 刷新）——报告撰写实测：Cargo.lock 已随校验阶段 cargo 运行刷新，**itertools 0.15 直连边已消失**（仅剩 0.13/0.14 由其他传递依赖贡献，见 §5.2）；E0432/E0433 回退指引：Storage_Search→Storage 与 Foundation_Collections 两项已按要求预先放弃，无需回退。

### 4.5 在线凭据文件改原子落盘并记录读取失败（fix）

- **涉及文件**：`src/kugou/client.rs`、`src/netease/client.rs`
- **建议提交信息**：`fix(online): 酷狗/网易凭据文件改 tmp+rename 原子落盘，读取失败分级记日志`
- **摘要**：酷狗/网易在线凭据落盘改为原子写：KugouSession::save（client.rs:132-161）与 NeteaseSession::save（client.rs:119-148）先写同目录临时文件（`with_extension("json.tmp")`，得 kugou_session.json.tmp / netease_session.json.tmp），成功后 std::fs::rename 覆盖到真实路径（Windows 上 rename 覆盖已存在目标，沿用仓库 session_storage.rs:73-77 已论证的样板）；tmp 写失败记 warn 并 remove_file 清理，rename 失败同样 warn 并清理，原有 create_dir_all 前置与序列化失败 warn 全部保留。两处 load（kugou:114-130、netease:101-117）补齐 read_to_string 失败日志：ErrorKind::NotFound 时仅 debug!（首次使用无凭据是正常路径，不告警刷日志），其余 IO 错误 warn! 并带上 path 字段；返回 None→重新生成默认会话的语义不变。未改会话 JSON 字段与格式，未动 store_login/每日领取守卫等登录态逻辑（diff 仅含两方法）。无新增依赖（仅 std::io::ErrorKind，全限定无新 import）、无新增用户可见文案。
- **读码自查（已完成）**：①git diff 确认改动仅限两文件各两个方法；②rg 审计全部调用点（kugou save×5+测试×1、netease save×1+测试×1、load 各×2）签名与语义兼容，两个 round-trip 测试路径同样走 tmp+rename 不受影响；③tmp 命名冲突核查：数据目录真实文件为 kugou_session.json/netease_session.json，.tmp 后缀不与 settings.json/library.db/playback_session.json 及其他 tmp 写入方（playback_session.json.tmp、*.hsr.tmp、封面缓存 {file}.tmp）冲突，且无代码按 *.tmp 扫描/清理数据目录。
- **未验证项**：cargo check 由收尾校验执行（§5 已通过）；"登录后无残留 .tmp 且重启登录态保持"的手动验证无法在本环境执行，未验证。

---

## 5. 校验记录

### 5.1 给定校验记录（收尾校验阶段）

| 校验项 | 结果 | 说明 |
|---|---|---|
| 基线 check | **通过** | 命令形态按本轮实施事项约定的验收命令：`cargo check --release --tests --features kugou,netease`（release 模式、含 kugou/netease 聚合 feature）；给定校验数据仅记录"通过"结论，未附完整命令行与原始输出 |
| cargo fmt | **已运行** | 按仓库约定即 `cargo fmt --all`；实施事项 4.4 notes 中"手写展开形态未经 fmt 实跑"的遗留已由此覆盖 |
| 最终 check | **通过** | 同基线 check 命令形态，针对全部 5 项改动后的最终状态 |
| build-release.cmd | **通过** | 项目根目录 `build-release.cmd`（默认带 `--features kugou`，release 模式，按 AGENTS.md 约定为唯一构建入口） |
| 提交状态 | 记载"所有改动仅本地提交，未推送任何远端" | **报告撰写时实测与此矛盾，见 §5.3 第 1 条，以实测为准** |

**归因边界（重要）**：给定数据未记载校验运行时 §5.3.2 的 3 个计划外改动文件（controllers/windows.rs、main.rs、managed_image.rs）是否已在工作区树内；实测表明它们当前在树中，且校验在报告落盘前完成。因此四项"通过"应读作**含（或可能含）3 个计划外改动的完整树通过**，无法与 5 项改动的影响单独区分（反向同理：若曾出现编译错误，也无法区分其来源）。这 3 个文件的编译已被（若在树内的）check/build 覆盖，但其行为验收——尤其 managed_image.rs 位于 AGENTS.md 警示的图集回收漏斗路径的"崩溃不复发"——无任何记录，验收归属仍未闭环，需主线确认后另行提交与验证。

### 5.2 报告撰写阶段只读实测（本报告作者执行，未编译）

> **誊写更正声明**：本节第一版曾把带交替模式的 rg 命令誊写进 Markdown 表格单元格，为避免表格断列把交替符 `|` 写成了 `\|`——经独立复读员指出，`\|` 在 rg（Rust regex）中是**字面竖线**而非交替，按誊写形式复跑必然零命中，属不可复现的失真。现全部命令改为**精确可复现形式**（代码块内 `|` 即交替符），并在复审中**全部重跑**；重跑结果与第一版宣称的结论一致（涉及行号的均重新核对）。

#### 5.2.1 本轮实测命令与结果

```
git log --oneline -12
# → HEAD 为 358f9ec3c3（style: cargo fmt 全仓整理），其后无新提交

git status --short
# → 15 个文件改动 + 3 个未跟踪（nul、tr_keys.tmp、trn_keys.tmp），
#   未跟踪三项与"实施前已存在、未触碰"记载一致

git diff --stat
# → 15 files changed, 335 insertions(+), 144 deletions(-)；
#   Cargo.toml 12 行、Cargo.lock 22 行删除、translations/meta.json 6 行
#   （build.rs 构建期重写，AGENTS.md 明示属正常 diff）

git diff -- src/playback/thread.rs
# → use itertools::Itertools as _; 已删；
#   paths.iter().format(":") 已替换为
#   paths.iter().map(|path| path.to_string()).collect::<Vec<_>>().join(":")（§4.4 一致）

rg -n "itertools" Cargo.toml Cargo.lock src/playback/thread.rs
# → Cargo.toml 无声明、thread.rs 无导入；
#   Cargo.lock 仅剩 itertools 0.13/0.14（其他 crate 传递贡献），0.15 直连边已消失
#   （证实校验阶段 cargo 确已运行并刷新 lock）

rg -n "itertools|Itertools" src
# → 零命中（exit=1）：src/ 下 itertools 引用已全部移除（S-2 删除完成的现状）

rg -n "has_recent_user_interaction" src/ui/lyrics.rs
# → :1036 为 (self.follow_pending && !self.has_recent_user_interaction())，与 §4.3 一致

rg -n "json\.tmp|fs::rename|ErrorKind::NotFound" src/kugou/client.rs src/netease/client.rs
# → kugou client.rs:118(NotFound)/149(注释)/151(tmp)/157(rename)，
#   netease client.rs:105(NotFound)/136(注释)/138(tmp)/144(rename)，与 §4.5 一致

rg -n "is_volatile|dynamic_items" src/ui/components/palette/finder.rs src/ui/search/search_item.rs src/ui/search/model.rs
# → is_volatile 定义在 finder.rs:44 与 search_item.rs:188，
#   dynamic_items 在 finder.rs:118 等多处，与 §4.2 一致

rg -n "frame_time|frametime" src
# → 零命中（exit=1）：src/ 下无帧时间打点，P-2 佐证成立

rg -n "volume|seek|lyrics|queue" assets/keybinds.json
# → 仅命中 1 行：:20 {"key": "secondary-z", "action": "queue::Undo", ...}，PR-1 佐证成立

rg -n "items.is_empty|NO_ITEMS|empty_message" src/ui/components/table.rs
# → 零命中（exit=1）：table.rs 无空态提示，PR-3 佐证成立

rg -o "text_size\(px\(" src/ui | wc -l
# → 40：U-5 的"40 处 raw text_size(px)"复跑证实

rg -n "0x88888822" src
# → 恰 9 处：queue.rs:444、library/playlist_view.rs:294、library/sidebar/playlists.rs:438/440/441、
#   components/table/grid_item.rs:143/151、components/table/table_item.rs:164/171，U-5 佐证成立

rg -c "\.await\.unwrap\(\)" src/controllers/mpris.rs
# → 13：Q-4 的"13 处 await.unwrap"复跑证实

rg -n "crate::ui" src --glob "!src/ui/**"
# → 17 处（给定清单记 10 处，差值为 src/main.rs 的 7 处）；
#   逐点清单与归属判断见 §3.1 A-1 附注

ls src/ui / src/ui/kugou / src/ui/netease / src/kugou / src/netease；wc -l 上述平行文件
# → 平行对结构与行数（ui/kugou.rs 1452 / ui/netease.rs 1206 等），见 §3.1 A-1 附注

Read queries/library/find_artists_tracks_asc.sql（及 desc 版）
# → 新形态在盘：两个预聚合 GROUP BY 子查询 + COALESCE(at.n,0)+COALESCE(tc.n,0) 排序
#   + name_sortable COLLATE NOCASE 次级键
```

#### 5.2.2 审计数据给定、本轮未复跑的命令

以下命令与结果来自 findings 原文（读码/registry 源码核对类不产生可复跑命令），引用时以"给定数据"标识：

- `cargo tree -i windows@0.61.3`、`cargo tree -e features`（S-2/S-3 的双主版本与 Threading/SystemServices 透传证据）；
- registry 源码核对：cntp_i18n_core-0.3.0 的 `[dependencies.icu]` 伞依赖与 lib.rs 的 plurals/locale 引用（S-1）、windows-0.62.2 生成源码 `Storage/mod.rs:5220` 的双 feature cfg（§4.4 保留 Foundation_Collections/Storage_Search 的裁决依据）、gpui-ce view.rs:475 的 reuse_paint 语义（U-2）；
- python sqlite3 最小数据集四方一致验证与 EXPLAIN QUERY PLAN（§4.1，实施阶段执行）；
- i18n 全量 423 键逐语言覆盖率统计（U-4，原始审计 rg+python，本轮采信未复算）。

### 5.3 实测与给定数据的差异（矛盾注明，以实测为准）

1. **提交状态矛盾**：给定 checks 记载"所有改动仅本地提交，未推送任何远端"，但实测 HEAD 仍为 `358f9ec3c3` 且全部改动在工作区未提交。以实测为准：**截至本报告落盘，5 项改动尚未提交**，提交动作需随后执行（建议提交信息见 §1.3）。
2. **3 个超出 5 项记录的改动文件**（给定数据未记载其来源与验收，实测 `git diff` 确认）：
   - `src/controllers/windows.rs`（约 41 行区段）：SMTC 专辑封面持久流改为"流持久复用 + DataWriter 每次重建 + SetSize 精确钉长"，修复封面字节随曲目增长的回流问题（diff 注释详尽）；
   - `src/main.rs`（+2 行）：[mem] 探针输出新增 `funnel_pushed`、`tiles_kept_by_cache` 两个计数；
   - `src/ui/components/managed_image.rs`（+10 行）：keyed state 跨内容复用（同元素位置逐曲换封面）时把被覆盖旧图推入回收漏斗（`queue_orphan_tile_drop`），附中文注释。
   后两者的方向与本报告发现 P-2（[mem] 活动瓦片数）与 P-3（续体/复用路径漏斗覆盖）一致，但不属于本轮 5 项实施记录；其中 managed_image.rs 位于 AGENTS.md 警示的图集回收漏斗路径，其行为验收（崩溃不复发）不在给定数据范围内，**需主线开发者确认处置后另行提交与验证**。
3. `translations/meta.json` 的 diff：build.rs i18n 生成器构建期重写词条行号引用所致，AGENTS.md 明示属正常，随本次改动一起提交即可。

---

## 6. 未覆盖范围说明

1. **实施阶段的二进制/性能收益未实测**：icu 收窄（S-1）、windows features 收窄（S-2）、sysinfo 收窄（S-2）的编译时间与 exe 体积收益需构建前后对比（教义第 22 条）；本轮仅有 check/build 通过，无体积数字。
2. **手动/环境验证未做**：歌词面板重绘停止观察（GPU/CPU 归零，U-1）、带 kugou/netease key 的在线搜索回归（AL-1）、酷狗/网易登录后无残留 .tmp 与重启登录态保持（Q-2）、Linux MPRIS 行为（Q-4）、CI（release.yml，S-5）改动均无法在本机验证。
3. **i18n 全量 423 键逐语言对账未在本轮复算**（U-4 中各语言覆盖率数字采信原始审计的 rg+python 统计）。
4. **[profile] 各档位决策未重新评估**：现有决策有 bench 注释支撑，未发现推翻依据（performance 概览），不动。
5. **帧时间归因未测量**：`set_trace_enabled(false)`，rg 无 frame_time 打点；建议按需 samply/ETW（P-2）。
6. **合并与否决的明细（含账目缺口）**：给定数据的合并标注共涉及原始审计 **7 条编号**（A1+A2、P2+P3、S4+S2+S5，分别为 2+2+3 条）并入 3 条合并发现，1 条最终否决（S-3 windows 双主版本）。账目缺口：synthesis 断言 40→34，但按合并算术 40−7+3=36，相差的 2-3 条原始发现去向在给定数据中未交代；原始 40 条完整对照清单不在给定数据内，本报告如实留缺口、不虚构补齐（§1.1/§2 同）。
7. **自动更新功能的完整设计文档**（原始审计材料 roadmapNotes 第 6 条）未随本次给定数据提供，REPORT/ROADMAP 只收录数据明示要点，不虚构其余内容。
8. **工作区 3 个计划外改动文件**（§5.3 第 2 条）的来源、验收与提交归属不在本轮给定数据范围内。
9. `assets/tests/`（音频测试样本）历史上已删除，引用 `assets/tests/audio-fixtures` 的测试失败属已知状态（AGENTS.md），未在本轮处理。

---

## 7. 后续入口

- 长远布局、分期规划、全量 backlog 与自动更新设计储备：见 [ROADMAP.md](ROADMAP.md)。
- 下批实施候选（按 §1.2 结论 5 与 ROADMAP §2.1 排序）：A-2 token_key 上移 → P-1 auto_height 网格 → U-2 queue_width observe → Q-1 播放线程离线日志 → Q-3 命令面板降级 → U-4 zh-CN 3 键。
