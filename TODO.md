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

## 未完成

> **P0（核心功能）已全部完成**，见上方"已完成"列表。以下为 P1–P4 待办（本会话已补齐 P1 右键菜单/点击穿透、P2 卡顿闪烁、P3 WAL 基础、P4 文档）。

### P1 — 交互功能
- [ ] **系统托盘图标** — 最小化到托盘 + 托盘右键菜单（`tray-icon` + `muda`）。需与 eframe 事件循环集成，沙箱内无法可视化验证运行时行为，暂缓实现。窗口内右键菜单已可用作替代。

### P2 — 高级功能
- [ ] **任务栏嵌入** — 伪任务栏窗口（无边框透明，手动拖到任务栏位置）
- [ ] **配置热加载** — `notify` crate 监听 config.toml 变更并热更新采集/显示配置
- [ ] **通知弹窗** — Windows 原生 toast 通知（检测到 Major/Critical 时弹出）

### P3 — 服务与部署（服务化架构重构）
- [ ] **Windows 服务改造** — 服务只做采集+写库，不负责 GUI（~30 行，`windows-service` crate 已声明）
- [ ] **GUI 改为只读模式** — GUI 从 stutter.db 轮询读取最新数据，不再自行采集（WAL 模式已就绪，可并发读）
- [ ] **服务健康检测** — GUI 检测服务是否运行，断开时提示"服务已停止"
- [ ] **开机自启** — 通过服务安装实现
- [ ] **Installer 打包** — NSIS 或 WiX 安装包

### P4 — 代码质量
- [x] **移除 unused warnings** — 清理 `crates/core/src/logger.rs` 未使用的 `DateTime` 导入、`crates/ui/src/overlay.rs` 未使用的 `Duration` 导入；release 构建现已零警告
- [x] **config.toml 示例完善** — 添加所有字段的注释说明
- [x] **README.md** — 项目说明、使用方法、构建指南
