# find-stutter TODO

## 架构决策

**服务采集 + GUI 显示**（SQLite WAL 轮询）
- 服务（后台）：`Collector` + `Logger` 常驻，每秒写 stutter.db
- GUI（前台）：连接同一个 stutter.db，只做 `SELECT` 读取，每秒刷新
- 不需要 IPC/互斥，SQLite WAL 模式支持并发读写
- GUI 崩溃不丢数据，服务停止后 GUI 显示"服务已断开"

## 已完成
- [x] 系统指标采集（CPU/内存/网络/进程）— `collector.rs`
- [x] 卡顿检测引擎（阈值+突变检测）— `detector.rs`
- [x] SQLite 日志 + CSV 导出 — `logger.rs`
- [x] 悬浮窗 UI（透明+置顶+CJK 字体）— `app.rs` + `overlay.rs`
- [x] 皮肤系统（TOML 配置）— `skin.rs`
- [x] CLI 命令（run/export/stats）— `main.rs`
- [x] 147 个单元测试
- [x] Logger 集成到 overlay 模式（每秒采样写入 stutter.db）
- [x] CSV 导出修复（Vec<f32> 序列化问题）
- [x] Win32 WS_EX_LAYERED 分层窗口设置
- [x] **拖动无重影** — 改用 egui `ViewportCommand::StartDrag`（winit 在 Windows 上走 `SC_DRAGMOVE` 原生拖拽，由系统负责重绘），彻底消除透明窗每帧 `OuterPosition` 位移导致的重影/闪烁 — `crates/ui/src/app.rs`
- [x] **磁盘速率采集** — PDH API `\PhysicalDisk(_Total)\Disk Read/Write Bytes/sec`，每 tick 采样，不再恒为 `0 B/s` — `crates/core/src/collector.rs`
- [x] **GPU 利用率采集** — WMI `Win32_PerfFormattedData_GPUPerformanceCounters_GPUEngine`，累加各引擎 `UtilizationPercentage`（兼容 UI8/UI4）并封顶 100%；WMI 失败降级 `N/A`（sysinfo 不提供 GPU 利用率字段，故不回退 sysinfo）— `crates/core/src/collector.rs`
- [x] **CPU 温度采集** — WMI `Win32_PerfFormattedData_ThermalZoneInformation`，失败降级 `N/A` — `crates/core/src/collector.rs`
- [x] **SQLite WAL 模式** — `Logger::new` 打开连接后执行 `PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;`，采集线程持续写、GUI/导出并发读互不阻塞，为服务化采集 + GUI 只读模式打基础 — `crates/core/src/logger.rs`
- [x] **卡顿闪烁提醒** — 后台线程检测到 Major/Critical 卡顿时在 `OverlayState` 写入严重程度与闪烁截止时间，悬浮窗边框脉冲闪烁（Critical=红 / Major=橙）；同时修复 `stutter_count` 此前从未累加的 bug — `crates/ui/src/app.rs` + `overlay.rs` + `crates/core/src/types.rs`(`Severity`)
- [x] **右键菜单** — 用 egui 内置 `Response::context_menu` 实现：暂停/恢复监控、展开/收起详情、点击穿透开关、退出（共享 `paused` 标志控制后台采集线程）— `crates/ui/src/app.rs`（注：原计划用 `muda`，egui 内置菜单更稳且无新依赖风险，托盘菜单仍可用 `muda`+`tray-icon`）
- [x] **点击穿透模式** — `FindWindowW` 按标题取 HWND，`SetWindowLongPtrW` 切换 `WS_EX_TRANSPARENT`；右键菜单开关 + `T` 键兜底退出 + 穿透时显示提示文字 — `crates/ui/src/app.rs`
- [x] **config.toml 注释完善** — 所有字段加中文注释说明 — `config.toml`
- [x] **README.md** — 项目说明、构建/运行、CLI 子命令、交互说明、配置、目录结构、已知限制 — `README.md`
- [x] **P3 服务化重构** — 拆出独立 `find-stutter-service` crate（Windows 服务，6 个 CLI 子命令），GUI 改为 1Hz SQLite 轮询只读（`DbReader` + `ServiceHealth`），WAL 并发读写，111 个测试全过 — `crates/service/` + `crates/ui/src/reader.rs`

## 未完成

> **P0（核心功能）已全部完成**，见上方"已完成"列表。以下为 P1–P4 待办（本会话已补齐 P1 右键菜单/点击穿透、P2 卡顿闪烁、P3 WAL 基础、P4 文档）。

### P1 — 交互功能
- [ ] **系统托盘图标** — 最小化到托盘 + 托盘右键菜单（`tray-icon` + `muda`）。需与 eframe 事件循环集成，沙箱内无法可视化验证运行时行为，暂缓实现。窗口内右键菜单已可用作替代。

### P2 — 高级功能
- [ ] **任务栏嵌入** — 伪任务栏窗口（无边框透明，手动拖到任务栏位置）
- [ ] **配置热加载** — `notify` crate 监听 config.toml 变更并热更新采集/显示配置
- [ ] **通知弹窗** — Windows 原生 toast 通知（检测到 Major/Critical 时弹出）

### P3 — 服务与部署（服务化架构重构）
- [x] **Windows 服务改造** — 独立 `find-stutter-service` crate（`crates/service/`），用 `windows-service` 0.8 注册 SCM 服务；服务循环：每秒 `collect → detect → touch_heartbeat → write_sample`，提供 `run` / `install` / `uninstall` / `start` / `stop` / `status` CLI 子命令；SCM 名 `FindStutter`
- [x] **GUI 改为只读模式** — `crates/ui/src/reader.rs` 新增 `DbReader`，1Hz 轮询 `stutter.db`（`latest_sample_summary` / `latest_heartbeat` / `latest_event` / `event_count_today`）；删除 `spawn_collector` 和 `Collector` 实例；UI 只跑只读连接
- [x] **服务健康检测** — `Logger::touch_heartbeat`（id=1 单行 UPSERT）+ `DbReader::poll` 推算 `ServiceHealth` (`Running` / `Stale` / `Stopped` / `NoDatabase`)；UI 顶部状态条：绿/黄/红配色 + 文字；暂停按钮在非 `Running` 时禁用
- [x] **WAL 并发读写** — `Logger` 端 `PRAGMA journal_mode=WAL` + `Reader` 端 `SQLITE_OPEN_READ_ONLY` + `PRAGMA journal_mode=WAL`，服务写、GUI 读互不阻塞
- [x] **P3 测试** — `find-stutter-service` 16 测试 + `find-stutter-ui` 30 测试（reader 健康检测 + overlay 格式化 + integration reader 端到端），全部通过
- [x] **GUI 自动启动服务** — `crates/ui/src/auto_start.rs` 启动 GUI 时检测后台服务：复用 `find-stutter-service status` / `start` 子命令；找不到 exe / 未注册 / 启动失败 都不阻塞 GUI 启动，仅写日志；7 个新单元测试，总测试数 118 — `crates/ui/src/auto_start.rs`



### P4 — 代码质量
- [x] **移除 unused warnings** — 清理 `crates/core/src/logger.rs` 未使用的 `DateTime` 导入、`crates/ui/src/overlay.rs` 未使用的 `Duration` 导入；release 构建现已零警告
- [x] **config.toml 示例完善** — 添加所有字段的注释说明
- [x] **README.md** — 项目说明、使用方法、构建指南
