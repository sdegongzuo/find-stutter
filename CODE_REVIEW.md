# find-stutter 代码评审与优化建议

> 范围：`crates/` 下全部 crate（core / service / ui / bin），以"评估"为目的，未做任何改动。
> 结论先行：**整体质量很高**——分层清晰（采集 / 检测 / 存储 / 只读 UI）、检测逻辑有滞回 + 连续确认 + 绝对下限等多重防误报设计、单测覆盖充分。下面只列可改进/可优化点，按收益排序。

---

## 一、性能优化（热路径：每秒一次）

### 1. 【高收益】`Collector::collect()` 每个 tick 都全量扫描所有进程构建 `top_processes`
`crates/core/src/collector.rs:208` 在 `collect()` 里**无条件**调用 `Self::collect_top_processes(&self.sys)`，它会：
- 遍历 `sys.processes()` 全部进程（常驻几百个）；
- 为每个进程 `clone` 名称、构造 `ProcessBrief`；
- 按 CPU、内存各 `sort_by` + 截取去重。

但 `Sample.top_processes` 只在**卡顿进行中**才会被 `Detector` 累积为 culprits（`detector.rs:56`，仅 `!causes.is_empty()` 时），且只有 `write_event`/`snapshot` 会落库（`write_sample` 根本不写 `top_processes`）。也就是说：**平时 99% 的 tick 里，这整份进程快照算完即丢。**

**建议**：`collect()` 增加 `collect_culprits: bool` 入参，由 `service/src/service.rs:128` 的主循环传入 `detector.is_active()`（或新加一个 `Detector::stutter_active()` 访问器）。仅在卡顿激活时构建全量 top。这是热路径上最值得做的一处优化。

### 2. 【中收益】`collect()` 每 tick 调用 `sys.refresh_all()`
`crates/core/src/collector.rs:148` 每 tick `refresh_all()`（CPU+内存+进程+磁盘+网络全套刷新）。对 1Hz 监控可接受，但 `refresh_all` 相对昂贵。可改为按维度细粒度刷新（如 `refresh_cpu_specifics` + `refresh_memory` + `refresh_processes(ProcessesToUpdate::All, true)`），把不需要的（如磁盘列表、组件列表）剔掉。`DiskPdh`/`WMI` 走的是独立通道，不依赖 sysinfo 这部分。属于"能省则省"，改动需小心回归。

### 3. 【中收益】`collect_wmi_slow()` 每 5 tick 新建 `WMIConnection`
`crates/core/src/collector.rs:256` 每个慢通道 tick 都 `WMIConnection::new()`（内部初始化 COM + 建连）。慢通道 5s 一次，开销不大，但 `WMIConnection` 设计为可复用——可考虑把连接缓存在 `Collector` 字段里（注意多线程/跨 tick 安全性，必要时加 `Mutex`）。属小优化。

### 4. 【低收益】`Detector::history` 用 `Vec` + `remove(0)` 截断
`crates/core/src/detector.rs:46`，`history.len() > 120` 时 `history.remove(0)` 是 O(n) 整体前移。120 个元素在 1Hz 下开销可忽略，但语义上更适合 `VecDeque`（环形，O(1) 弹出队首）。纯风格/微优化。

### 5. 【低收益】`check_spike()` 每 tick 克隆多份 `Vec<f32>`
`crates/core/src/detector.rs:181-228` 每次从 `history` 切片克隆出 `cpu_r/cpu_b/disk_r/disk_b/...` 共 4 份 `Vec<f32>` 再求均值。可直接在切片上计算均值/计数，省掉分配。1Hz 下无所谓，但顺手能改。

### 6. 【中收益】`DbReader::poll()` 每 tick 反序列化整个 `StutterEvent`（含 `snapshot` JSON）
`crates/ui/src/reader.rs:172-208` 为了显示"上次卡顿闪烁"提示，每 tick `SELECT ... snapshot, culprits` 并 `serde_json::from_str` 反序列化完整 `Sample`（含 `cpu_per_core` 数组等）。overlay 实际只用了 `event.timestamp`。**建议**：可只取 `timestamp, duration_ms, severity, causes` 三列反序列化（或单独存一个 `last_event_at` 轻量字段），省掉每 tick 的大 JSON 解析。

---

## 二、死代码 / 可清理

### 7. 【中】`crates/core/src/service.rs` 整文件是死代码（重复的 service loop 实现）
core crate 通过 `crates/core/src/lib.rs:4` 暴露 `pub mod service;`，其中 `run_service()` / `service_loop()` / `RUNNING` / `define_windows_service!(ffi_service_main, ...)` 全都没人调用——实际服务二进制用的是 `crates/service/src/service.rs`（`run_scm` / `run_foreground`）。`grep` 确认 `run_service` 仅出现在 `core/src/service.rs` 自身。
**建议**：删除 `crates/core/src/service.rs` 及 `lib.rs` 里的 `pub mod service;`。它不仅是冗余，还把一个 FFI service-main 宏注册在了库里（虽然永不被 SCM 派发），容易误导后续维护者。

### 8. 【低】`ProcessListWindow` 有多余的 `#[allow(dead_code)]` Arc 字段
`crates/ui/src/process_list.rs:1649-1675`：`sampler` / `detail_text` / `_refresh_ms` / `cache_version` 标注为"仅作 Arc 持有者"，字段本身从不读取。但这些 Arc 的**后台采样线程已持有自己的 clone**（`sampler_thread`、`detail_text` 等），所以窗口结构体保留这些字段是多余的，徒增结构体体积与阅读困惑。**建议**：删掉这些字段（线程已保证共享状态存活），只保留真正需要持有以防 drop 的 `_timer`。

### 9. 【低】`Sample.thread_count` 与 `Sample.gpu_temp` 永远是 0 / None
`crates/core/src/collector.rs:230` 写 `thread_count: 0`，`gpu_temp: None` 也从未赋值。两者都落库成 `samples` 表的列（`logger.rs:41, 39`）。属于**永远写空值的死列**：
- 要么删掉字段 + ALTER 删列（需迁移旧库，类似 `culprits` 的迁移手法）；
- 要么填上真实值（`thread_count` 可在 `collect_top_processes` 里顺手累加，代价是又要遍历进程，故更建议直接删除）。

### 10. 【低】`find-stutter` CLI 的 `Run` 子命令与默认分支完全重复
`crates/bin/src/main.rs:43` 默认分支直接跑 GUI，`Commands::Run`（`crates/bin/src/lib.rs:13`）也跑 GUI，二者行为一致。若保留 `Run` 仅为显式语义可加注释说明，否则可去掉 `Run` 子命令。

---

## 三、正确性与健壮性小点

### 11. 【无害】`write_sample` 与 service 主循环双重 flush
`crates/core/src/logger.rs:77` 缓冲满 10 条或 5s 自动 flush；同时 `crates/service/src/service.rs:148` 每 10 tick 又调一次 `flush()`。二者在 1Hz 下同一时刻触发，第二次为 buffer 已空的空操作——无害，可只在主循环统一 flush 并去掉 `write_sample` 内的自动 flush（或反之），逻辑更清晰。

### 12. 【待验证】`windows = "0.62"` 与 `windows-service = "0.8"` 版本对齐
`windows-service 0.8` 依赖的 `windows` 版本可能与直接指定的 `0.62` 不完全一致，导致 **`windows` crate 被编译两份**（不同版本），增大二进制与编译时间。建议在 `Cargo.lock` 中确认是否出现两个 `windows` 版本；若如此，可统一对齐（如让 `windows-service` 使用的 windows 版本与直接依赖一致，或用 `[patch]`）。同样，`rusqlite` 的 `bundled`（SQLite 源码）在 core 与 ui 两个 crate 各编译一份——这是分 crate 的常态，除非抽成共享，否则不必动。

### 13. 【低】`Config::load` 失败回退链较复杂
`crates/core/src/types.rs:371-410` 的查找顺序（给定路径 → binary 同目录 → 逐级向上 → 原路径报错）逻辑正确且对 SCM 场景必要，但可读性与"最后再试一次原路径"的兜底略绕。可在注释/结构上稍作整理（非必须）。

---

## 四、值得肯定的地方（不改）

- **检测算法**设计扎实：CPU/Swap 滞回、spike 的"只认突增 + 绝对下限 + 连续确认(≥6/10) + 滞回解除"，并配套了 `cause_key` 同类型去重，避免 `current_causes` 膨胀虚高 severity。单测覆盖各分支（含滞回带、骤降不误报、零星尖峰不触发）。
- **P3 服务化 + WAL 只读**架构干净：采集/UI 彻底解耦，GUI 不阻塞在采集上；reader 用 `OPEN_READ_ONLY | OPEN_NO_MUTEX` 复用连接。
- **Windows 原生坑**处理得很专业：`ensure_tool_window` 对抗 winit 重算扩展样式、`TrackPopupMenu` 模态后 `reset_mouse_state_after_menu` 复位鼠标状态机、UAC 提权 `taskkill`/explorer 用 `ShellExecuteW` 直传 UTF-16 避免引号转义——都是实战踩坑的沉淀。
- **进程详情页**采样/渲染分离（`process-sampler` 后台线程 + 共享 `Arc<Mutex<Vec<ProcessRow>>>` cache）、聚合排序预计算 key 避免 O(N log N × 子进程数)，细节到位。

---

## 五、优先级建议

| 优先级 | 项 | 收益 | 风险 |
|--------|----|------|------|
| P1 | #1 卡顿激活时才采集 top_processes | 高（每秒省一次全进程扫描+双排序） | 低（加一个 bool 入参） |
| P1 | #7 删除 `core/src/service.rs` 死代码 | 中（消除歧义/重复实现） | 低 |
| P2 | #6 `poll()` 只取需要的事件字段 | 中 | 低 |
| P2 | #9 处理 `thread_count`/`gpu_temp` 死列 | 低（库体积） | 中（需 DB 迁移） |
| P3 | #2 细粒度 refresh | 中 | 中（回归风险） |
| P3 | #3/#4/#5 小优化 | 低 | 低 |
| P3 | #8/#10/#11/#12 清理项 | 低 | 低/待验证 |

> 一句话总结：**最大的性价比在 #1（热路径关掉无谓的全进程快照）和 #7（删死代码）；其余多为锦上添花或低风险清理。**
