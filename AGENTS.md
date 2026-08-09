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
