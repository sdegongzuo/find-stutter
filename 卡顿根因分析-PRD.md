# 卡顿根因分析 — 产品需求文档（PRD）

> 版本：v0.2（评审修订）
> 起草日期：2026-08-11
> 修订日期：2026-08-12
> 状态：评审通过（已采纳代码评审意见，待实施）
> 关联文档：`卡顿分析界面-PRD.md`（F1–F8）、`TODO.md`（§P5 检测精度优化）、
> `crates/core/src/types.rs`、`crates/core/src/detector.rs`、`crates/core/src/collector.rs`、
> `crates/core/src/logger.rs`、`crates/ui/src/analytics.rs`

---

## 1. 背景与目标

### 1.1 现状

`卡顿分析界面-PRD.md`（F1–F8）已落地「事后看卡顿」能力：趋势、进程归因 Top N、
资源关联、类型占比、双模式、CSV 导出。但**根因分析只走到因果链最末端**：

- `causes` 是**自由文本数组**（如 `"CPU usage 95.0% > 90.0%"`、`"Disk Spike"`），没有统一枚举；
- `culprits` 只做**平权计数**（"出现在 N 次卡顿中"），不区分严重度与时长，也不标主因；
- 资源关联（F3）只能看"卡顿与资源**同时发生**"，无法区分**触发者**与**放大器**；
- 磁盘判据用 B/s 吞吐（SSD 写 130MB/s 根本不卡），DPC/中断/上下文切换等"系统级卡顿"信号完全缺失（见 `TODO.md` §P5-B）；
- 温度已采集但未用作根因；前台窗口是否真冻结未检测。

### 1.2 痛点

一句话：**现在只能回答"卡顿时谁在场、什么指标高"，回答不了"谁先动、谁是主因、是不是被牵连"。**
用户要的是"根因"，现有数据/算法给的是"相关"。

### 1.3 目标

分两层补齐因果链：

1. **检测/数据层（service 侧，写库）**——让根因有干净、结构化的数据基础：
   结构化 `CauseKind` 枚举、真实磁盘繁忙度与系统级信号、前台冻结检测、温度降频根因。
2. **归因逻辑层 + 呈现层（GUI 只读侧）**——在现有分析窗口上做深度归因：
   主因加权、因果方向、基线偏离、多进程共现、因果链，并以"单事件根因钻取卡"呈现。

### 1.4 非目标（明确不做）

- 不改检测阈值生产逻辑（F-RC12 what-if 仅客户端用存储的 `snapshot` 信号值**模拟**，不写 service）。
- 不跨机器聚合。
- 不把分析页改回可写 `stutter.db`（保持 P3 只读契约；基线/共现等均在内存或查询时计算）。

### 1.5 评审修订说明（2026-08-12，已对照代码核实）

本版对照 `types.rs` / `detector.rs` / `collector.rs` / `logger.rs` 实际代码做了修订，
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
7. **F-RC10 前置依赖**：`StutterEvent` Rust 结构（`types.rs`）未带 `id`，reader 读出后丢失事件主键；
   钻取关联需给 `StutterEvent` 加 `id`（或分析层单独 `SELECT id`）。
8. **置信度定义修正（F-RC11）**：多因并发是 major/critical 的**定义本身**，低置信会误伤所有高级事件；
   改为看"主因是否明显领先其余 cause（强度/时间差）"。
9. **已知缺口（后续扩展）**：本 PRD 缺「根因纵向趋势」（某根因/进程是否随时间恶化），可与「周期规律热图」呼应，留待二期。

---

## 2. 与 `卡顿分析界面-PRD.md` 的关系

本 PRD 是 F1–F8 的**根因深化扩展**，不是另起炉灶：

- **复用基础设施**：分析窗口（`analysis.slint` / `analysis.rs`）、plotters→Image 图表、
  后台线程渲染、像素降采样、本地时区分桶、`idx_events_ts` / `idx_samples_ts` 索引。
- **条目编号**：本 PRD 功能统一以 `F-RCn` 编号（RC = Root Cause），与 F1–F8 并列，避免混淆。
- **依赖边界**：`F-RC1~F-RC4` 属 service 侧改造，需改检测器/采集器/schema 并重装服务
  （`TODO.md` §P5 部署约束）；`F-RC5~F-RC13` 在 GUI 只读侧消费现有/新增字段。

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
| `id` | INTEGER(PK) | **前置依赖（F-RC10 钻取卡）**：`StutterEvent` 当前未带 `id`，reader 读出后丢失主键；需显式携带 | 已有主键 |

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

### 3.3 `CauseKind` 枚举（F-RC1 定义，检测端产出）

```
CpuHigh | MemLow | DiskBusy | DiskSpike | GpuHigh
ThermalThrottle | DpcInterrupt | InterruptStorm | ContextSwitchStorm
NetSpike | UiFrozen
```

- 枚举值**直接对齐 `detector.rs cause_key()` 现有稳定 key**，不得臆造（消除 R2「分类不连续」风险）。
- `ThermalThrottle` 数据源为 `cpu_temp` + `cpu_freq_mhz` 掉档判据（`gpu_temp` 从未填充，不纳入）。
- `UiFrozen` 为本次新增 cause。`NetSpike` / `DiskSpike` 等沿用 `cause_key` 现有 key。
- 序列化用字符串（便于旧库回读与新库直接 `GROUP BY`）。

---

## 4. 功能需求

### 层级一 — 检测/数据层（service 侧，写库）

#### F-RC1 — 结构化 CauseKind + 主因落库

- `CauseKind` 枚举**直接对齐 `detector.rs cause_key()`** 现有稳定类型 key（CPU usage / Available memory /
  Memory paging / Network spike / Disk spike …），不另起炉灶，避免枚举臆造（R2）。
- 多因同发时按"信号强度 × 持续"排序，第一项为 `primary_cause`；`write_event` 一并写入
  `cause_kinds` / `primary_cause` JSON；`reader.rs` 反序列化。
- 落库时**顺带记录各 cause 的「首触时刻」与事件「onset 时刻」**（见 §3.1），供 F-RC6 零复刻复用。
- 旧事件 `cause_kinds` 为空时，用 `cause_key()` 把 `causes` 文本**可靠回填**为枚举（精确映射，非脆弱关键词）。
- **建议**：把 `spike_check` / 硬阈值判定抽成 `detect_core` 纯函数，service 落库与 F-RC12 what-if 共用，
  从根上消除两边阈值语义漂移（见 R6）。
- 依赖：detector 改造 + schema 迁移 + 重装服务。

#### F-RC2 — 磁盘真繁忙度 + 系统级信号（协同 P5-B）

- `collector.rs` 新增 PDH / System 计数器：`% Disk Time`、`Avg Disk sec/Transfer`、
  `% DPC Time`、`% Interrupt Time`、`Context Switches/sec`（复用 `DiskPdh` 模式）。
- `detector.rs`：用 `disk_busy_percent > 95` 或 `disk_avg_io_ms > 50` **替代**磁盘 B/s spike；
  新增 `DpcInterrupt` / `InterruptStorm` / `ContextSwitchStorm`（带阈值 + 滞回）。
- **范围澄清**：`Memory paging`（`page_reads_per_sec`，真实 swap 卡顿信号）**已在采**，F-RC2 不重复覆盖；
  本次只补磁盘繁忙度 + 系统级信号。价值：磁盘根因从"吞吐"升级为"繁忙度"，补齐系统级卡顿真信号。

#### F-RC3 — 前台窗口冻结检测（UiFrozen）

- `detector.rs` 用 `SendMessageTimeout(WM_NULL, 200ms)` 探前台窗口是否真无响应；超时标 `UiFrozen` cause。
- 区分"资源高但还能动"与"前台应用真卡死"——后者才是用户感知到的卡顿。
- **阻塞约束（修订）**：每 tick 调用 500ms 超时会腰斩 1Hz 采样。**改为只在"已触发其它 cause"的帧探一次**
  （既然已判定卡顿，多 200ms 无感），或放独立低优先线程 + 200ms 超时 + **每 2s 限频**，绝不进入采集热路径。

#### F-RC4 — 温度→降频根因（ThermalThrottle）

- **数据源修订**：`gpu_temp` 从未填充（`collector.rs:437`），不纳入。改用 `cpu_temp` **+ `cpu_freq_mhz` 掉档判据**
  （负载下 cpu_freq 不升反降 = 真降频信号，强于"仅温度高"）。
- `detector.rs` 新增 `ThermalThrottle` cause：温度 > 阈值 **且** 疑似降频（cpu_freq 掉档 / cpu_usage 不随负载线性升）。
- 价值：笔记本散热差→降频→卡顿是真根因链，现被完全忽略。

### 层级二 — 归因逻辑层（分析/运行时，GUI 只读侧）

#### F-RC5 — 主因判定 + 加权归因

- **权重修订（对齐 detector 严重度定义）**：`detector.rs determine_severity` 按 `causes.len()` 定级
  （1→minor/2→major/3→critical），severity 实质就是并发 cause 数。若再乘 `severity_weight` 等于对 cause 数
  **重复计数**。故权重改为 `w = duration_norm × 主因信号强度`（主因信号强度取该 cause 的越阈幅度/持续），不乘 severity。
- Top N 进程/类型榜改用 `Σw` 排序——1 次 critical 30s 冻屏的元凶，权重大于 10 次 minor 200ms。
- `primary_cause`（已落库）直接作为"主因"高亮，而非把 `cause_kinds` 平铺。

#### F-RC6 — 因果方向（领先-滞后相关）

- **落库前置（修订）**：spike 类 cause 是**滑动基线比率 + 滞回**（`spike_check`），非静态阈值；回算易漂移。
  改为 **detector 落库事件时顺带记录每个 cause 的「首触时刻」与事件「onset 时刻」**
  （onset ≈ `event.t - 3s`，因 `sustained_seconds=3s` 持续满足后才记录，见 §3.1）。分析侧直接复用，零复刻。
- 对每事件取 `[onset-30s, event.t]` 的 `samples`，用首触时刻定位各 `cause_kinds` 资源**谁先动**；
  明显早于其余且唯一 → **触发者**；晚于/伴生 → **放大器**。
- 典型输出：`MemLow` 先动 → 随后 `DiskBusy`（paging）升高 → 卡顿；则内存不足是根因，磁盘忙是放大器。
- **性能（修订）**：不逐事件发 SQL。打开分析页时**一次性 bulk 拉取范围内全部 samples**（与 F3 降采样同源），
  内存切片算 leading signal，避免数千事件 × 小查询拖垮 DB。

#### F-RC7 — 基线偏离判定

- **数据前提修订（重要）**：`collector.rs` 中**非卡顿帧跳过 `top_processes` 构建**（`collect_with(false)`），
  非卡顿样本 `top_processes` 为空，故**不能**用非卡顿 `samples.top_processes` 算基线。
- **改为事件侧聚合**：从历史事件 `culprits` / `snapshot.top_processes` 聚合某进程"作为元凶时的典型占用"，
  作为该进程的常态基线；归因时只把超出 `基线 × factor` 的 culprit 标"显著偏离"。
- 持续高占用（如浏览器常驻 30% CPU）标"常驻高占用（噪声）"并降权。
- 与 F-RC13 画像复用同一数据源。价值：过滤"谁都在跑"的常驻进程噪声，根因更聚焦。

#### F-RC8 — 多进程共现聚类

- 把每事件 `culprits` 的进程名集合做共现统计（频次 / Jaccard），输出高频"卡顿组合"
  （如 `浏览器 + Windows Update + 杀软`）。
- 可下钻到某组合参与的全部事件（高级模式）。价值：抓"组合效应"而非单一元凶。

#### F-RC9 — 因果链 / 级联归因

- 多 `cause_kinds` 同发时，按 F-RC6 的 `t_lead`（首触时刻）排序成有向链（根因 → 传导 → 表象）：
  例 `MemLow → DiskBusy(Paging) → Stutter`。
- 呈现为轻量链路图（节点= cause，边= 时间先后），替代平铺 `causes` 列表。

### 层级三 — 呈现 / 交互层（GUI，复用分析窗口）

#### F-RC10 — 单事件根因钻取卡（核心载体）

- **前置依赖（修订）**：`StutterEvent`（`types.rs`）当前**未带 `id`**，reader 读出后丢失事件主键，钻取卡无法精准关联。
  需给 `StutterEvent` 加 `id: i64`（或分析层单独 `SELECT id` 随行携带），否则无法关联到那一行（见 §3.1）。
- 点一条事件（携带 `id`）→ 卡片同时给出：
  1. **主因**（带置信度，见 F-RC11）；
  2. **前导资源曲线**（±60s，复用 F3 降采样 + plotters）；
  3. **参与进程及偏离基线幅度**（F-RC7 结果）；
  4. **因果链图**（F-RC9 结果）。
- 比 F3 全区间叠加精确，是根因 UI 的核心入口。

#### F-RC11 — 根因置信度

- **置信度修订（重要）**：多因并发是 major/critical 的**定义本身**（`causes.len()` ≥2 才 major），
  若"多因即低置信"会误伤所有高级事件。置信度应看**主因是否明显领先其余 cause**（强度/时间差），而非 cause 数量：
  - 主因信号强度 / 首触时刻明显领先其余 → 高置信；
  - 主因与次因强度接近、首触时刻重叠 → 低置信（标注"主因不显著，疑多因并发"）。
- UI 用色阶/文字标注，避免用户被低置信结论误导。
- **校准建议**：公式初值为经验值，建议用 notify 用户反馈 / 人工标注样本校准，否则只是装饰。

#### F-RC12 — 阈值敏感性 what-if

- 客户端用 `snapshot` 中**已存储的信号值** vs 用户可调阈值，重算"若阈值 X 是否会触发该 cause"。
- **不改 service**（保持只读契约），仅本地模拟，反向辅助调 `config.toml [detection]` 配置。
- 约束：模拟阈值语义必须与 `detect_core` 纯函数一致（复用 service 同套逻辑，见 §5.1 / R6）。

#### F-RC13 — 同类事件画像对比

- 按 `cause_kinds + culprit 集合 + duration 分桶` 聚类历史事件；对当前事件显示
  "匹配已知画像：进程 Y 典型卡顿"，辅助判断是已知元凶复发还是新情况。
- 与 F-RC7 共用事件侧聚合数据源（进程作为元凶时的典型占用画像）。

---

## 5. 技术方案

### 5.1 总体路线

- **检测/数据层**：改 `detector.rs` / `collector.rs` / `logger.rs` / `types.rs`，经 P5 部署约束（停服重装）生效。
- **分析层**：扩展 `crates/ui/src/analytics.rs` 新增纯函数（`leading_signal` / `baseline` / `cooccurrence` / `cause_chain` / `confidence`），全部带单元测试；UI 在 `analysis.slint` 现有窗口内新增"根因"区块/钻取卡。
- **`detect_core` 纯函数（关键）**：把 `spike_check` / 硬阈值判定抽成与 service 共用的纯函数，F-RC6 重算首触与
  F-RC12 what-if 都调用它，从根上消除"分析侧复刻 detector 数学"的漂移（见 R6/R8）。
- 图表、降采样、后台线程、本地时区、索引**全部复用** `卡顿分析界面-PRD.md` 既有方案，不重复造轮子。

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
- [ ] 分析侧全程只读 `stutter.db`，service 侧改动经重装生效，不破坏 P3 契约。
- [ ] 新增代码通过 `rtk cargo test` 且 `rtk cargo build --release` 零警告。

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
| R7 | 事件 `id` 缺失前置 | F-RC10 前给 `StutterEvent` 加 `id`（已列为前置依赖，§3.1） | 无 id 钻取卡无法精准关联 |
| R8 | 阈值语义漂移 | 抽 `detect_core` 纯函数，service 与 what-if 共用（见 §5.1/R6） | 影响 F-RC6/F-RC12 可信度 |

---

## 9. 实施里程碑

1. **M1 数据地基**：F-RC1 结构化 `CauseKind`+`primary_cause` 落库（含各 cause 首触时刻 / 事件 onset 落库、抽 `detect_core` 纯函数）；F-RC2 P5-B 信号；F-RC3 `UiFrozen`；F-RC4 `ThermalThrottle`（协同 P5 部署）；**`StutterEvent` 加 `id`**（F-RC10 前置）。
2. **M2 归因算法**：F-RC5（`duration × 主因强度`，不乘 severity）；F-RC6（落库 onset/首触 + bulk samples）；F-RC7（事件侧基线）；F-RC8 共现；F-RC9 因果链（`analytics.rs` 纯函数 + 单测）。
3. **M3 呈现**：F-RC10 钻取卡；F-RC11 置信度；F-RC12 what-if；F-RC13 画像对比（复用分析窗口）。
4. **M4 打磨**：旧库兼容、测试、零警告构建。

> 备注：M1 与 `TODO.md` §P5 检测精度优化强耦合，建议合并排期；M2/M3 可在 M1 数据就绪前用
> 现有 `causes` 文本 + `snapshot` 信号先做算法原型（降级路径），待结构化数据到位无缝切换。
