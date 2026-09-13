# gpui-ce / Zed DirectX 图集：瓦片搁浅与悬空槽位（上游问题分析）

> 本文是提交给 gpui-ce（PR）与 zed-industries/zed（issue/PR）的完整材料。
> 现场证据：Meliora（gpui-ce 应用）日志中 4 次 `etagere bucketed.rs:496` 代际断言
> + 3 次 `directx_atlas.rs:255` texture() unwrap 崩溃（完整回溯已留存）。

## 两类崩溃，两个上游各缺一半修复

| | 槽位保持（页清空后可采样） | allocate 跳过 free_list 页 |
|---|---|---|
| **gpui-ce**（ae7c411） | ✅ 已有（2026-09-08 补丁，`remove` 无条件回填） | ❌ 缺失 → **搁浅断言崩溃** |
| **Zed 主线** | ❌ 缺失（页清空后槽位置 None） | 天然免疫搁浅（None 槽位被 flatten 跳过）→ **悬空槽位崩溃类** |

- **gpui-ce 的搁浅链**：页清空 → free_list + 槽位保持 Some → `allocate()` 的
  `iter_mut().flatten()` 仍会命中该页并分配新瓦片（`tiles_by_key` 指入待回收页）
  → 之后 `push_texture` 用全新分配器覆盖槽位 → 旧键指向死分配器 → `remove`
  时 `assert_eq!(generation, expected_generation)`（left 1 right 0）。
- **Zed 主线的悬空链**：`remove` 对 unreferenced 页**不回填槽位**（:116-124 take
  后仅 else 分支回填）→ 若 `drop_image` 发生在帧管线内而同帧 sprite 仍引用该页
  → present 时 `texture()` 对 None 槽位 unwrap 崩溃。

**完整修复 = 槽位保持 + allocate 跳过 free_list**（本目录
`gpui-ce-directx-atlas-stranding.patch`，+61/-4 行，已在本机 Meliora 实测验证）。
对 Zed 主线：需要引入槽位保持 + allocate 跳过两件套，才能同时消除悬空与搁浅。

## 复现条件（Meliora 实测）

1. 封面密集列表快速滚动（大量 256px 瓦片分配/回收，页频繁清空入 free_list）；
2. 同一会话内出现一张放不进任何现有页的大图（全尺寸封面，触发 `push_texture`
   从 free_list 弹出槽位并重建分配器）；
3. 此后任意一次对搁浅键的 `drop_image` → `bucketed.rs:496 assert_eq!` 崩溃；
   或（catch_unwind 兜底下）`tiles_by_key` 其他键指向的槽位被 pop 重建 →
   present 时 `directx_atlas.rs:255` unwrap 崩溃。

## 补丁内容（gpui-ce 版，即本 patch 文件）

1. `allocate()`：跳过 `free_list` 中的槽位（收集 free_list 后 enumerate+rev+filter）；
2. `remove()`：`deallocate` 包 `catch_unwind` + 无条件回填槽位（防御性：即使
   别处断言，槽位不残留 None）。

## 给上游的 issue 文本（英文，可直接粘贴）

### gpui-ce (PR) / zed-industries (issue)

**Title**: gpui_windows: tiles allocated into free-listed atlas pages get stranded, tripping etagere's generation assertion on later `remove`

**Body**:

While investigating random `assert_eq!(generation, expected_generation)` aborts
in `etagere::bucketed.rs` (and `directx_atlas.rs` `texture()` unwrap panics) in a
music player built on gpui, I traced a deterministic lifecycle hole in
`DirectXAtlas` on Windows:

1. When a texture page's last key is removed, `remove` pushes the page index to
   `free_list`. The slot-keep behavior (page object stays `Some` in the slot so
   in-flight sprites stay sampleable) means `allocate()`'s
   `textures.iter_mut()` traversal **still reaches free-listed pages** and hands
   out new tiles from them; `tiles_by_key` then references a page that is
   pending recycle.
2. A later `push_texture` (an image that fits no existing page) pops that slot
   from `free_list` and installs a **fresh allocator** (all generations reset to
   0). Every `tiles_by_key` entry allocated in step 1 is now stranded.
3. Any subsequent `drop_image` for a stranded key deallocates a gen-N tile id
   against a gen-0 bucket: `assert_eq!(generation, expected_generation)` in
   etagere aborts the process. (With the panic caught, the slot is left `None`
   and present-time `texture()` unwraps the dangling slot instead.)

Observed in the wild: 4 generation-assertion aborts + 3 dangling-slot unwrap
panics across sessions, all correlated with cover-image heavy scrolling followed
by a large-image allocation.

**Fix** (patch attached, +61/-4, in production in our app since 2026-09-13 with
no recurrence): in `allocate`, skip slots whose index is in `free_list`. As
defense in depth, wrap the `deallocate` call in `catch_unwind` and always
restore the slot.

For zed-industries/zed mainline specifically: `remove` currently leaves the slot
`None` for unreferenced pages (no slot-keep), so pages recycled via `free_list`
cannot strand keys there — but the slot-None behavior is itself the dangling
sprite hazard on mid-frame `drop_image`. The combined
"slot-keep + skip-free-listed-in-allocate" pair resolves both.

## 中文操作指引

1. gpui-ce：fork 后应用 `patches/gpui-ce-directx-atlas-stranding.patch`，开 PR
   （标题/正文用上文英文）。
2. zed-industries/zed：开 issue（英文文本），指出 mainline 缺 slot-keep 存在
   悬空风险，并附 gpui-ce 的完整修复作为参考实现。
3. Meliora 侧：本地 checkout 已打补丁（`MELIORA LOCAL PATCH` 标注），上游合并
   后升级 rev 即可删除本地补丁。
