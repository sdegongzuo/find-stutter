# 卡顿分析界面 — 产品需求文档（PRD）

> 版本：v0.2（评审修订）
> 起草日期：2026-08-09
> 状态：待评审
> 修订：纳入代码评审意见（时区口径 / F3 降采样 / F4 现实预期 / hover 静态图缓存 / 索引提前到 M2 / 验收补 rtk 与本地时区）
> 关联文档：`README.md`、`PLAN.md`、`TODO.md`、`crates/core/src/types.rs`、`crates/core/src/logger.rs`、`crates/ui/src/reader.rs`

---

## 1. 背景与目标

### 1.1 现状

`find-stutter` 当前的能力是「**实时**监控 + **实时**提醒」：

- 后台 Windows 服务每秒采集系统指标写入 `stutter.db`（WAL 模式），GUI 悬浮窗 1Hz 只读轮询显示最新指标，并在右下角显示「上次卡顿时间」。
- 卡顿事件已结构化落库（`stutter_events` 表含严重程度、触发原因、持续时长、事件瞬间资源快照、元凶进程列表），历史数据保留 30 天（`retention_days` 默认 30）。

### 1.2 痛点

- 悬浮窗只呈现「**最近一次**卡顿的时间」，无法回答：「**今天一共卡了几次？什么时候最频繁？是谁造成的？卡的时候 CPU/内存/磁盘到底是什么状态？**」
- `stutter.db` 里积累了大量高质量的历史卡顿数据，但**完全没有事后分析能力**，数据价值被闲置。
- 「进程详情页」解决了实时进程视图，但它是**当前快照**，不是「历史上哪些进程最常导致卡顿」的归因。

### 1.3 目标

在**现有 slint 桌面 GUI 内新增一个独立的「卡顿分析」页面/窗口**，让用户事后回看涨趋势、定位元凶、对齐资源拐点，把已采集的数据用起来。

### 1.4 非目标（明确不做）

- 不做实时告警改造（沿用现有悬浮窗 + 系统通知）。
- 不做卡顿检测算法改造（检测逻辑只在 service 进程跑，本页面只读）。
- 不做 Web 仪表盘 / 服务端（本期定位为单机桌面工具内的离线分析）。
- 不做跨机器聚合分析。

---

## 2. 目标用户与场景

采用**「基础模式 + 高级模式」双模式**设计：

| 模式 | 用户 | 信息密度 | 典型场景 |
| --- | --- | --- | --- |
| 基础模式 | 普通终端用户 | 低，结论导向 | 「今天卡得厉不厉害？主要是啥原因？」一句话结论 + 简单图表 |
| 高级模式 | 开发者 / 测试 | 高，可下钻原始数据 | 对齐资源拐点、按进程归因 Top N、查看每次卡顿的元凶与资源快照 |

切换方式：页面内一个「基础 / 高级」开关；高级模式额外显示原始数据表、明细钻取、细粒度筛选。

---

## 3. 数据模型（基于真实 schema）

> 以下字段均来自 `crates/core/src/types.rs` 与 `crates/core/src/logger.rs` 的实际建表语句，非设想。

### 3.1 `samples` 表（1Hz 时序，用于资源趋势）

| 字段 | 类型 | 说明 | 分析用途 |
| --- | --- | --- | --- |
| `timestamp` | TEXT (RFC3339) | 采样时间 | 时间轴 |
| `cpu_usage` | REAL | CPU 总使用率 % | 资源关联 |
| `mem_usage_percent` | REAL | 内存使用率 % | 资源关联 |
| `mem_available_mb` | INTEGER | 可用内存 MB | 资源关联 |
| `disk_read_bps` / `disk_write_bps` | INTEGER | 磁盘读写速率 B/s | 资源关联 |
| `net_sent_bps` / `net_recv_bps` | INTEGER | 网络收发速率 B/s | 资源关联 |
| `gpu_usage` | REAL(NULL) | GPU 利用率 %（可能为空） | 资源关联 |
| `cpu_temp` / `gpu_temp` | REAL(NULL) | 温度（可能为空） | 资源关联 |
| `process_count` / `thread_count` | INTEGER | 进程 / 线程数 | 辅助 |

> ⚠️ **待确认/约束**：P5 计划中设想的 `disk_busy_percent`、`disk_avg_io_ms`、`dpc_percent`、`interrupt_percent`、`context_switches_per_sec`、`top_processes` 等**细粒度字段尚未写入建表语句**（见 `TODO.md` §P5-B）。本期「资源关联」v1 只能基于上表中的 cpu/内存/磁盘速率/GPU/温度字段；如需磁盘繁忙度、DPC/中断等更贴近「真实卡顿」的信号，需先落地 P5-B（service 端改 schema + 重装服务）。

### 3.2 `stutter_events` 表（卡顿事件，用于归因/分类/趋势）

| 字段 | 类型 | 说明 | 分析用途 |
| --- | --- | --- | --- |
| `timestamp` | TEXT | 卡顿发生时间 | 时间趋势 / 对齐 |
| `duration_ms` | INTEGER | 卡顿持续时长 | 严重度量化 |
| `severity` | TEXT | `minor` / `major` / `critical` | 分级统计 |
| `causes` | TEXT(JSON 数组) | 触发原因字符串，如 `"CPU usage 95.0% > 90.0%"` | **卡顿类型细分**（见 §6.4） |
| `snapshot` | TEXT(JSON) | 事件瞬间全量 `Sample` | 资源关联（事件点状态） |
| `culprits` | TEXT(JSON 数组) | 元凶进程 `ProcessBrief`：`pid`/`name`/`cpu_usage`/`mem_used_mb` | **进程归因 Top N** |

### 3.3 数据量级与性能提示

- 1Hz × 30 天 ≈ **259 万行** `samples`。分析查询必须**带时间范围**且依赖时间戳索引。
- 当前建表**没有** `samples.timestamp` / `stutter_events.timestamp` 的索引（建表语句仅主键自增）。**建议本期新增** `CREATE INDEX IF NOT EXISTS idx_samples_ts ON samples(timestamp)` 与 `idx_events_ts ON stutter_events(timestamp)`，否则按时间范围聚合会全表扫描。
- ⚠️ **索引落地归属（实施修订）**：分析页严格「只读 stutter.db」（§1.4/§8），以 `SQLITE_OPEN_READ_ONLY` 打开连接，`CREATE INDEX` 在只读连接下必然失败。故真正的索引由 **service 端建表逻辑 `crates/core/src/logger.rs`** 以 `CREATE INDEX IF NOT EXISTS` 创建（service 重启/重装即生效），分析页只读消费；`analytics.rs::ensure_indexes` 仅在连接可写时真正执行，只读连接下对 `ReadOnly` 错误优雅降级为全表扫描（不影响功能，仅大范围查询稍慢，待 service 端索引生效消除）。
- 旧库（P5 前）`stutter_events` 无 `culprits` 列，读取需做列存在性探测（reader 已有先例，沿用即可）。
- ⚠️ **时区口径**：所有 `timestamp` 以 `to_rfc3339()` 存入，是 **UTC** 时间（`+00:00` 后缀）。
  分析页所有时间维度（时间轴、分桶、KPI「今日 / 高峰时段」、自定义范围）**必须按本地时区展示**，
  否则 UTC+8 用户会整体偏移 8 小时。推荐在 Rust 侧把 `timestamp` 解析为 `DateTime<Local>` 后分桶，
  而非直接 `strftime` 分 UTC 小时；同时与悬浮窗 `event_count_today`（用 `local_today_bounds()`
  本地零点→现在，见 `crates/core/src/logger.rs`）的口径对齐——分析页 `load_kpi_today` / `TimeRange::Today`
  共用同一边界——两处「今日卡顿 N 次」必须一致，否则用户会困惑。

---

## 4. 功能需求

### F1 — 时间分布与趋势

- 以时间桶（默认按「小时」聚合；高级模式可选 15 分钟 / 天）统计卡顿**次数**与**时长**。
- 展示折线/柱状图：X 轴时间，Y 轴次数；叠加 severity 堆叠（minor/major/critical 不同色）。
- 顶部 KPI 卡片（基础模式核心）：「今日卡顿 N 次」「最严重一次持续 Xs」「高峰时段 HH:00」。
- 数据来源：聚合 `stutter_events`，按 `timestamp` 分桶 `COUNT(*)` + `SUM(duration_ms)` + `severity` 分组。

### F2 — 卡顿归因（按进程/应用 Top N）

- 解析每条事件的 `culprits` JSON，按进程 `name`（聚合同名不同 PID）统计：
  - 「作为元凶出现在 N 次卡顿中」
  - 「关联卡顿累计时长」
  - 「最高单次 CPU 占用 / 内存占用」
- 展示 Top 10 横向条形榜（基础模式 Top 5 + 一句话结论：「XX 是今天最大的卡顿元凶」）。
- 点击某进程 → 钻取到「该进程参与的所有卡顿事件列表」（高级模式）。
- 数据来源：读取 `stutter_events.culprits` 并在内存中按 name 聚合。

### F3 — 系统资源关联（CPU / 内存 / IO）

- 在时间趋势图上叠加**资源曲线**：选择某时间段，绘制 `samples` 的 `cpu_usage` / `mem_usage_percent` / `disk_*_bps` 折线（多轴或归一化叠加）。
- **卡顿事件标记**：在资源时间轴上用竖线/圆点标出每次卡顿发生时刻，直观看「卡顿是否对齐 CPU 尖峰 / 内存见底 / 磁盘风暴」。
- 也可直接利用每条事件的 `snapshot`（事件瞬间资源状态）做「卡顿时平均 CPU = X%」的汇总（无需回查 samples）。
- 高级模式：可勾选显示的具体指标、切换对数轴、查看事件点 hover 详情（该次 snapshot 全字段）。

### F4 — 卡顿类型细分

- 按**卡顿原因类型**统计占比（饼图/条形）：
  - 例：CPU 过高、内存不足、磁盘繁忙、GPU 过高、网络突增、DPC/中断等。
- 数据基础见 §6.4「待确认」——`causes` 目前是**自由文本字符串数组**，需先做「文本 → 类型」归类。

### F5 — 基础 / 高级模式

- 页面内开关切换。
- 基础模式：KPI 卡片 + 2~3 张结论性图表 + 简短文字结论，隐藏原始表与钻取。
- 高级模式：全部图表 + 原始事件表（可排序/筛选）+ 时间范围自定义 + 进程钻取 + 资源曲线细调。

### F6 — 入口与导航

- 复用现有「进程详情页」的入口模式（右键菜单 → 独立置顶窗口）：
  - 悬浮窗**右键菜单**新增「卡顿分析」项（与「进程详情」并列）。
  - 系统**托盘菜单**同步新增「卡顿分析」项。
- 窗口为独立 slint 窗口，置顶、可关闭；不影响悬浮窗常驻监控。
- （备选）悬浮窗常驻区增加一个常驻入口按钮。

### F7 — 时间范围与刷新

- 时间范围选择器：今日 / 近 7 天 / 近 30 天 / 自定义（受 `retention_days` 上限约束）。
- 分析页**非 1Hz**：打开时查询一次 + 「刷新」按钮；高级模式可选「自动刷新（默认 30s）」。
- 默认范围：**今日**（最快、最常用）。

### F8 — 数据导出（高级模式）

- 将当前筛选范围内的卡顿事件导出为 CSV（字段：时间、时长、严重程度、原因、元凶进程列表），与现有 `export` CLI 互补（CLI 导出的是 samples 时序，本功能导出事件归因）。
- 表头与说明用中文（遵循 `AGENTS.md`）。

---

## 5. 信息架构与页面草图

```
┌─────────────────────────────────────────────────────────────┐
│  卡顿分析                              [基础|高级]  [今日▾] [刷新]│
├─────────────────────────────────────────────────────────────┤
│  KPI 卡片：今日 N 次 | 最严重 Xs | 高峰 HH:00 | 头号元凶 XX    │
├──────────────────────────┬──────────────────────────────────┤
│  趋势图（次数+severity堆叠）  │  卡顿类型占比（饼/条形）        │
│  ───────────────────────  │  ─────────────────────────────  │
│  资源关联图（CPU/内存/磁盘   │  元凶进程 Top N（条形榜）        │
│  曲线 + 卡顿事件标记）        │                                │
├──────────────────────────┴──────────────────────────────────┤
│  （高级模式）原始事件表：时间 | 时长 | 等级 | 原因 | 元凶      │
│  点击行 → 钻取该次快照全字段                    [导出 CSV]     │
└─────────────────────────────────────────────────────────────┘
```

- 布局参考现有进程详情页的窗口风格（置顶、可缩放、深色/浅色跟随皮肤）。
- 图表区占主视觉，表格在高级模式展开于底部。

---

## 6. 技术方案

### 6.1 总体路线（与现有架构一致）

沿用 P3 只读模式 + 进程详情页窗口模式：

- 新增 slint 组件：`crates/ui/ui/analysis.slint`（页面布局）。
- 新增 Rust 窗口封装模块：`crates/ui/src/analysis.rs`（类比 `process_list.rs` 的 `ProcessListWindow`），持有 `Slint` 窗口 + 数据加载/刷新逻辑。
- 新增只读分析查询层：`crates/ui/src/analytics.rs` 或扩展 `DbReader`，提供聚合查询函数（见 §7）。
- 入口：在 `lib.rs` 右键菜单 `NativeMenuCmd` 增加 `Analysis` 分支，复用 `process_win` 的「首次创建 + 复用 + refresh」模式；`window.rs` 的 `show_context_menu` 与托盘菜单同步加项。
- **全程只读** `stutter.db`，不引入采集/写库逻辑，不破坏 P3 服务化契约。

### 6.2 图表渲染方案（⚠️ 关键技术分叉，待确认）

slint 1.x **没有内置图表控件**。三个候选：

| 方案 | 做法 | 优点 | 缺点 |
| --- | --- | --- | --- |
| **A. plotters 渲染到图片（推荐）** | 用纯 Rust 的 `plotters` 把图表画到内存 `RGBA` 缓冲，转 `slint::Image`（`SharedImageBuffer`）显示 | 离线、无原生依赖、折线/柱状/饼图都现成、开发量小 | 静态图，交互（hover）需自己重绘 |
| B. slint 原生图元手绘 | 用 `Path`/`Rectangle` 在 slint 里拼折线/柱状 | 可与 slint 主题/皮肤统一、可交互 | 开发量大、饼图/坐标轴都要自己算 |
| C. 嵌入 WebView | 用 web 技术画图 | 生态成熟 | slint 无官方 webview，需额外 crate，体积/复杂度高，偏离现有技术栈 |

**建议默认 A**（plotters → Image）。hover/钻取交互**不必重绘整图**：plotters 把图表一次性渲染为静态 `slint::Image` 缓存，slint 端叠加透明 `TouchArea`，命中分区后用 tooltip 覆盖层（十字线 + 文本）显示该点数据——交互只更新覆盖层、不重绘位图。位图渲染（尤其近 30 天 259 万点）建议放后台线程，渲染完成后把 `Image` 传回 UI 线程显示，避免分析页打开时窗口冻结。若后续交互要求高再考虑 B。请在评审时确认。

### 6.3 性能与刷新

- 所有聚合查询**必须带 `WHERE timestamp BETWEEN ? AND ?`**，范围默认今日（≤86400 行 samples）。
- 新增时间戳索引（见 §3.3）。
- 聚合在打开/刷新时**一次性**执行，结果缓存在窗口状态里；高级模式自动刷新默认 30s，且只对当前时间范围重查。
- 进程归因（解析 `culprits`）在内存聚合，数据量 = 时间范围内事件数（通常几千以内），开销可控。
- ⚠️ **资源曲线降采样（F3 必做）**：1 天 ≈ 86400 点、30 天 ≈ 259 万点，plotters 直接绘制会卡 UI 线程。绘制前**按显示像素宽度降采样**（每像素桶取 min/max/avg 构成折线，或降点采样），近 30 天范围必须对 `samples` 做分桶聚合后再喂给图表，而非回查全量原始点。

### 6.4 卡顿类型细分的数据基础（⚠️ 待确认）

当前 `causes` 是自由文本数组，样例形如 `"CPU usage 95.0% > 90.0%"`、`"Disk Spike"`、`"Mem Low"` 等，**没有统一枚举**。实际产出的文案以 `detector.rs` 当前检测逻辑为准（主要源于 CPU 阈值、内存不足、磁盘/网络 spike 等），不要臆造枚举值。做可靠的「类型占比」有两种路线：

- **路线 1（推荐，需小改 service）**：在 `DetectionConfig`/检测器里定义结构化 `CauseKind` 枚举（如 `CpuHigh` / `MemLow` / `DiskBusy` / `DiskSpike` / `GpuHigh` / `NetworkSpike` / `DpcInterrupt` / `ContextSwitchStorm`），`StutterEvent` 新增 `cause_kinds: Vec<CauseKind>`，`write_event` 一并落库。分析页直接按枚举聚合，语义干净。**代价**：改检测器 + schema 迁移 + 需重装服务（P5 部署约束）。
- **路线 2（零改造，向后兼容）**：分析页侧做「文本 → 类型」关键词归类（正则/包含匹配），对旧库也有效。**代价**：脆弱、原因文案一改就漏归类。

- ⚠️ **F4 现实预期**：P5-A（`enable_network_spike` 默认关、降低网络误报）尚未落地，当前卡顿大量由「网络 spike」触发（实测单日 13 次卡顿 100% 由网络 spike 触发），故 F4 饼图短期会「网络一家独大」；`DPC/中断`、`上下文切换` 等类型在当前数据源中**不存在**（依赖 P5-B 未落地）。路线 2 的关键词表须按 `detector.rs` 当前实际文案编写，并在界面注明「粗糙归类、随检测器文案可能漂移」。

**建议**：本期先用路线 2 跑通界面（零改造、可立即验证），同时在 PRD 备注路线 1 作为 P5 协同项；待检测器结构化改造落地后无缝切换。请确认是否接受「先 2 后 1」。

---

## 7. 数据查询设计（SQL 草稿）

> 以下为草案，落地时按 §6.4 选择的 cause 路线微调。

**F1 时间趋势（按小时分桶）：**
```sql
-- 按本地时区分桶（见 §3.3 时区口径）；亦可选择在 Rust 侧解析 DateTime<Local> 后分桶
SELECT strftime('%Y-%m-%d %H:00', datetime(timestamp, 'localtime')) AS bucket,
       COUNT(*)                                            AS cnt,
       SUM(duration_ms)                                    AS total_ms,
       SUM(CASE severity WHEN 'critical' THEN 1 ELSE 0 END) AS c_crit,
       SUM(CASE severity WHEN 'major'    THEN 1 ELSE 0 END) AS c_major,
       SUM(CASE severity WHEN 'minor'    THEN 1 ELSE 0 END) AS c_minor
FROM stutter_events
WHERE timestamp BETWEEN ?1 AND ?2
GROUP BY bucket ORDER BY bucket;
```

**F2 进程归因 Top N（内存解析 culprits）：**
```sql
-- 先取时间范围内事件（含 culprits 列），在 Rust 侧 JSON 解析并按 name 聚合
SELECT id, timestamp, duration_ms, severity, culprits
FROM stutter_events
WHERE timestamp BETWEEN ?1 AND ?2
ORDER BY timestamp;
-- Rust: 对每个 culprit 累加 (出现次数, 累计时长, 最大 cpu/mem) → 按 name 排序取 Top N
```

**F3 资源曲线（对齐用）：**
```sql
SELECT timestamp, cpu_usage, mem_usage_percent,
       disk_read_bps, disk_write_bps, gpu_usage
FROM samples
WHERE timestamp BETWEEN ?1 AND ?2
ORDER BY timestamp;
-- 同时在同范围取 stutter_events.timestamp 作为标记点
```

**F4 类型占比（路线 2 关键词归类在 Rust 侧完成；路线 1 直接 GROUP BY cause_kind）：**
```sql
SELECT causes FROM stutter_events
WHERE timestamp BETWEEN ?1 AND ?2;
-- Rust: 解析数组 → 归类 → 计数
```

---

## 8. 验收标准

- [ ] 从悬浮窗右键菜单 / 托盘菜单可打开「卡顿分析」独立窗口，关闭后不影响悬浮窗监控。
- [ ] 默认「今日」范围下，趋势图、类型占比、元凶 Top N、KPI 卡片均能正确渲染（与 `stutter_events` 实际数据一致）。
- [ ] 切换「基础/高级」模式，高级模式出现原始事件表 + 导出按钮 + 时间范围自定义。
- [ ] 资源关联图在时间轴上标出卡顿事件点，且能看出与 CPU/内存尖峰的对应关系。
- [ ] 自定义时间范围（如近 7 天）查询在合理耗时内返回（依赖 §3.3 索引）。
- [ ] 旧库（无 `culprits` 列）打开分析页不崩溃，缺失字段回退为空/0。
- [ ] 全程只读 `stutter.db`，不新增写操作、不干扰后台服务。
- [ ] 新增代码通过 `rtk cargo test -p find-stutter-ui` 且 `rtk cargo build --release` 零警告（所有 cargo 命令遵循 rtk 包裹约定）。
- [ ] 分析页所有时间维度（时间轴 / 分桶 / KPI「今日」「高峰时段」/ 自定义范围）按**本地时区**展示，与悬浮窗 `event_count_today` 的「今日卡顿 N 次」口径一致。
- [ ] 近 30 天资源曲线已按像素宽度降采样，打开 / 刷新时窗口不冻结（位图渲染不阻塞 UI 线程）。

---

## 9. 风险与待确认清单

| # | 事项 | 建议默认 | 影响 |
| --- | --- | --- | --- |
| R1 | 图表渲染方案 | plotters → Image（§6.2 A） | 决定开发量与交互上限 |
| R2 | 卡顿类型数据基础 | 先关键词归类（§6.4 路线 2），结构化枚举作后续协同 | 决定 F4 准确度与是否改 service |
| R3 | 是否新增时间戳索引 | 是（§3.3） | 影响大范围查询性能 |
| R4 | 默认时间范围 | 今日 | 体验与性能平衡点 |
| R5 | 磁盘繁忙度/DPC 等深层信号 | 依赖 P5-B 落地，本期不做 | 限制 F3 深度 |
| R6 | 入口形式 | 右键菜单 + 托盘菜单（复用进程详情页模式） | 一致性 |
| R7 | 时区口径 | 按本地时区展示，并与悬浮窗 `event_count_today` 对齐 | 影响时间轴 /「今日」/ 高峰时段正确性，否则偏移 8h |
| R8 | 资源曲线降采样 | F3 按像素宽度 min/max/avg 桶降采样 + 后台线程渲染 | 影响大范围查询流畅度与 UI 响应 |

---

## 10. 实施里程碑（建议）

1. **M1 骨架**：`analysis.slint` + `analysis.rs` 窗口封装 + 右键/托盘入口 + 只读查询层（F6/F7），空页面跑通。
2. **M2 趋势与 KPI（F1）**：时间分桶聚合（本地时区）+ plotters 折线/柱状 + KPI 卡片；引入 `ensure_indexes` 骨架（只读连接下对 `ReadOnly` 错误优雅降级，真正索引由 service 端 `logger.rs` 建表逻辑创建，见 §3.3）。
3. **M3 归因与类型**（F2/F4）：culprits 内存聚合 Top N + 原因归类占比。
4. **M4 资源关联**（F3）：samples 曲线 + 事件标记叠加。
5. **M5 双模式与导出**（F5/F8）：基础/高级开关、原始事件表、CSV 导出、皮肤适配。
6. **M6 打磨**：性能复核（降采样生效）、旧库兼容、测试、零警告构建。

> 备注：M3 的 F4 若采用 §6.4 路线 1，则需与 `TODO.md` §P5 的检测器结构化改造协同排期。
