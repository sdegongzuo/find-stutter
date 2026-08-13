# 卡顿根因分析 — 产品需求文档（PRD）

> 版本：v0.3（软件根因定位 + 结论落库）
> 起草日期：2026-08-11
> 修订日期：2026-08-13
> 状态：v0.2 已实施（F-RC1~F-RC13 分析层已落地，提交 c983950/3c93df7）；v0.3 待评审实施
> 关联文档：`卡顿分析界面-PRD.md`（F1–F8）、`TODO.md`（§P5 检测精度优化）、
> `crates/core/src/types.rs`、`crates/core/src/detector.rs`、`crates/core/src/collector.rs`、
> `crates/core/src/logger.rs`、`crates/ui/src/analytics.rs`、`crates/ui/src/analysis.rs`

---

## 1. 背景与目标

### 1.1 现状

v0.2 已实施：`cause_kinds` / `primary_cause` / `cause_first_touch` / `onset_ts` 结构化落库，
F-RC5~F-RC13 归因算法（加权主因、因果方向、基线偏离、共现、因果链、置信度、what-if、
画像）全部落地（提交 c983950 / 3c93df7）。至此根因已走到**「资源信号级因果链」的最末端**：

- 能回答「哪个资源指标先异常、谁是主因、谁是放大器」（如 `MemLow → DiskBusy(Paging) → 卡顿`）；
- 但**仍回答不了「是哪个软件 / 驱动 / 模块把它搞卡的」**——`culprits` 只有进程名与
  CPU/内存占比，没有可执行文件完整路径、句柄/GDI 对象数、进程级 IO、已加载模块、调用栈；
- Windows 事件日志（显卡驱动 TDR 超时、服务崩溃、磁盘坏块、WHEA 硬件错误）**完全未采集**，
  而它们恰恰是「软件 / 驱动 / 硬件级根因」最直接的证据；
- **分析结论（主因、因果链、置信度、软件根因）不落库**——F-RC5~F-RC13 的结论都在
  GUI 内存中临时计算（受 §1.4 原非目标「不把分析页改回可写 stutter.db」约束），
  下次打开会重算，算法版本一改结论就漂移，**无法回溯、无法审计**。

### 1.2 痛点

v0.2 之后一句话：**现在能回答「哪个资源指标先动、谁是资源级主因」，但仍回答不了
「是哪个软件 / 驱动 / 模块在作祟」，而且算出来的结论不落库、查不到历史判断。**
用户要的是能落到「元凶软件」的根因，现有数据/算法给的是「资源级因果 + 无法回溯的临时结论」。

### 1.3 目标

分三层补齐因果链（v0.2 已完成第 1、2 层，v0.3 新增第 3 层）：

1. **检测/数据层（service 侧，写库）**——让根因有干净、结构化的数据基础：
   结构化 `CauseKind` 枚举、真实磁盘繁忙度与系统级信号、前台冻结检测、温度降频根因。
2. **归因逻辑层 + 呈现层（GUI 只读侧）**——在现有分析窗口上做深度归因：
   主因加权、因果方向、基线偏离、多进程共现、因果链，并以"单事件根因钻取卡"呈现。
3. **软件根因定位层 + 结论落库层 + 呈现层（v0.3 新增）**——把因果链从「资源指标」延伸到
   「元凶软件/驱动/模块」，把分析结论持久化，并在 UI 呈现：
   - **软件根因定位**：进程指纹（完整路径/句柄/GDI 对象）、进程级 IO 与已加载模块、
     Windows 事件日志（驱动 TDR / 服务崩溃 / 磁盘坏块 / WHEA）、ETW CPU 调用栈热点；
   - **结论落库**：把「主因 + 因果链 + 置信度 + 软件根因」写入独立结论表，可回溯、可审计；
   - **UI 呈现**：钻取卡新增「软件根因」区块（元凶进程 / 事件日志命中 / 调用栈热点），
     主因「软件级优先」，并支持结论回溯对比（F-RC16）。

### 1.4 非目标（明确不做）

- 不改检测阈值生产逻辑（F-RC12 what-if 仅客户端用存储的 `snapshot` 信号值**模拟**，不写 service）。
- 不跨机器聚合。
- **（v0.3 修订）不再一刀切禁止 GUI 写库，而是把写权收窄到「结论表」**：`samples` /
  `stutter_events` / `service_heartbeat` 等原始采集表仍严格只读（P3 契约不变）；GUI 仅对
  新增的 `root_cause_reports` 结论表持有写权（见 F-RC15）。基线/共现等派生数据仍内存计算，
  **只有最终分析结论落库**。
- 不做完整 PDB 符号化：调用栈采样只解析到「模块名 + RVA 偏移」级别（见 F-RC14-d），
  不引入 dbghelp + 符号服务器做函数名精确定位。
- 不做持续全量调用栈采集：ETW 采样仅在卡顿触发后限频进行（见 F-RC14-d），不进采集热路径。

### 1.5 评审修订说明（v0.2 于 2026-08-12 对照代码核实；v0.3 于 2026-08-13 追加）

本版（v0.2）对照 `types.rs` / `detector.rs` / `collector.rs` / `logger.rs` 实际代码做了修订，
纠正下列前提性错误，避免 M2 实现卡壳或给出错误结论：

1. **F-RC7 基线数据前提**：`collector.rs` 中**非卡顿样本 `top_processes` 为空**（非卡顿帧跳过进程构建），
   故不能用非卡顿 `samples.top_processes` 算基线；基线改为从**事件侧**聚合（历史事件 `culprits` / `snapshot.top_processes`）。
2. **严重度 ≠ 独立权重**：`detector.rs determine_severity` 按 `causes.len()` 定级（1→minor/2→major/3→critical），
   故 F-RC5 权重**不乘 severity**（会重复计数 cause 数），改为 `duration × 主因信号强度`。
3. **F-RC6 阈值前提**：spike 类 cause 是**滑动基线比率 + 滞回**（`spike_check`），非静态阈值；
   改为 detector 落库时记录**各 cause 首触时刻 / 事件 onset 时刻**，分析侧复用，避免复刻数学漂移。
4. **F-RC3 阻塞风险**：`SendMessageTimeout(WM_NULL, 500ms)` 每 tick 调用会腰斩 1Hz 采样；
   改为"仅在已触发其它 cause 的帧探一次"或独立低优先线程 + 200ms 超时 + 每 2s 限频。
5. **F-RC4 数据源**：`gpu_temp` 从未填充（`collector.rs:437`），用 `cpu_temp` + `cpu_freq_mhz` 掉档判据替代；
   `page_reads_per_sec`（真实 swap 卡顿信号）已存在，F-RC2 不再重复造词。
6. **F-RC1 复用 `cause_key()`**：`detector.rs cause_key()` 已把 cause 文本映射到稳定类型 key，
   `CauseKind` 枚举直接对齐这套 key；旧事件 `cause_kinds` 空时可用 `cause_key` **可靠回填**（消除 R2「分类不连续」风险）。
7. **F-RC10 前置依赖（已解决）**：`StutterEvent` Rust 结构（`types.rs`）已带 `id: i64`（v0.2 修订后补齐），
   reader 可正常携带事件主键，钻取关联已无阻塞。
8. **置信度定义修正（F-RC11）**：多因并发是 major/critical 的**定义本身**，低置信会误伤所有高级事件；
   改为看"主因是否明显领先其余 cause（强度/时间差）"。
9. **已知缺口（后续扩展）**：本 PRD 缺「根因纵向趋势」（某根因/进程是否随时间恶化），可与「周期规律热图」呼应，留待二期。

**v0.3 追加修订（2026-08-13，同样对照当前代码核实）**，纠正/明确下列前提：

10. **`ProcessBrief` 现状**：当前 `types.rs` 的 `ProcessBrief` 仅含 `pid/name/cpu_usage/mem_used_mb`
    四字段（`collector.rs` 用 `sysinfo::Process` 的 `name()/cpu_usage()/memory()` 构建，见
    `collect_top_processes`）。软件根因定位需**新增字段**（`exe_path`/`handle_count` 等），
    `sysinfo` 的 `process.exe()` 可拿路径、`process.threads()` 可拿线程数；**句柄数 / GDI/USER
    对象数 sysinfo 不直接提供**，需 `GetProcessHandleCount` / `GetGuiResources` 补充，且均有
    一次性的进程打开开销，只应在「卡顿帧」限频采集（对齐 `collect_with(true)` 的既有节制）。
11. **事件日志读取走 `windows` crate 的 `Win32::System::EventLog`**（`OpenEventLogW`/
    `ReadEventLogW`），非 WMI——比 WMI 查询轻、能按事件 ID/Provider 过滤；读取时机为
    **卡顿事件生成时回溯** `[onset-30s, now]` 窗口，而非常驻监听（避免热路径开销）。
12. **ETW 调用栈采样是「重」能力，必须独立线程 + 限频 + 可降级**：service 以 SYSTEM 权限
    运行已满足 ETW 权限前提；但 Rust 侧需直接调 `Win32::System::Diagnostics::Etw`
    （`StartTrace`/`EnableTraceEx2`/`OpenTrace`/`ProcessTrace`）解析 `SampledProfile`+`StackWalk`
    事件，实现成本高，**列为 F-RC14-d 的独立里程碑 + 降级路径**（见 §9 M5）。
13. **结论落库改变写权边界**：`analytics.rs` 现有 `open_readonly` 以 `SQLITE_OPEN_READ_ONLY`
    打开。F-RC15 需**新增一条可写连接**，且只对 `root_cause_reports` 表做 INSERT/UPSERT；
    `ensure_indexes` 已有「只读连接建索引失败优雅降级」的先例，结论表索引须在**首次可写
    连接**时创建（或由 service 建表，见 F-RC15）。

---

## 2. 与 `卡顿分析界面-PRD.md` 的关系

本 PRD 是 F1–F8 的**根因深化扩展**，不是另起炉灶：

- **复用基础设施**：分析窗口（`analysis.slint` / `analysis.rs`）、plotters→Image 图表、
  后台线程渲染、像素降采样、本地时区分桶、`idx_events_ts` / `idx_samples_ts` 索引。
- **条目编号**：本 PRD 功能统一以 `F-RCn` 编号（RC = Root Cause），与 F1–F8 并列，避免混淆。
- **依赖边界**：`F-RC1~F-RC4`、`F-RC14` 属 service 侧改造，需改检测器/采集器/schema 并重装服务
  （`TODO.md` §P5 部署约束）；`F-RC5~F-RC13`、`F-RC16` 在 GUI 只读侧消费现有/新增字段与软件根因表；
  `F-RC15` 在 GUI 侧新增一条「只写结论表」的窄写权连接（原始表仍只读）。

---

## 3. 数据模型变更

> 以下字段均建立在 `crates/core/src/types.rs` 现有结构上，落地需协同 `TODO.md` §P5。

### 3.1 `StutterEvent` 新增（检测/数据层 F-RC1）

| 字段 | 类型 | 说明 | 来源 |
| --- | --- | --- | --- |
| `cause_kinds` | TEXT(JSON 数组) | 结构化根因枚举，如 `["CpuHigh","MemLow"]` | `detector.rs` 输出 |
| `primary_cause` | TEXT(NULL) | 主因枚举（多因同发时按信号强度排序取第一） | `detector.rs` 排序 |
| `cause_first_touch` | TEXT(JSON 对象, NULL) | 各 cause 的「首触时刻」`{CpuHigh: t1, MemLow: t2}` | `detector.rs` 记录 |
| `onset_ts` | INTEGER(NULL) | 事件 onset 时刻（≈ `t - sustained_seconds`，即真实卡顿起点） | `detector.rs` 记录 |
| `id` | INTEGER(PK) | ✅ 已落地（v0.2 修订后补齐）：`StutterEvent` 已带 `id: i64`，reader 可正常携带主键 | 已有主键 |

- **旧库 `cause_kinds` 为空时的回填**：用 `detector.rs cause_key()` 把 `causes` 文本**可靠映射**为枚举
  （`cause_key` 已稳定映射 CPU usage / Available memory / Memory paging / Network spike / Disk spike…），
  这是基于现有代码的精确映射，**非脆弱关键词猜测**；`causes` 文本列保留作最终兜底。
- 迁移：`ALTER TABLE stutter_events ADD COLUMN cause_kinds TEXT;`（忽略"列已存在"）；`ADD COLUMN primary_cause TEXT;`；
  `ADD COLUMN cause_first_touch TEXT;`（可选，缺则分析侧回退到 §5.2 重算）；`onset_ts` 可由 `timestamp - 3s` 推导亦可落库；
  `reader.rs` 列存在性探测（已有先例）。

### 3.2 `Sample` 新增（协同 P5-B，F-RC2）

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `disk_busy_percent` | REAL(NULL) | `\PhysicalDisk(_Total)\% Disk Time` 磁盘繁忙度 |
| `disk_avg_io_ms` | REAL(NULL) | `Avg. Disk sec/Transfer` 单次 IO 延迟(ms) |
| `dpc_percent` | REAL(NULL) | `% DPC Time` 系统底层卡顿信号 |
| `interrupt_percent` | REAL(NULL) | `% Interrupt Time` |
| `context_switches_per_sec` | INTEGER(NULL) | `\System\Context Switches/sec` |

- 迁移：`ALTER TABLE samples ADD COLUMN ...`（旧库回退 N/A，UI 不绘制）。
- **范围澄清**：`Memory paging`（`page_reads_per_sec`，`types.rs` 已标注为真实 swap 卡顿信号）**已在采**，
  F-RC2 不重复覆盖——本次只补「磁盘真繁忙度 + DPC/中断/上下文切换」。
- `top_processes` **不再用于基线**（见 F-RC7 修订：非卡顿样本 `top_processes` 为空，基线改从事件侧聚合）。

### 3.3 `CauseKind` 枚举（F-RC1 定义，检测端产出；v0.3 扩展软件级 cause）

```
// 资源级（v0.2 已落地，对齐 cause_key()）
CpuHigh | CpuSpike | MemLow | DiskBusy | DiskSpike | GpuHigh
ThermalThrottle | DpcInterrupt | InterruptStorm | ContextSwitchStorm
NetSpike | UiFrozen
// 软件/驱动/硬件级（v0.3 F-RC14 新增）
ProcessHandleLeak | GdiObjectLeak | DriverTimeout
ServiceCrash | DiskIoError | HardwareError
```

- 枚举值**直接对齐 `detector.rs cause_key()` 现有稳定 key**，不得臆造（消除 R2「分类不连续」风险）。
  `CpuSpike` 已按 `cause_key()` 实际产出的 `"CPU spike"` key 补齐（v0.2 首版清单漏列，与代码不一致）。
- **软件级 cause（v0.3 新增）来源不是 PDH 阈值**，而是「进程指纹阈值 + Windows 事件日志回溯」，
  检测路径独立于 `check_hard_thresholds` / `check_spike`（见 F-RC14）。
- `ThermalThrottle` 数据源为 `cpu_temp` + `cpu_freq_mhz` 掉档判据（`gpu_temp` 从未填充，不纳入）。
- `UiFrozen` 为 v0.2 新增 cause。`NetSpike` / `DiskSpike` 等沿用 `cause_key` 现有 key。
- 序列化用字符串（便于旧库回读与新库直接 `GROUP BY`）。
- **软件级 cause 主因排序（v0.3）**：多个软件级 cause 同时命中时，按严重程度排序取第一：
  `HardwareError` > `DriverTimeout` > `ServiceCrash` > `DiskIoError` > `ProcessHandleLeak` > `GdiObjectLeak`。
  软件级 cause 整体优先于资源级 cause（见 §5.6）。

### 3.4 软件根因定位 + 结论落库的数据模型（F-RC14 / F-RC15，v0.3 新增）

#### 3.4.1 `ProcessBrief` 扩展（F-RC14-a / F-RC14-b）

| 字段 | 类型 | 说明 | 来源 |
| --- | --- | --- | --- |
| `exe_path` | TEXT(NULL) | 可执行文件完整路径 | `sysinfo process.exe()` |
| `handle_count` | INTEGER(NULL) | 句柄数（泄漏信号） | `GetProcessHandleCount` |
| `gdi_objects` | INTEGER(NULL) | GDI 对象数（GUI 泄漏经典信号） | `GetGuiResources(GR_GDIOBJECTS)` |
| `user_objects` | INTEGER(NULL) | USER 对象数 | `GetGuiResources(GR_USEROBJECTS)` |
| `io_read_bps` | INTEGER(NULL) | 该进程磁盘读速率 B/s | `GetProcessIoCounters` 差分 |
| `io_write_bps` | INTEGER(NULL) | 该进程磁盘写速率 B/s | 同上 |

- 落库：`culprits` JSON 随 `ProcessBrief` 结构扩展自动落库（新增字段加 `#[serde(default)]`，
  旧 JSON 反序列化不崩，对齐 `Sample` 既有 F-RC2 字段的兼容做法）。
- 采集时机：仅在 `collect_with(true)`（卡顿帧）填充，非卡顿帧跳过（对齐 `top_processes` 既有节制）。
- 价值：把 culprit 从「浏览器.exe」升级为「`C:\...\browser.exe` + 句柄 12 万 + 磁盘读 80MB/s」，
  直接支撑「句柄泄漏 / 磁盘狂读写」两类软件级根因判定。

#### 3.4.2 独立表 `process_modules`（F-RC14-b 已加载模块）

| 列 | 类型 | 说明 |
| --- | --- | --- |
| `id` | INTEGER PK | |
| `event_id` | INTEGER | → `stutter_events.id`（关联到卡顿事件） |
| `pid` | INTEGER | 进程 ID |
| `process_name` | TEXT | 进程名 |
| `module_path` | TEXT | 模块完整路径（如 `C:\Windows\System32\foo.dll`） |
| `module_size` | INTEGER | 模块大小（字节） |

- 采集时机：卡顿事件生成时，对每个 culprit 进程 snap 一次已加载模块列表
  （`Toolhelp32Snapshot` + `Module32First/Next`）。
- 价值：识别注入的可疑 DLL / 第三方驱动模块，是「软件根因」的直接证据。

#### 3.4.3 独立表 `windows_events`（F-RC14-c 事件日志回溯）

| 列 | 类型 | 说明 |
| --- | --- | --- |
| `id` | INTEGER PK | |
| `event_id` | INTEGER | → `stutter_events.id`（关联到卡顿事件） |
| `channel` | TEXT | 日志通道（`System` / `Application`） |
| `provider` | TEXT | 事件源（如 `Display` / `disk` / `Service Control Manager` / `Microsoft-Windows-WHEA-Logger`） |
| `win_event_id` | INTEGER | Windows 事件 ID（如 4101 / 7 / 51 / 7031 / 41） |
| `level` | TEXT | 级别（Error / Warning） |
| `message` | TEXT | 事件消息（截断到 512 字符，防膨胀） |
| `ts` | TEXT | 事件发生时刻（RFC3339） |

- 采集时机：卡顿事件生成时回溯 `[onset-30s, now]` 窗口，仅抽**高价值白名单事件 ID**：
  `disk`(7 坏块/51 分页错误/153 IO 超时)、`Display`(4101 TDR 驱动重置)、`Kernel-Power`(41 意外断电/崩溃)、
  `Service Control Manager`(7031/7034 服务意外终止/崩溃)、`Microsoft-Windows-WHEA-Logger`(硬件错误)。
- 价值：直接点出「显卡驱动 TDR 超时」「磁盘坏块」「某服务崩溃」「硬件错误」，
  这些是「软件 / 驱动 / 硬件级根因」最直接的证据，且读日志成本远低于持续采样。

#### 3.4.4 独立表 `stack_samples`（F-RC14-d ETW 调用栈热点）

| 列 | 类型 | 说明 |
| --- | --- | --- |
| `id` | INTEGER PK | |
| `event_id` | INTEGER | → `stutter_events.id` |
| `pid` | INTEGER | 进程 ID |
| `process_name` | TEXT | 进程名 |
| `module` | TEXT | 热点模块名（exe / dll） |
| `rva` | INTEGER | 模块内相对偏移（RVA） |
| `sample_count` | INTEGER | 该 `(process, module, rva)` 热点采样命中次数（聚合后） |

- 采集：卡顿触发后由独立低优先级线程经 ETW `NT Kernel Logger` 的 `SampledProfile` +
  `StackWalk` 事件采样，聚合为热点计数；**不落原始栈帧序列**，只落聚合热点。
- 符号化：只解析到「模块名 + RVA 偏移」级别（§1.4 非目标：不做完整 PDB 符号化）。

#### 3.4.5 结论表 `root_cause_reports`（F-RC15 分析结论落库）

| 列 | 类型 | 说明 |
| --- | --- | --- |
| `id` | INTEGER PK | |
| `event_id` | INTEGER UNIQUE | → `stutter_events.id`（一事件一条结论，UPSERT） |
| `algorithm_version` | TEXT | 分析算法版本（回溯「结论为何变」） |
| `primary_cause` | TEXT | 主因枚举 |
| `confidence` | REAL | 置信度 0..1 |
| `cause_chain` | TEXT(JSON) | 因果链（`CauseKind` 枚举数组，F-RC9 结果） |
| `software_root_cause` | TEXT(JSON) | 软件根因定位结论（进程/模块/事件 ID 摘要，F-RC14 结果） |
| `baseline_delta` | TEXT(JSON) | 偏离基线摘要（F-RC7 结果） |
| `computed_at` | TEXT | 计算时刻（RFC3339） |

- 写权：GUI 新增**可写连接**，仅对该表做 `INSERT OR REPLACE`（按 `event_id` UPSERT）；
  `samples` / `stutter_events` / `service_heartbeat` 原始表仍严格只读。
- 建表：由 service 在建表批次一并 `CREATE TABLE IF NOT EXISTS`（与 `samples` 同批，见 §F-RC15）。
- 价值：分析结论可回溯、可审计，算法版本升级后可对比「上次判 A 是主因，这次为何变 B」。

#### 3.4.6 数据保留策略（v0.3）

新增的 4 张软件根因表均通过 `event_id` 外键关联 `stutter_events.id`，并设置
`ON DELETE CASCADE`：当 `stutter_events` 中超过 **7 天** 的记录被清理时，关联的
`windows_events` / `process_modules` / `stack_samples` / `root_cause_reports` 行自动级联删除。

- 清理由 service 端定期执行（与现有 `samples` 30 天清理同机制，周期不同）；
- `root_cause_reports` 作为结论表也在清理范围内——历史结论随事件一起淘汰，
  回溯对比仅对 7 天内的活跃事件有意义；
- 外键约束在 `logger.rs` 建表时通过 `REFERENCES stutter_events(id) ON DELETE CASCADE` 声明，
  同时 `PRAGMA foreign_keys = ON` 确保生效。

---

## 4. 功能需求

### 层级一 — 检测/数据层（service 侧，写库）✅ v0.2 已落地

> F-RC1~F-RC4 已于 v0.2 实施（提交 c983950 / 3c93df7），以下保留完整需求描述供参考。

#### F-RC1 — 结构化 CauseKind + 主因落库 ✅

- `CauseKind` 枚举**直接对齐 `detector.rs cause_key()`** 现有稳定类型 key（CPU usage / Available memory /
  Memory paging / Network spike / Disk spike …），不另起炉灶，避免枚举臆造（R2）。
- 多因同发时按"信号强度 × 持续"排序，第一项为 `primary_cause`；`write_event` 一并写入
  `cause_kinds` / `primary_cause` JSON；`reader.rs` 反序列化。
- 落库时**顺带记录各 cause 的「首触时刻」与事件「onset 时刻」**（见 §3.1），供 F-RC6 零复刻复用。
- 旧事件 `cause_kinds` 为空时，用 `cause_key()` 把 `causes` 文本**可靠回填**为枚举（精确映射，非脆弱关键词）。
- **建议**：把 `spike_check` / 硬阈值判定抽成 `detect_core` 纯函数，service 落库与 F-RC12 what-if 共用，
  从根上消除两边阈值语义漂移（见 R6）。
- 依赖：detector 改造 + schema 迁移 + 重装服务。

#### F-RC2 — 磁盘真繁忙度 + 系统级信号（协同 P5-B）✅

- `collector.rs` 新增 PDH / System 计数器：`% Disk Time`、`Avg Disk sec/Transfer`、
  `% DPC Time`、`% Interrupt Time`、`Context Switches/sec`（复用 `DiskPdh` 模式）。
- `detector.rs`：用 `disk_busy_percent > 95` 或 `disk_avg_io_ms > 50` **替代**磁盘 B/s spike；
  新增 `DpcInterrupt` / `InterruptStorm` / `ContextSwitchStorm`（带阈值 + 滞回）。
- **范围澄清**：`Memory paging`（`page_reads_per_sec`，真实 swap 卡顿信号）**已在采**，F-RC2 不重复覆盖；
  本次只补磁盘繁忙度 + 系统级信号。价值：磁盘根因从"吞吐"升级为"繁忙度"，补齐系统级卡顿真信号。

#### F-RC3 — 前台窗口冻结检测（UiFrozen）✅

- `detector.rs` 用 `SendMessageTimeout(WM_NULL, 200ms)` 探前台窗口是否真无响应；超时标 `UiFrozen` cause。
- 区分"资源高但还能动"与"前台应用真卡死"——后者才是用户感知到的卡顿。
- **阻塞约束（修订）**：每 tick 调用 500ms 超时会腰斩 1Hz 采样。**改为只在"已触发其它 cause"的帧探一次**
  （既然已判定卡顿，多 200ms 无感），或放独立低优先线程 + 200ms 超时 + **每 2s 限频**，绝不进入采集热路径。

#### F-RC4 — 温度→降频根因（ThermalThrottle）✅

- **数据源修订**：`gpu_temp` 从未填充（`collector.rs:437`），不纳入。改用 `cpu_temp` **+ `cpu_freq_mhz` 掉档判据**
  （负载下 cpu_freq 不升反降 = 真降频信号，强于"仅温度高"）。
- `detector.rs` 新增 `ThermalThrottle` cause：温度 > 阈值 **且** 疑似降频（cpu_freq 掉档 / cpu_usage 不随负载线性升）。
- 价值：笔记本散热差→降频→卡顿是真根因链，现被完全忽略。

### 层级二 — 归因逻辑层（分析/运行时，GUI 只读侧）✅ v0.2 已落地

> F-RC5~F-RC13 已于 v0.2 实施，以下保留完整需求描述供参考。

#### F-RC5 — 主因判定 + 加权归因 ✅

- **权重修订（对齐 detector 严重度定义）**：`detector.rs determine_severity` 按 `causes.len()` 定级
  （1→minor/2→major/3→critical），severity 实质就是并发 cause 数。若再乘 `severity_weight` 等于对 cause 数
  **重复计数**。故权重改为 `w = duration_norm × 主因信号强度`（主因信号强度取该 cause 的越阈幅度/持续），不乘 severity。
- Top N 进程/类型榜改用 `Σw` 排序——1 次 critical 30s 冻屏的元凶，权重大于 10 次 minor 200ms。
- `primary_cause`（已落库）直接作为"主因"高亮，而非把 `cause_kinds` 平铺。

#### F-RC6 — 因果方向（领先-滞后相关）✅

- **落库前置（修订）**：spike 类 cause 是**滑动基线比率 + 滞回**（`spike_check`），非静态阈值；回算易漂移。
  改为 **detector 落库事件时顺带记录每个 cause 的「首触时刻」与事件「onset 时刻」**
  （onset ≈ `event.t - 3s`，因 `sustained_seconds=3s` 持续满足后才记录，见 §3.1）。分析侧直接复用，零复刻。
- 对每事件取 `[onset-30s, event.t]` 的 `samples`，用首触时刻定位各 `cause_kinds` 资源**谁先动**；
  明显早于其余且唯一 → **触发者**；晚于/伴生 → **放大器**。
- 典型输出：`MemLow` 先动 → 随后 `DiskBusy`（paging）升高 → 卡顿；则内存不足是根因，磁盘忙是放大器。
- **性能（修订）**：不逐事件发 SQL。打开分析页时**一次性 bulk 拉取范围内全部 samples**（与 F3 降采样同源），
  内存切片算 leading signal，避免数千事件 × 小查询拖垮 DB。

#### F-RC7 — 基线偏离判定 ✅

- **数据前提修订（重要）**：`collector.rs` 中**非卡顿帧跳过 `top_processes` 构建**（`collect_with(false)`），
  非卡顿样本 `top_processes` 为空，故**不能**用非卡顿 `samples.top_processes` 算基线。
- **改为事件侧聚合**：从历史事件 `culprits` / `snapshot.top_processes` 聚合某进程"作为元凶时的典型占用"，
  作为该进程的常态基线；归因时只把超出 `基线 × factor` 的 culprit 标"显著偏离"。
- 持续高占用（如浏览器常驻 30% CPU）标"常驻高占用（噪声）"并降权。
- 与 F-RC13 画像复用同一数据源。价值：过滤"谁都在跑"的常驻进程噪声，根因更聚焦。

#### F-RC8 — 多进程共现聚类 ✅

- 把每事件 `culprits` 的进程名集合做共现统计（频次 / Jaccard），输出高频"卡顿组合"
  （如 `浏览器 + Windows Update + 杀软`）。
- 可下钻到某组合参与的全部事件（高级模式）。价值：抓"组合效应"而非单一元凶。

#### F-RC9 — 因果链 / 级联归因 ✅

- 多 `cause_kinds` 同发时，按 F-RC6 的 `t_lead`（首触时刻）排序成有向链（根因 → 传导 → 表象）：
  例 `MemLow → DiskBusy(Paging) → Stutter`。
- 呈现为轻量链路图（节点= cause，边= 时间先后），替代平铺 `causes` 列表。

### 层级三 — 呈现 / 交互层（GUI，复用分析窗口）✅ v0.2 已落地

> F-RC10~F-RC13 已于 v0.2 实施，以下保留完整需求描述供参考。

#### F-RC10 — 单事件根因钻取卡（核心载体）✅

- **前置依赖（已解决）**：`StutterEvent`（`types.rs`）已带 `id: i64`（v0.2 修订后补齐），reader 可正常携带事件主键，钻取卡精准关联无阻塞。
- 点一条事件（携带 `id`）→ 卡片同时给出：
  1. **主因**（带置信度，见 F-RC11）；
  2. **前导资源曲线**（±60s，复用 F3 降采样 + plotters）；
  3. **参与进程及偏离基线幅度**（F-RC7 结果）；
  4. **因果链图**（F-RC9 结果）；
  5. **软件根因定位**（v0.3 新增，见 F-RC16：元凶进程完整路径/句柄泄漏/事件日志命中/调用栈热点）。
- 比 F3 全区间叠加精确，是根因 UI 的核心入口。

#### F-RC11 — 根因置信度 ✅

- **置信度修订（重要）**：多因并发是 major/critical 的**定义本身**（`causes.len()` ≥2 才 major），
  若"多因即低置信"会误伤所有高级事件。置信度应看**主因是否明显领先其余 cause**（强度/时间差），而非 cause 数量：
  - 主因信号强度 / 首触时刻明显领先其余 → 高置信；
  - 主因与次因强度接近、首触时刻重叠 → 低置信（标注"主因不显著，疑多因并发"）。
- UI 用色阶/文字标注，避免用户被低置信结论误导。
- **校准建议**：公式初值为经验值，建议用 notify 用户反馈 / 人工标注样本校准，否则只是装饰。

#### F-RC12 — 阈值敏感性 what-if ✅

- 客户端用 `snapshot` 中**已存储的信号值** vs 用户可调阈值，重算"若阈值 X 是否会触发该 cause"。
- **不改 service**（保持只读契约），仅本地模拟，反向辅助调 `config.toml [detection]` 配置。
- 约束：模拟阈值语义必须与 `detect_core` 纯函数一致（复用 service 同套逻辑，见 §5.1 / R6）。

#### F-RC13 — 同类事件画像对比 ✅

- 按 `cause_kinds + culprit 集合 + duration 分桶` 聚类历史事件；对当前事件显示
  "匹配已知画像：进程 Y 典型卡顿"，辅助判断是已知元凶复发还是新情况。
- 与 F-RC7 共用事件侧聚合数据源（进程作为元凶时的典型占用画像）。

### 层级四 — 软件根因定位 + 结论落库（v0.3 新增，service 采集 → 结论落库 → UI 呈现）

> 本条把因果链从「资源指标」延伸到「元凶软件 / 驱动 / 模块」，并把结论持久化：
> F-RC14 采集（service 侧）→ F-RC15 落库（GUI 窄写权）→ F-RC16 呈现（GUI 只读）。

#### F-RC14 — 软件根因定位（进程级 + 事件日志 + 调用栈，service 侧采集落库）

把因果链从「资源指标」延伸到「元凶软件 / 驱动 / 模块」，分四层递进，**每层独立可降级**：

**F-RC14-a 进程指纹扩充（进程级「是谁」+「是否泄漏」）**
- `ProcessBrief` 新增 `exe_path` / `handle_count` / `gdi_objects` / `user_objects`（§3.4.1）。
- 仅卡顿帧（`collect_with(true)`）采集，非卡顿帧跳过；句柄/GDI 用 `GetProcessHandleCount` /
  `GetGuiResources`，进程打开开销一次性、限频。
- 判定：`handle_count` 超阈值（`config.toml [detection] handle_leak_threshold`，默认 10000）→
  `ProcessHandleLeak`；`gdi_objects + user_objects` 超阈值（`config.toml [detection] gdi_leak_threshold`，
  默认 10000）→ `GdiObjectLeak`。阈值需根据实际采集数据校准（正常 Chrome 即可上万句柄）。
- 落库：`culprits` JSON 随结构扩展自动落库。
- 价值：回答「是哪个软件」，并抓「句柄泄漏 / GDI 泄漏」这类真正的软件 bug 根因。

**F-RC14-b 进程级 IO + 已加载模块（「哪个进程在狂读写 / 加载了什么」）**
- `ProcessBrief` 新增 `io_read_bps` / `io_write_bps`（`GetProcessIoCounters` 差分）。
- 卡顿事件生成时 snap 每个 culprit 的已加载模块列表 → `process_modules` 表（§3.4.2）。
- 判定：某进程 `io_*_bps` 显著高于其余 culprit → 标「磁盘狂读写元凶」；模块列表含
  可疑第三方 DLL/驱动 → 供人工研判。
- 价值：把「磁盘繁忙」落到「是哪个进程在狂读盘」。

**F-RC14-c Windows 事件日志回溯（「驱动 / 服务 / 硬件发生了什么」）**
- 卡顿事件生成时回溯 `[onset-30s, now]` 的 System/Application 日志，白名单抽高价值事件
  （§3.4.3），落 `windows_events` 表。
- 判定：命中 → 对应软件级 cause：`Display 4101`→`DriverTimeout`；`SCM 7031/7034`→`ServiceCrash`；
  `disk 7/51/153`→`DiskIoError`；`WHEA`→`HardwareError`。
- 价值：直接点出「显卡驱动超时」「磁盘坏块」「某服务崩溃」，这是资源信号给不了的真根因。

**F-RC14-d ETW CPU 调用栈采样（代码级热点，重能力）**
- 数据源：ETW `NT Kernel Logger` 的 `SampledProfile` + `StackWalk` 事件。
- 采集：独立低优先级线程，卡顿触发后限频（如每 2s 一轮、每轮 ≤100ms）采样；绝不进采集热路径。
- 符号化：只到「模块名 + RVA」级别（§1.4 非目标，不做完整 PDB）。
- 落库：聚合热点 → `stack_samples` 表（§3.4.4）。
- 价值：定位「CPU 热点集中在哪个模块」，把 `CpuHigh` 落到「某 exe 的某偏移反复被采样」。
- 约束/风险：Rust 侧需直接调 `Win32::System::Diagnostics::Etw`，实现成本高；**列为 M5 独立
  里程碑 + 降级路径**（ETW 初始化失败 → 该层静默关闭，不影响其余层）。

**软件级 cause 与主因关系**：软件级 cause（`ProcessHandleLeak` / `GdiObjectLeak` /
`DriverTimeout` / `ServiceCrash` / `DiskIoError` / `HardwareError`）在卡顿事件生成时由
`enrich_software_causes` 追加到 `cause_kinds`，并参与 `primary_cause` 排序——**若事件日志
命中驱动超时/服务崩溃，应优先于资源级 cause 作为主因**（因为那才是用户能采取行动的真根因）。

#### F-RC15 — 分析结论落库（可回溯、可审计）

- 新增 `root_cause_reports` 表（§3.4.5），由 service 建表批次一并 `CREATE TABLE IF NOT EXISTS`。
- GUI 新增**可写连接**（区别于 `open_readonly`），仅对该表 `INSERT OR REPLACE`（按 `event_id`
  UPSERT）；原始表仍只读，P3 契约本质不变。
- 触发时机：GUI 打开分析页 / 点开钻取卡时，对当前事件算完 F-RC5~F-RC14 结论后落库；
  二次打开直接读表，不再重算（除非算法版本变化）。
- `algorithm_version` 记录分析算法版本：版本升级后重算时可见「上次结论 vs 本次结论」，实现可审计。
- 价值：解决 v0.2「结论只活在内存、下次打开就丢、算法一变就漂移」的缺陷。

#### F-RC16 — 软件根因 + 结论回溯的 UI 呈现（GUI 只读侧）

把 F-RC14 的软件根因数据与 F-RC15 的落库结论在现有分析窗口呈现，**复用 F-RC10 钻取卡载体**：

1. **钻取卡新增「软件根因」区块**（扩展 F-RC10 第 5 项）：
   - **元凶软件卡片**：完整路径（`exe_path`）+ 句柄/GDI 泄漏标记（`ProcessHandleLeak` / `GdiObjectLeak`）
     + 进程级磁盘读写（`io_read_bps` / `io_write_bps`）；
   - **事件日志命中**：回读 `windows_events`，展示「显卡驱动 TDR 超时(4101)」「磁盘坏块(7/153)」
     「服务崩溃(7031/7034)」「硬件错误(WHEA)」等，带 provider + 事件 ID + 消息摘要；
   - **已加载模块**：回读 `process_modules`，全部展示；对非系统目录（非 `C:\Windows\System32\`、
     `C:\Windows\SysWOW64\`、`C:\Windows\System32\drivers\` 等系统路径）的 DLL/驱动加 ⚠️ 标记，
     供人工研判（恶意模块也可能在 System32 下，故不过滤，仅标注）；
   - **调用栈热点**：回读 `stack_samples`，Top N 热点模块（模块名 + 命中数），把 `CpuHigh` 落到
     「某 exe / dll 的某偏移反复被采样」。
2. **主因呈现修订**：主因从「资源指标」升级为「软件级优先」——命中 `DriverTimeout` / `ServiceCrash` /
   `DiskIoError` / `HardwareError` 时，主因高亮显示软件根因（可行动），资源级 cause 降为次要展示。
3. **结论回溯 UI**（F-RC15 配套）：
   - 钻取卡顶部显示「上次结论（`algorithm_version` vX）」vs「本次结论（vY）」差异；
   - 历史结论时间线：按事件查看历次结论变更（`root_cause_reports` 回读，见 §6）。
4. **数据来源**：全程只读——新增 `load_software_root_cause(event_id)` 聚合查询
   （`windows_events` / `process_modules` / `stack_samples` / `root_cause_reports`），
   复用 `load_full_events` 的只读连接与后台线程渲染模式。
5. **降级**：软件根因数据为空时（service 未升级 / ETW 关闭 / 旧事件无软件字段），钻取卡隐藏
   软件根因区块，仅显示资源级结论，不破坏现有体验。

---

## 5. 技术方案

### 5.1 总体路线

- **检测/数据层**：改 `detector.rs` / `collector.rs` / `logger.rs` / `types.rs`，经 P5 部署约束（停服重装）生效。
- **分析层**：扩展 `crates/ui/src/analytics.rs` 新增纯函数（`leading_signal` / `baseline` / `cooccurrence` / `cause_chain` / `confidence`），全部带单元测试；UI 在 `analysis.slint` 现有窗口内新增"根因"区块/钻取卡。
- **`detect_core` 纯函数（关键）**：把 `spike_check` / 硬阈值判定抽成与 service 共用的纯函数，F-RC6 重算首触与
  F-RC12 what-if 都调用它，从根上消除"分析侧复刻 detector 数学"的漂移（见 R6/R8）。
- 图表、降采样、后台线程、本地时区、索引**全部复用** `卡顿分析界面-PRD.md` 既有方案，不重复造轮子。
- **软件根因定位层（F-RC14，v0.3）**：`collector.rs` 扩展进程指纹 + 进程级 IO（`GetProcessHandleCount`/
  `GetGuiResources`/`GetProcessIoCounters`）；`logger.rs` 建 `windows_events` / `process_modules` /
  `stack_samples` 三表；`detector.rs` 新增 `enrich_software_causes`（事件日志回溯 + 进程指纹阈值 →
  软件级 cause）；ETW 调用栈采样独立线程。全部经 P5 部署约束（停服重装）生效。
- **结论落库层（F-RC15，v0.3）**：`logger.rs` 建 `root_cause_reports` 表；`analytics.rs` 新增
  `open_report_writer` 可写连接（只写结论表）+ `save_root_cause_report`，复用 F-RC5~F-RC14 纯函数结果落库。
- **软件根因呈现层（F-RC16，v0.3）**：`analytics.rs` 新增 `load_software_root_cause(event_id)` 只读聚合
  （`windows_events` / `process_modules` / `stack_samples` / `root_cause_reports`）；`analysis.rs` 在钻取卡
  新增「软件根因」区块 + 结论回溯，复用 `load_full_events` 的只读连接与后台线程渲染模式。

### 5.2 因果方向算法（F-RC6）

```
对每个事件 e（含 onset 与 各 cause 首触时刻，由 detector 落库，见 §3.1）:
  窗口 samples_win = samples WHERE ts IN [e.onset-30s, e.t]   # onset≈e.t-3s
  # 优先用落库首触时刻；缺失时回退到用 detect_core 同套阈值在 samples_win 内重算首触
  for each cause c in e.cause_kinds:
     t_lead[c] = e.首触时刻[c]  （或 samples_win 内该资源首次越阈时刻）
  触发者 = argmin(t_lead[c] 且非 None)
  若 触发者 的 t_lead 早于 e.t 且与其余 cause 差距 > Δ → 高置信触发者
  其余 → 放大器
  # 阈值语义必须与 detect_core 纯函数一致（见 §5.1 / R6）
```

### 5.3 基线计算（F-RC7）

- **数据来源修订（见 F-RC7）**：非卡顿样本 `top_processes` 为空，基线改从**事件侧**聚合：
  取时间范围内历史事件的 `culprits` / `snapshot.top_processes`，按进程名聚合其作为元凶时的典型
  CPU/内存占用（如 90 分位）→ `baseline[name]`。
- 样本不足（该进程作为元凶出现次数 < N）时降级为绝对阈值（沿用 detector 阈值），并在 UI 注明。

### 5.4 置信度（F-RC11）

```
单 cause                       → 0.8~1.0
多 cause 但 主因强度/首触明显领先  → 0.6~0.85（主因可信）
主因与次因强度接近、首触重叠     → 0.2~0.5（标注"主因不显著，疑多因并发"）
# 注：多因并发本身是 major/critical 定义，不再单独压低置信（见 F-RC11 修订）
```

### 5.5 性能

- **因果方向**：打开分析页时**一次性 bulk 拉取范围内全部 samples**（与 F3 降采样同源），
  内存切片算 leading signal，避免"每事件一次小查询"在数千事件下拖垮 DB（修订见 F-RC6）。
- 基线/共现：分析打开时一次性计算并缓存，刷新时重算当前范围。
- 资源曲线降采样沿用 F3 方案，避免 30 天 259 万点卡 UI。

### 5.6 软件根因定位技术方案（F-RC14）

软件级 cause 检测与现有 PDH 阈值检测是**两条独立路径**，在卡顿事件生成阶段汇合：

- **进程指纹阈值（-a/-b）**：`detector.rs` 在事件生成前对 `current_culprits` 里每个 culprit 的
  `handle_count` / `gdi_objects+user_objects` / `io_*_bps` 做硬阈值判定，命中即追加
  `ProcessHandleLeak` / `GdiObjectLeak` / 磁盘狂读写标记。进程指纹不常震荡，直接硬阈值即可（无需滞回）。
- **事件日志回溯（-c）**：`enrich_software_causes(event)` 在 `write_event` 前调用，读
  `[onset-30s, now]` 事件日志白名单事件，映射到软件级 cause 并写 `windows_events`。
- **调用栈采样（-d）**：独立低优先级线程，仅在卡顿激活时触发（对齐 F-RC3 的 `ui_probe` 依赖注入
  模式，便于单测绕过真实 ETW）；采样结果聚合后写 `stack_samples`。
- **主因优先**：`primary_cause` 排序规则调整为「软件级 cause（若存在）> 资源级 cause 按首触」，
  因为驱动超时 / 服务崩溃才是用户能采取行动的真根因。多个软件级 cause 同时命中时按严重程度排序：
  `HardwareError` > `DriverTimeout` > `ServiceCrash` > `DiskIoError` > `ProcessHandleLeak` > `GdiObjectLeak`（见 §3.3）。

### 5.7 结论落库技术方案（F-RC15）

- **写权隔离**：`analytics.rs` 保留 `open_readonly`（读原始表），新增 `open_report_writer(db_path)`
  用 `SQLITE_OPEN_READ_WRITE` 打开，仅执行 `INSERT OR REPLACE INTO root_cause_reports ...`。
- **WAL 并发**：WAL 已开（`logger.rs` 建库时 `PRAGMA journal_mode=WAL`），service 写 samples/events、
  GUI 写 reports，写不同表可共存；`busy_timeout` 设 5000ms 避免两个写者偶发争抢（service 批量 flush 时可能短暂持锁）。
- **建表归属**：`root_cause_reports` 由 service 建（`logger.rs` 建表批次），保证表存在；
  GUI 侧 `ensure_report_table` 仅做 `CREATE TABLE IF NOT EXISTS` 兜底（避免 service 未升级时缺表）。
- **幂等**：`INSERT OR REPLACE`（`event_id` UNIQUE）天然幂等，重复分析不产生重复行。
- **算法版本**：常量 `ANALYSIS_ALGO_VERSION`（如 `"rc5-rc14.v1"`）落库时写入；升级算法时改版本号，
  读表时比对版本决定是否重算。

---

## 6. 数据查询 / 算法草稿

**F-RC1 结构化解（枚举直接聚合）：**
```sql
SELECT cause_kinds FROM stutter_events WHERE timestamp BETWEEN ?1 AND ?2;
-- Rust: JSON 解析数组 → 按枚举 GROUP BY 计数；
--       旧库 cause_kinds 空 → 用 cause_key() 可靠回填（见 §3.1，非脆弱关键词）
```

**F-RC5 加权归因（内存聚合）：**
```sql
SELECT culprits, duration_ms, primary_cause, cause_kinds
FROM stutter_events WHERE timestamp BETWEEN ?1 AND ?2;
-- Rust: 主因信号强度 sig = 该 cause 越阈幅度/持续（来自 snapshot 或落库首触）；
--       w = dur_norm(duration_ms) * sig(primary_cause)；按 culprit name 累加 Σw（不乘 severity，见 F-RC5）
```

**F-RC6 因果方向（bulk 拉取，内存切片）：**
```sql
-- 一次性 bulk 拉取范围内全部 samples（避免逐事件查询），内存切片到每事件 [onset-30s, t]
SELECT id, timestamp, cpu_usage, mem_usage_percent, mem_available_mb,
       disk_busy_percent, dpc_percent, cpu_freq_mhz, cpu_temp
FROM samples
WHERE timestamp BETWEEN ?1 AND ?2
ORDER BY timestamp;
-- 配合 stutter_events 的 id / onset / 各 cause 首触时刻（落库字段，见 F-RC1/F-RC6）
-- Rust: 内存切片算 leading signal → 触发者/放大器
```

**F-RC7 基线（事件侧聚合，非卡顿 top_processes 为空）：**
```sql
-- 基线改从事件侧聚合（非卡顿 samples.top_processes 为空，见 F-RC7）
SELECT culprits, snapshot FROM stutter_events
WHERE timestamp BETWEEN ?1 AND ?2;
-- Rust: 解析 snapshot.top_processes，按进程名聚合其作为元凶时的典型占用 → baseline[name]
```

**F-RC14-c 事件日志回溯（白名单过滤，卡顿事件生成时一次性回溯）：**
```rust
// Rust：EventLog 读取伪代码（非 SQL，读 Windows 事件日志）
let events = read_event_log(channels = ["System", "Application"],
                            from = onset - 30s, to = now)
    .filter(|e| WHITELIST.contains(&(e.provider, e.event_id)));
// 白名单：Display 4101、disk 7/51/153、Kernel-Power 41、SCM 7031/7034、WHEA
// 命中 → 映射 DriverTimeout / ServiceCrash / DiskIoError / HardwareError，
// 落 windows_events（含 channel 列，区分 System / Application）
```

**F-RC14-d 调用栈热点（聚合后落 stack_samples，按命中数取 Top）：**
```sql
SELECT module, rva, SUM(sample_count) AS hits
FROM stack_samples
WHERE event_id = ?1
GROUP BY module, rva
ORDER BY hits DESC LIMIT 20;
```

**F-RC15 结论落库（UPSERT by event_id，可回溯）：**
```sql
INSERT OR REPLACE INTO root_cause_reports
  (event_id, algorithm_version, primary_cause, confidence, cause_chain,
   software_root_cause, baseline_delta, computed_at)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8);
-- Rust: 复用 F-RC5~F-RC14 纯函数结果序列化后落库；
--       读回时比对 algorithm_version 决定是否重算（实现可审计）。
```

**F-RC16 软件根因回读（钻取卡聚合查询，只读）：**
```sql
-- 事件日志命中
SELECT channel, provider, win_event_id, level, message, ts
FROM windows_events WHERE event_id = ?1 ORDER BY ts;
-- 已加载模块（全部展示；UI 层对非系统路径加 ⚠️ 标记）
SELECT pid, process_name, module_path,
  CASE WHEN module_path NOT LIKE 'C:\Windows\System32\%'
        AND module_path NOT LIKE 'C:\Windows\SysWOW64\%'
        AND module_path NOT LIKE 'C:\Windows\System32\drivers\%'
       THEN 1 ELSE 0 END AS non_system
FROM process_modules WHERE event_id = ?1
ORDER BY non_system DESC, module_path;
-- 调用栈热点（Top 模块）
SELECT process_name, module, rva, sample_count
FROM stack_samples WHERE event_id = ?1 ORDER BY sample_count DESC LIMIT 20;
-- 结论回溯（历次算法版本结论对比）
SELECT algorithm_version, primary_cause, confidence, computed_at
FROM root_cause_reports WHERE event_id = ?1;
```

---

## 7. 验收标准

- [ ] `cause_kinds` / `primary_cause` / `cause_first_touch` / `onset_ts` 落库并可被 `reader` 回读；
      旧库 `cause_kinds` 为空时用 `cause_key()` 可靠回填不崩溃。
- [ ] F-RC2 磁盘繁忙度 / DPC / 中断 / 上下文切换信号已采集并可作为 cause（协同 P5-B；paging 已覆盖不重复）。
- [ ] F-RC3 `UiFrozen` 在"前台窗口真无响应"时触发，且**不进入采集热路径**（仅已触发其它 cause 帧探 / 独立线程 200ms 限频）。
- [ ] F-RC4 `ThermalThrottle` 在温度高 + `cpu_freq` 掉档（疑似降频）时触发（`gpu_temp` 不纳入）。
- [ ] F-RC5 主因榜按 `duration × 主因信号强度` 加权（不乘 severity），critical 长事件主导排序。
- [ ] F-RC6 能区分触发者与放大器（依赖 detector 落库 onset/首触时刻；抽样事件人工核对合理）。
- [ ] F-RC7 基线从**事件侧**聚合（非卡顿 `top_processes` 为空），过滤常驻高占用噪声。
- [ ] F-RC8 输出高频"卡顿组合"并可下钻。
- [ ] F-RC9 多因事件呈现有向因果链。
- [ ] F-RC10 单事件根因钻取卡显示主因 + 前导曲线 + 偏离幅度 + 因果链（事件需携带 `id`，见 §3.1 前置依赖）。
- [ ] F-RC11 根因置信度在 UI 可见，按"主因是否领先"判定，低置信标注"主因不显著，疑多因并发"。
- [ ] F-RC12 what-if 纯客户端模拟、不改 service、复用 `detect_core` 阈值语义。
- [ ] F-RC13 同类事件画像对比可给出"匹配已知画像"结论（与 F-RC7 共用事件侧数据源）。
- [ ] 分析侧对原始表（`samples` / `stutter_events` / `service_heartbeat`）只读；仅
      `root_cause_reports` 结论表有窄写权，service 侧改动经重装生效，不破坏 P3 契约。
- [ ] F-RC14-a `ProcessBrief` 新增 `exe_path`/`handle_count`/`gdi_objects`/`user_objects` 落库且
      仅卡顿帧采集（非卡顿帧不额外开销）；句柄/GDI 超阈值产出 `ProcessHandleLeak`/`GdiObjectLeak`。
- [ ] F-RC14-b 进程级 IO（`GetProcessIoCounters` 差分）+ 已加载模块（`process_modules` 表）落库可回读。
- [ ] F-RC14-c 卡顿事件生成时回溯 `[onset-30s, now]` 事件日志，白名单事件落 `windows_events`，
      命中映射 `DriverTimeout`/`ServiceCrash`/`DiskIoError`/`HardwareError`。
- [ ] F-RC14-d ETW 调用栈热点落 `stack_samples`（模块+RVA 级别）；ETW 初始化失败时该层静默关闭、
      不影响其余层与采集热路径。
- [ ] F-RC14 软件级 cause 命中时优先于资源级 cause 作为 `primary_cause`。
- [ ] F-RC15 `root_cause_reports` 落库（UPSERT by `event_id`），`algorithm_version` 变更后可对比
      新旧结论；重复分析幂等不产生重复行。
- [ ] F-RC16 钻取卡新增「软件根因」区块：元凶软件卡片（完整路径/句柄/GDI 泄漏/进程级 IO）、
      事件日志命中（provider+事件 ID+消息摘要）、可疑模块、调用栈热点（Top 模块）。
- [ ] F-RC16 主因呈现「软件级优先」：命中 `DriverTimeout`/`ServiceCrash`/`DiskIoError`/`HardwareError`
      时高亮软件根因，资源级 cause 降为次要。
- [ ] F-RC16 结论回溯 UI 可见「上次结论 vs 本次结论」差异（`algorithm_version` 对比）；软件根因
      数据为空时钻取卡隐藏软件区块、不破坏现有体验。
- [ ] 新增代码通过 `rtk cargo test` 且 `rtk cargo build --release` 零警告。
- [ ] 用旧库 `culprits` JSON 反序列化新 `ProcessBrief` 结构不崩溃（`#[serde(default)]` 兼容性验证）。
- [ ] `windows_events` / `process_modules` / `stack_samples` / `root_cause_reports` 四表通过
      `ON DELETE CASCADE` 随 `stutter_events` 7 天清理级联删除，无孤儿行。
- [ ] 句柄泄漏阈值（`handle_leak_threshold`）与 GDI 泄漏阈值（`gdi_leak_threshold`）在
      `config.toml [detection]` 中可配置，默认均为 10000。
- [ ] 多个软件级 cause 同时命中时，按 `HardwareError > DriverTimeout > ServiceCrash > DiskIoError
      > ProcessHandleLeak > GdiObjectLeak` 排序取主因。
- [ ] F-RC16 已加载模块列表全部展示（不过滤），非系统路径的 DLL/驱动加 ⚠️ 标记。

---

## 8. 风险与待确认清单

| # | 事项 | 建议默认 | 影响 |
| --- | --- | --- | --- |
| R1 | 检测/数据层需改 service + 重装 | 与 P5-A/P5-B 协同排期 | 决定 F-RC1~F-RC4 落地节奏 |
| R2 | `CauseKind` 枚举对齐 | 直接对齐 `detector.rs cause_key()` 现有 key（已修订） | 臆造 key 归类错误；旧事件用 `cause_key` 回填 |
| R3 | 置信度误伤高级事件 | F-RC11 改为"主因是否领先"而非"是否多因"（已修订）；公式需用户反馈校准 | 影响 F-RC6/F-RC11 结论可靠性 |
| R4 | 基线依赖事件样本量 | 改从事件侧聚合；某进程作元凶次数不足时降级绝对阈值（已修订） | 影响 F-RC7 早期可用性 |
| R5 | `UiFrozen` 阻塞风险 | 仅在已触发其它 cause 帧探一次 / 独立线程 + 200ms 超时 + 每 2s 限频（已修订） | 影响采集热路径稳定性 |
| R6 | what-if 阈值语义一致性 | 抽 `detect_core` 纯函数 service/what-if 共用（已建议） | 影响 F-RC12 模拟可信度 |
| R7 | 事件 `id` 缺失前置 | ✅ 已解决：`StutterEvent` 已带 `id: i64`（v0.2 修订后补齐） | 无 id 钻取卡无法精准关联 |
| R8 | 阈值语义漂移 | 抽 `detect_core` 纯函数，service 与 what-if 共用（见 §5.1/R6） | 影响 F-RC6/F-RC12 可信度 |
| R9 | 软件根因定位的权限 / 采集成本 | 进程指纹仅卡顿帧限频；事件日志仅回溯；ETW 独立线程 + 限频 + 可降级 | 影响 F-RC14 稳定性与开销 |
| R10 | ETW 调用栈实现成本高 | 列为 M5 独立里程碑；符号化只到模块 + RVA；初始化失败静默降级 | 影响 F-RC14-d 能否按期落地 |
| R11 | GUI 写结论表破坏「只读」契约 | 窄写权：仅 `root_cause_reports` 表，原始表仍只读；`busy_timeout` 兜底 | 影响 P3 契约边界 |
| R12 | 算法版本升级致历史结论漂移 | `algorithm_version` 落库，读回比对决定重算，新旧可对比 | 影响 F-RC15 可审计性 |
| R13 | 句柄/GDI 泄漏阈值缺乏经验依据 | 默认 10000，在 `config.toml` 中可配置；需根据实际采集数据校准（正常 Chrome 即可上万句柄） | 影响 F-RC14-a 误报率 |
| R14 | 新增 4 张表缺少清理策略致 DB 膨胀 | `ON DELETE CASCADE` 随 `stutter_events` 7 天清理级联删除（§3.4.6） | 影响长期运行磁盘占用 |

---

## 9. 实施里程碑

1. **M1 数据地基** ✅ 已完成（v0.2，提交 c983950 / 3c93df7）：F-RC1 结构化 `CauseKind`+`primary_cause` 落库（含各 cause 首触时刻 / 事件 onset 落库、抽 `detect_core` 纯函数）；F-RC2 P5-B 信号；F-RC3 `UiFrozen`；F-RC4 `ThermalThrottle`（协同 P5 部署）；**`StutterEvent` 加 `id`**（F-RC10 前置）。
2. **M2 归因算法** ✅ 已完成（v0.2）：F-RC5（`duration × 主因强度`，不乘 severity）；F-RC6（落库 onset/首触 + bulk samples）；F-RC7（事件侧基线）；F-RC8 共现；F-RC9 因果链（`analytics.rs` 纯函数 + 单测）。
3. **M3 呈现** ✅ 已完成（v0.2）：F-RC10 钻取卡；F-RC11 置信度；F-RC12 what-if；F-RC13 画像对比（复用分析窗口）。
4. **M4 打磨** ✅ 已完成（v0.2）：旧库兼容、测试、零警告构建。
5. **M5 软件根因 + 结论落库 + UI 呈现（v0.3）**：
   - M5.1 F-RC14-a 进程指纹 + F-RC14-b 进程级 IO/已加载模块（`collector.rs` + `detector.rs` +
     `logger.rs` 建 `process_modules` 表）；
   - M5.2 F-RC14-c 事件日志回溯（`enrich_software_causes` + `windows_events` 表）；
   - M5.3 F-RC14-d ETW 调用栈（独立线程 + `stack_samples` 表；**实现成本高，可降级、可独立延后**）；
   - M5.4 F-RC15 结论落库（`root_cause_reports` 表 + `open_report_writer` + `save_root_cause_report`）；
   - M5.5 F-RC16 UI 呈现（钻取卡软件根因区块 + 主因软件级优先 + 结论回溯，`analysis.rs`/`analysis.slint`）。

> 备注：M1~M4 已于 v0.2 完成（提交 c983950 / 3c93df7）。M5 为 v0.3 新增，当前待实施。
> M5.1~M5.3 属 service 侧改造（需重装服务）；M5.4 在 GUI 侧，依赖 M5.1~M5.3 的软件根因字段
> 就绪后才有完整结论可落库（降级路径：先落资源级结论，软件字段为空）。
> M5.5（F-RC16 UI）依赖 M5.1~M5.4；但 UI 区块可先按「数据为空则隐藏」实现，与数据层解耦并行开发。
