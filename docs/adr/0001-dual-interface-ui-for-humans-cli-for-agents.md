# ADR-0001：双界面架构 —— UI 给人、CLI 给 agent，分析纯函数下沉 core

- 状态：已接受（2026-08-16）
- 背景：grilling 会议（简化项目结构：一条命令启动 + ui/cli 双轴重组）

## 背景

项目原本是「GUI 为主、CLI 附带」的形态：`find-stutter` 主入口启动 GUI，CLI 仅有 `export`（CSV）与 `stats`（一行文本）；分析聚合的纯函数（KPI、元凶榜、因果链等 F-RC5~13）住在 `ui` crate，CLI 想复用就得拖入整个 slint/GUI 依赖树。与此同时，本仓库的日常开发由 coding agent 承担（验收判定标准效果、排查误报、查事件），却没有机器可读的查询入口，agent 只能直接读 SQLite。

## 决策

1. **界面双轴成为组织原则**：
   - `crates/ui` = 给人看的（悬浮窗、进程详情、分析图表）；
   - `crates/cli` = 给 agent 用的（一等 crate：`events` / `samples` / `analysis` / `config` / `status` / `process` 子命令，JSON 输出：英文键、ISO8601 时间、`--from/--to/--limit` 过滤）；
   - `crates/bin` 变薄：只做分发——无参数 = 启动 GUI（并确保服务在跑），子命令 = 转发 CLI。
2. **分析纯函数下沉 core**：`ui/analytics` 中无 GUI 依赖的聚合逻辑搬入 `find-stutter-core`，UI 与 CLI 共用同一份分析口径，避免两边各自漂移。
3. **CLI 契约跟随领域模型演进**：首要服务本仓库的 coding agent，不冻结 JSON schema、不做版本承诺；等出现第二个消费者再考虑稳定契约。
4. **CLI 查询为主，不做提权控制**：`status` 可读服务/心跳健康；`start/stop/install` 等需要管理员权限的控制不进 CLI（人类走 GUI 自动拉起或 service exe 子命令）。
5. **启动收口为一条命令**：`find-stutter`（无参数）= GUI + 自动确保服务（UAC 由系统弹出）；文档（README/BUILD）同步只写这一条。
6. **升级收进同一条命令入口**：新增 `find-stutter upgrade [--no-build]` 子命令（Rust 实现：停服释放 exe 锁 → `rtk cargo build --release` → `install-start`，UAC 提权），废弃本地 `upgrade-service.ps1` 脚本——启动与升级共用一个入口，升级流程进入版本控制（ps1 此前被 `*.ps1` 忽略规则排除在仓库外，且三次踩坑：MSYS 路径、`sc` 别名、引号转义）。

## 备选方案与取舍

- **单进程模式（砍掉 Windows 服务）**：无 SCM/UAC 复杂度，但 GUI 关闭即停止采集、失去免登录自启——拒绝。
- **CLI 依赖 ui crate 复用分析函数**：零搬迁成本，但 agent 查询要编译/携带整个 GUI 依赖树——拒绝（编译时间与依赖面）。
- **通用 agent 接口（稳定 schema + 版本化）**：面向未知消费者做过早设计——拒绝（YAGNI），保留升级路径（先内部后冻结）。

## 后果

- workspace 变为 5 个成员：core / ui / cli / bin / service。
- `stats` 子命令删除（被 `events --today` 覆盖）；`export`（CSV）保留；`find-stutter-ui.exe` 这个重复入口移除，收敛到单一 `find-stutter`。
- CSV 导出表头维持中文（对人），JSON 键英文（对 agent）——两套受众、两套约定，属有意为之。
