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

## 未完成

### P0 — 核心功能
- [ ] **拖动无重影** — 用 WM_NCHITTEST 原生拖拽替代 egui 拖拽，彻底解决透明窗口重影
- [ ] **GPU 利用率采集** — WMI `Win32_PerfFormattedData_GPUPerformanceCounters` 失败时降级到 sysinfo
- [ ] **磁盘速率采集** — PDH API 精确实时速率，替代 sysinfo delta
- [ ] **CPU 温度采集** — WMI `Win32_PerfFormattedData_ThermalZoneInformation` 降级方案

### P1 — 交互功能
- [ ] **右键菜单** — 暂停/恢复监控、显示设置、皮肤切换、退出（`muda` crate）
- [ ] **系统托盘图标** — 最小化到托盘、托盘右键菜单（`tray-icon` crate）
- [ ] **点击穿透模式** — WS_EX_TRANSPARENT 切换，穿透时拖拽失效需提示
- [ ] **展开详情面板** — 点击展开显示今日流量、CPU 频率、温度、卡顿统计

### P2 — 高级功能
- [ ] **任务栏嵌入** — 伪任务栏窗口（无边框透明，手动拖到任务栏位置）
- [ ] **配置热加载** — `notify` crate 监听 config.toml 变更
- [ ] **卡顿闪烁提醒** — 检测到 Major/Critical 时悬浮窗闪烁
- [ ] **通知弹窗** — Windows 原生 toast 通知

### P3 — 服务与部署
- [ ] **Windows 服务改造** — 服务只做采集+写库，不负责 GUI（~30 行）
- [ ] **GUI 改为只读模式** — 从 stutter.db 读取最新数据，不做采集
- [ ] **SQLite WAL 模式** — 服务写 + GUI 并发读，无锁竞争
- [ ] **服务健康检测** — GUI 检测服务是否运行，断开时提示"服务已停止"
- [ ] **开机自启** — 通过服务安装实现
- [ ] **Installer 打包** — NSIS 或 WiX 安装包

### P4 — 代码质量
- [ ] **移除 unused warnings** — theme.rs 未使用的常量、skin.rs default_opacity
- [ ] **config.toml 示例完善** — 添加所有字段的注释说明
- [ ] **README.md** — 项目说明、使用方法、构建指南
