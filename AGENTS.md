# AGENTS.md

本文件用于指导在本项目中工作的 AI 编码助手（Coding Agent）。

## 语言要求

- 所有输出均使用中文：包括对话回复、代码注释、文档，以及提交信息。
- 技术专有名词（API、CPU、SQL、GPU、WAL 等）可保留英文原词，但解释与叙述必须用中文。
- 代码中的变量名、函数名、类型名遵循代码库现有的英文约定，无需强行中文化。
- 导出的数据文件（CSV 等）表头与说明使用中文。

## 命令执行约定（RTK）

本项目使用 **RTK** 作为 cargo 的工具链包装器，所有 cargo 命令**必须**通过 RTK 执行，不得直接调用 `cargo`。

- RTK 路径：`/d/app/cargo/bin/rtk`（v0.42.4）。
- 用法：在任意 `cargo` 子命令前加 `rtk` 前缀。
  - 编译：`rtk cargo build`
  - 检查：`rtk cargo check`
  - 测试：`rtk cargo test`（可按包指定，例如 `rtk cargo test -p find-stutter-ui`）
  - 运行：`rtk cargo run`
- 任何绕开 RTK 直接执行 `cargo` 的操作都可能导致工具链/编译环境不一致，属于禁止行为。

## 多任务并行开发约定（强制）

本项目允许多个 AI 会话并行开发，但**一个任务必须对应一个独立 git worktree**，禁止多个会话在同一工作区同时改代码。完整方案见 `docs/并行开发规范.md`。

- 位置固定：主线 `D:\dev\cli\find-stutter`（`main`，只做集成与共享文件），任务 `D:\dev\cli\find-stutter-wt\<task-id>`（分支 `task/<task-id>`）。不临时另建目录。
- 开工先读 `D:\dev\cli\find-stutter\.workbuddy\parallel\board\` 下所有「进行中」条目，登记本任务的**文件所有权**；与他人有交集就换任务或等待。
- 共享文件（根/各 crate `Cargo.toml`、`Cargo.lock`、`config.toml`、`AGENTS.md`、`BUILD.md`、`README.md`、`UPGRADE.md`、PRD、`docs/adr/`）**只有主线能改**。任务分支不得执行 `cargo add` / `cargo update`，不得提交 `Cargo.lock` 变更。
- **轻量默认：全机只有一个 `CARGO_TARGET_DIR`（build worktree `D:\dev\cli\find-stutter-build\target`）+ 一条串行构建队列**。coding worktree 禁止执行任何 cargo 命令；编译与测试只是队列里的两个阶段，**整段持锁**（MCP Agent Mail build slot），不能只靠 cargo 自带的 build lock（它只覆盖单个 cargo 命令，`cargo test` 运行测试期间会释放）。每次切分支先 `rtk cargo clean -p` 清掉 5 个 workspace crate（否则共享 target 会复用旧 rlib、静默链接到别的任务代码），job 结束 `reset` 回 main。详见 `docs/并行开发规范.md` §3 / §9。
- `CARGO_BUILD_JOBS=2`（重链接改 1）；本机所有 cargo 命令必须 `rtk` 包裹，任务分支不跑 `--release`。**CI 是例外**：`AGENTS.md` 的 RTK 规约只约束本机 agent 会话，云端 runner 没有 RTK，`.github/workflows/windows-test.yml` 直接用 `cargo`。看到 `can't find crate` / `invalid metadata files` 先怀疑内存不足，别去改代码。
- 运行前设 `FIND_STUTTER_SKIP_SERVICE=1`，用私有 config 指向私有 DB 与日志；主线根目录的 `stutter.db` 只读，禁止写、删、`VACUUM`。
- 任务 worktree **禁止**执行 `install-start` / `restart` / `upgrade`，禁止启动真实采集：SCM 服务 `FindStutter` 与 `NT Kernel Logger` / `find-stutter-net` 两个 ETW 会话是系统级独占资源，需要时先在 `.workbuddy/parallel/runtime.lock` 登记，一次一个任务。
- 测试由队列串行执行，只跑改动相关包（`rtk cargo test -p <包> --lib --test <目标> -- --test-threads=2`），并与主线基线对照，明确区分「本次改动引入的回归」与「既有/并行环境失败」；执行 Agent 不自己补跑。
- 提交前 `git diff --name-only main...HEAD` 自检越界；只提交本任务文件，不要 `git add -A`；提交信息一律中文。**main 的唯一写者是合并队列**：由总控推 `task/<id>` 分支 → 开 PR → Actions `test` 绿 → 打 `ready-to-queue` label 入队；禁止本地 `git merge --ff-only` 或 push main。收工 `git worktree remove`，并确认 build worktree 已 reset 回 main、build slot 已释放。
- **开工先 `source /d/tools/parallel/env.sh`**（含 `STORAGE_ROOT` 指向 D 盘、`WORKTREES_ENABLED=true`、`BEADS_NO_DAEMON=1`）。队列入口固定为 `JOB_ID=<任务> AGENT_NAME=<会话> /d/tools/parallel/queue-worker.sh <命令>`；`WORKTREES_ENABLED` 不设会让 `am am-run` 不持租约（假排队），`--block-on-conflicts` 冲突时是 abort 而非等待，排队靠 worker 的重试循环。任务图用 `br`（`.beads/`）。两种模式不要同时用同一 `STORAGE_ROOT`：stdio MCP 或 HTTP 服务二选一。
