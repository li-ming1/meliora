# 发布流程（Release Checklist）

v0.0.6 起的标准五步。每一步都对应一次真实的发布日坑，按顺序执行，不要跳步。

## 1. bump 版本

- 同时修改 `Cargo.toml` 与 `Cargo.lock` 中 meliora 的 `version`（两处都要改）。
- **为什么**：应用内更新器（`src/updater.rs` 的 `apply_check_result`）用 `CARGO_PKG_VERSION` 与 `/releases/latest` 的 tag 做数值比对——tag 不大于运行版本即报"已是最新"。**不 bump 就发 tag，客户端永远检测不到新 release。**

## 2. 提交并推送 main

- 要发布的内容全部进 main 后再打 tag。
- push-to-main 会触发 `.github/workflows/check.yml`：四个非 Windows 目标（macOS x64/arm64、Linux x64/arm64）的 `cargo check --features kugou` 门禁。**本地只编 Windows，这道门禁是平台代码在发布日前唯一的编译验证**——红了先修，不要带着红灯往下走。

## 3. 打 tag 并推送

```bash
git tag -a vX.Y.Z -m "Meliora vX.Y.Z"
git push origin vX.Y.Z
```

- 自动触发 `.github/workflows/release.yml`：6 平台矩阵构建（fail-fast: false），全部成功后 release job 建 GitHub Release（非 draft、自动生成 notes）。

## 4. 监控与失败处置

- 监控：`gh run list --workflow=release.yml`；`gh run watch <id> --interval 30 --exit-status`（本机 gh 的 `run view` 没有 `--interval` 参数，别用）。
- **编译失败时不要直接重打 tag**。先修，再用手动触发单编失败平台验证：

  ```bash
  gh workflow run release.yml --ref main -f platforms=macos-x64,macos-arm64,linux-x64,linux-arm64
  ```

  全绿后再移动 tag（删远端 → 本地重打 → 推送）：

  ```bash
  git push origin :refs/tags/vX.Y.Z
  git tag -d vX.Y.Z && git tag -a vX.Y.Z -m "Meliora vX.Y.Z"
  git push origin vX.Y.Z
  ```

- 失败日志：`gh run view <id> --log-failed`。**警惕 lock 里同名 crate 的多版本**（如 objc2-app-kit 0.2.2/0.3.2 并存），核对 API 认准本项目依赖的版本目录。

## 5. 手工补 Release 正文

- 工作流 body 模板只有固定介绍，**「本版本变更」历来是发布后手工补**（v0.0.5 / v0.0.6 均如此，v0.0.6 的成品可在 git 历史里找）。
- 归类统计：

  ```bash
  git log --format='%s' vA.B.C..vX.Y.Z | sed 's/(.*//' | sort | uniq -c | sort -rn
  ```

- 正文骨架（六节，条目带 7 位短哈希）：

  ```markdown
  ### 本版本变更 / What's new in vX.Y.Z

  自 vA.B.C 以来共 N 个提交，几条主线：……

  **新功能 / Features**
  **内存与稳定性 / Memory & stability**
  **性能 / Performance**
  **修复 / Fixes**
  **重构与依赖 / Refactoring & dependencies**

  ### 关于体积 / About binary size

  Windows x64 裸二进制 A → B（-x%）（`gh release view` 对比两版 assets 的 size）。
  ```

- 更新正文：`gh release edit vX.Y.Z --notes-file <file>`。
- **动手前用 `git grep <符号> <上一个tag>` 核对每项修复在上个版本是否已存在**——v0.0.6 的内存四层修复其实在 v0.0.5 就已落地，勿把旧功劳算进新版本。

## 附：发布日坑位清单

| 坑 | 防线 |
|---|---|
| 不 bump 版本就发 tag → 客户端永远检测不到更新 | 第 1 步 |
| 平台代码编不过 → 发布日多平台全挂、连续三轮 | check.yml 门禁（第 2 步）+ 第 4 条的 dispatch 验证 |
| 移动 tag 时忘删远端 tag → push 被拒 | 第 4 条命令顺序（先删后推） |
| Release 正文只有模板、没有变更说明 | 第 5 步 |
| 文档宣传已删除的功能（网易云、YRC） | 发版前对功能词做全仓 grep（含 README 与 release.yml 模板） |
