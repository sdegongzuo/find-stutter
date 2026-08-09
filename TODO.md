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

### 进程详情页 & 悬浮窗迭代（2026-08-07）

- [x] **进程聚合支持同名多 root**（c55aa4c）— 组内 PPID 不在组内的进程均为 root，
      每个 root 独立成组；子进程沿 PPID 链归属（find_root_pid 防环），孙进程扁平化。
- [x] **进程内存列改提交大小**（e012ed6）— PagefileUsage（任务管理器「详细信息」页
      口径），失败回退工作集；采样热路径每进程一次 GetProcessMemoryInfo。
- [x] **进程详情页首次显示修复** — 非阻塞 refresh（去掉 3s 死等）+ 首帧 1s 快速 tick，
      数据就绪立即渲染，窗口不卡顿。
- [x] **进程详情页列调整**（7f92b0a）— 删「状态」「操作」「用户」列（用户列连带
      移除采样热路径的 user 查询，每进程省一次 token 查询）；新增「物理内存」列
      （工作集，支持 pmem 排序）；「名称」列占两列宽（220px）。
- [x] **详情面板可复制**（7f92b0a）— 内容改只读 TextInput（拖选 + Ctrl+C 复制选中），
      标题栏「复制」按钮全量写剪贴板（Win32 CF_UNICODETEXT）。
- [x] **sort_groups 性能优化**（7f92b0a）— 预计算聚合 key 后对索引排序，比较器不再
      重复 group_aggregate（O(N log N × C) → O(N × C)），循环置换就地重排。
- [x] **悬浮窗三列布局 + 暂停按钮**（e2236fa / 7728b9d）— 左 28% / 中剩余 / 右 96px，
      行高 16px 行距 5px；暂停按钮点击并入全窗口 TouchArea 坐标判断（z 层级实测无效）。
- [x] 全量测试 250 个（core 82 / ui 146 / service 17 / bin 5）全过，workspace 零警告。

## 未完成

> **P0（核心功能）已全部完成**，见上方"已完成"列表。以下为 P1–P4 待办（本会话已补齐 P1 右键菜单/点击穿透、P2 卡顿闪烁、P3 WAL 基础、P4 文档）。

### P1 — 交互功能
- [x] **系统托盘图标** — `tray-icon` 0.19 + `muda` 0.15：后台线程建托盘 + 最小 win32 消息循环（`GetMessageW`），菜单事件经 `MenuEvent::receiver()` 轮询后由 UI 1Hz tick 消费；菜单项：显示/隐藏悬浮窗、暂停/恢复、退出；左键单击托盘图标 = 显示/隐藏。启动失败不阻塞 GUI — `crates/ui/src/tray.rs`

### P2 — 高级功能
- [x] **任务栏嵌入** — 伪任务栏窗口（PLAN §3.5 Phase 1 方案）：Slint 第二个窗口 `Taskbar` 组件（横向窄条，无边框透明置顶），默认定位工作区底部中央（`SystemParametersInfoW(SPI_GETWORKAREA)`），可拖动到任务栏位置；`config.toml [ui] taskbar = true` 启用 — `crates/ui/src/taskbar.rs`
- [x] **配置热加载** — `notify` 6.1 监听 config.toml（父目录非递归）+ skins/ 目录（递归，缺目录兜底 `crates/ui/skins`）；事件经 `classify_change` 分类、150ms 防抖；UI tick 消费：config.toml 变更 → 重载配置（皮肤名变化时重载皮肤），skin.toml 变更 → 重载当前皮肤 — `crates/ui/src/hotreload.rs` + `crates/ui/src/lib.rs`
- [x] **通知弹窗** — Windows 原生气泡通知（`Shell_NotifyIconW` NIF_INFO，无需 AUMID/manifest）；`should_notify` 纯逻辑：开关 + 严重程度门槛（未知等级按最严格）+ 事件时间戳去重；检测到新的 Major/Critical 事件时弹出 — `crates/ui/src/notify.rs`

### P3 — 服务与部署（服务化架构重构）
- [x] **Windows 服务改造** — 独立 `find-stutter-service` crate（`crates/service/`），用 `windows-service` 0.8 注册 SCM 服务；服务循环：每秒 `collect → detect → touch_heartbeat → write_sample`，提供 `run` / `install` / `uninstall` / `start` / `stop` / `status` CLI 子命令；SCM 名 `FindStutter`
- [x] **GUI 改为只读模式** — `crates/ui/src/reader.rs` 新增 `DbReader`，1Hz 轮询 `stutter.db`（`latest_sample_summary` / `latest_heartbeat` / `latest_event` / `event_count_today`）；删除 `spawn_collector` 和 `Collector` 实例；UI 只跑只读连接
- [x] **服务健康检测** — `Logger::touch_heartbeat`（id=1 单行 UPSERT）+ `DbReader::poll` 推算 `ServiceHealth` (`Running` / `Stale` / `Stopped` / `NoDatabase`)；UI 顶部状态条：绿/黄/红配色 + 文字；暂停按钮在非 `Running` 时禁用
- [x] **WAL 并发读写** — `Logger` 端 `PRAGMA journal_mode=WAL` + `Reader` 端 `SQLITE_OPEN_READ_ONLY` + `PRAGMA journal_mode=WAL`，服务写、GUI 读互不阻塞
- [x] **P3 测试** — `find-stutter-service` 16 测试 + `find-stutter-ui` 30 测试（reader 健康检测 + overlay 格式化 + integration reader 端到端），全部通过
- [x] **GUI 自动启动服务** — `crates/ui/src/auto_start.rs` 启动 GUI 时检测后台服务：复用 `find-stutter-service status` / `start` 子命令；找不到 exe / 未注册 / 启动失败 都不阻塞 GUI 启动，仅写日志；7 个新单元测试，总测试数 118 — `crates/ui/src/auto_start.rs`
- [x] **GUI 启动时 UAC 自动提权安装/启动** — `crates/ui/src/elevate.rs` 新增 `ShellExecuteExW` + `"runas"` 同步提权调用；`auto_start.rs` 按 `status` 退出码（0=Running/1=Stopped/2=NotFound/3=Error）走不同路径，NotFound → 提权 `install-start` 一次完成 install+start，Stopped → 提权 `start`；UAC 拒绝/超时/shell 失败均不阻塞 GUI，仅写日志。`service` 端新增 `install-start` 子命令 + `status` 三档退出码 — `crates/ui/src/elevate.rs` + `crates/ui/src/auto_start.rs` + `crates/service/src/main.rs` + `crates/service/src/cli.rs`，UI 测试 30→38（+8）



### P4 — 代码质量
- [x] **移除 unused warnings** — 清理 `crates/core/src/logger.rs` 未使用的 `DateTime` 导入、`crates/ui/src/overlay.rs` 未使用的 `Duration` 导入；release 构建现已零警告
- [x] **config.toml 示例完善** — 添加所有字段的注释说明
- [x] **README.md** — 项目说明、使用方法、构建指南

## 本会话修复的「标记完成但实现有问题」项（UT 核对）

> 用户要求核对已标记完成功能的 UT 完备性。核对发现并修复如下：

1. **皮肤系统名存实亡** — `skins/default/skin.toml` 是嵌套 TOML（`[skin]`/`[window]`/`[text]`），而 `SkinConfig` 是扁平结构 → `load()` 永远解析失败 fallback 默认；且 `apply_metrics` 从未把皮肤颜色接到 Slint（`overlay.slint` 颜色硬编码）。修复：skin.toml 改扁平结构、`SkinConfig::load` 支持 CWD / exe 同目录 / workspace 源码三处查找、恢复 `overlay::parse_color`、`overlay.slint` 全部颜色/字号/尺寸参数化并由 `apply_metrics` 注入（新增 5 个 parse_color 测试）。
2. **`hotreload.rs` 未接入 lib.rs** — 代码含测试早已写好但从未编译（lib.rs 无 `pub mod hotreload;`）。修复：接入模块 + `run()` tick 消费事件 + `ConfigWatcher::disabled()` 降级构造 + skins 目录兜底；同时修掉首次编译暴露的 notify 6.1 API 错误（`Data(RenameMode)` → `Data(DataChange)`、`Path::to_ascii_lowercase` 不存在等）。
3. **`config_save_and_reload_roundtrip` 集成测试失败** — `Config::load` 有意把相对 db_path 解析为配置所在目录的绝对路径（防 SCM 服务写 System32），测试却断言 roundtrip 相等。修复：测试断言改为「load 后是绝对路径且指向配置文件目录」（与实现契约一致）。
5. **`window.rs` 无任何 UT** — 点击穿透的扩展样式位运算从未被测试。修复：抽 `toggle_transparent_style` 纯函数 + 4 个测试（置位/清位/幂等/零样式）。
6. **collector GPU 聚合无 UT** — `aggregate_gpu_utilization` 内联在 WMI 查询里。修复：抽纯函数 + 7 个测试（空/单/多引擎求和/封顶 100/缺失视为 0/全缺失/溢出饱和）。
7. **每次启动都弹 UAC（影响自动测试）** — GUI 启动必跑 `ensure_service_running`，服务未注册/停止时弹 UAC。修复：`FIND_STUTTER_SKIP_SERVICE` 环境变量（任意非空即跳过）+ `config.toml [ui] auto_start_service = false` 双重开关，命中时返回新变体 `AutoStartResult::Skipped`；env 相关测试用进程级 Mutex 串行化避免并发干扰。
8. **服务永远起不来 → GUI 显示「服务已停止」+ 数据无变化** — 两个叠加 bug：
   - `install.rs` 注册服务用 `launch_arguments: vec![]`（SCM 启动服务**不传任何参数**），但 `main.rs` 用 clap 必填 subcommand → SCM 无参拉起时 clap 报 usage 退出，服务进程瞬间死亡。修复：`Cli.command` 改 `Option`，无子命令 = SCM 服务模式 → `service::run_scm()`；diag log 的 `subcommand: None` 即此路径。
   - `Config::load` fallback 只查 binary 同目录（`target/release/config.toml` 不存在，config 在项目根）→ SCM 服务 CWD=System32 时加载失败用默认配置，db 写错位置。修复：从 binary 目录**逐级向上**找 config（target/release → target → 项目根）。
   - 顺带：主循环先 `collect()`（首次 WMI/COM 初始化卡数秒）再写心跳 → GUI 启动头几秒误判 Stopped/Stale。修复：心跳提到 `collect()` 之前写；`parse_with_base` 对相对 base 用 `current_dir` 绝对化（消除 "db_path 解析为绝对路径: stutter.db" 的假日志）。
   - `ensure_service_when_exe_missing` 原是环境耦合测试（假设机器上无 service exe），服务注册后必失败。修复：拆出 `ensure_service_running_with_exe(exe, db)` 可注入版本，测试传 `None` 断言 `ExeNotFound`，与真实环境彻底隔离。

## P5 — 检测精度优化（贴近真实卡顿体验）— 2026-08-09

> 背景：当前检测器把「资源活动突增」当「卡顿」。今日（08-09）13 次卡顿
> 100% 由网络 spike 触发（下载/同步流量突增），但当时 CPU 仅 19–74%、内存
> 44–67%，并不高；磁盘判据用「吞吐量 B/s」而非「繁忙度 %」，SSD 写 130MB/s
> 根本不卡。目标：让「卡顿」更贴近真实卡顿体验（系统无响应 / 界面冻屏），
> 并**记录造成卡顿的进程信息**。
>
> 设计参考：早期 `PLAN.md` §3.3 / §5 已设想 `detect_ui_freeze`（向前台窗口
> 发消息超时判冻结）与 `cpu_spike_min_baseline` 等，本次据实落地并扩展。

### A — 去网络误报（最小改动，先止血）
- [ ] `DetectionConfig` 新增 `enable_network_spike: bool`（默认 false）：关闭后
      Network spike 不再作为卡顿触发源（`detector.rs check_spike` 跳过 net 分支），
      从源头消除「下载被当卡顿」这类误报。
- [ ] `config.toml [detection]` 增加 `enable_network_spike` 字段 + 中文注释。
- [ ] `spike_min_bps` 默认值由 2MB/s 上调至 10MB/s，减少零头波动误报。
- [ ] 单测：`enable_network_spike=false` 时网络 spike 不触发卡顿。

### B — 指标升级（直击根因）
- [ ] `collector.rs` 新增 PDH 计数器（复用 `DiskPdh` 模式，新增 `SystemPdh`）：
      - `\PhysicalDisk(_Total)\% Disk Time`（磁盘繁忙度，替代 B/s 吞吐）
      - `\PhysicalDisk(_Total)\Avg. Disk sec/Transfer`（单次 IO 延迟）
      - `\Processor Information(_Total)\% DPC Time`（系统底层卡顿信号）
      - `\Processor Information(_Total)\% Interrupt Time`
      - `\System\Context Switches/sec`（上下文切换风暴）
- [ ] `Sample` 增加字段：`disk_busy_percent`、`disk_avg_io_ms`、`dpc_percent`、
      `interrupt_percent`、`context_switches_per_sec`。
- [ ] `detector.rs`：用 `disk_busy_percent > 95` 或 `disk_avg_io_ms > 50` 替代
      磁盘 B/s spike；新增 DPC/中断/上下文切换作为「系统级卡顿」cause（带阈值 + 滞回）。
- [ ] `config.toml [detection]` 增加对应阈值字段（默认合理值）与中文注释。
- [ ] 单测覆盖新 cause 的触发与滞回。

### C — 进程 culprit 记录（新需求）— 已实现（代码 + 单测通过）
卡顿事件记录「造成卡顿的进程信息」，便于事后定位元凶。
- [x] `collector.rs` 每 tick 采集 top 进程快照：sysinfo `Process::cpu_usage()` /
      `Process::memory()`（bytes→MB）；取 CPU top8 + 内存 top8 去重合并最多 12 个，
      存入 `Sample.top_processes`（`ProcessBrief` 列表）。磁盘/网络 per-process IO 仍进阶。
- [x] 新增 `ProcessBrief` 结构（替代计划的 `ProcessCulprit`）：`pid: u32`、`name: String`、
      `cpu_usage: f32`、`mem_used_mb: u64`（同时带 CPU/内存双维度用量，比单 `value` 更直观）。
- [x] `StutterEvent` 增加 `culprits: Vec<ProcessBrief>`；`Sample` 增加 `top_processes`。
- [x] `stutter_events` 表 `Logger::new` 内 `ALTER TABLE ... ADD COLUMN culprits TEXT`
      （旧库自动迁移；忽略「列已存在」错误）；`write_event` 写入 culprits JSON；
      `reader.rs` 读取并反序列化进 `StutterEvent.culprits`。
- [x] `detector.rs`：卡顿持续期间用 `current_culprits: HashMap<u32, ProcessBrief>` 累积
      top 进程（同 pid 取最大 CPU/内存用量），结束时 `extract_culprits` 取 CPU top3 +
      内存 top3 去重（≤6 个）作为 culprits。
- [x] **用户可见透出**：`notify.rs` 的卡顿气泡通知新增「元凶进程」行（top3，格式
      `name (CPU%, MB)`），用户收到卡顿提醒时直接看到是谁造成的。
- [x] 单测：`detector` 卡顿记录 culprit（CPU top 排第一）、空 top 不 panic；
      `logger` culprits 落库回读；`reader` poll 读回 culprits（152 UI 测试全过）。

### 部署约束
- 检测逻辑跑在 `find-stutter-service.exe`；改完后**需管理员停服重装才生效**。
- 已提供 `upgrade-service.ps1` 一键提权升级脚本（自提权 UAC → `sc stop` 释放 exe 锁
  → `rtk cargo build --release` → `find-stutter-service.exe install-start` → 校验
  RUNNING）；支持 `-NoBuild` 只重启。详见 `UPGRADE.md`。
- 架构保持不变（按用户要求：**不**让 GUI 双采集，检测只在 service 进程跑）。
