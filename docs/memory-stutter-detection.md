# 内存卡顿判定方案

本文件定义 find-stutter 如何判断「由内存引起的卡顿」，以及各指标的取舍理由。
所有判定逻辑位于 `crates/core/src/detector.rs` 的 `check_hard_thresholds`；采集位于
`crates/core/src/collector.rs`；阈值配置位于 `config.toml`。

## 核心原则

内存类卡顿的因果信号是 **「压力 / 即将耗尽」**，而不是「用了多少」。

- 谁会让操作系统被迫把页面换出到磁盘、注入一次磁盘 I/O 延迟，谁才是真信号。
- 静态的「占用百分比 / 已用绝对值」只有在逼近上限、开始产生换页活动时，才会真正让人感到卡顿。

据此，判定采用**多个互补口径的「或」关系**：任一成立即记为内存压力（对应一条 cause，
参与 `determine_severity` 的计数定级）。

## 触发口径

| # | 信号 | 字段 | 阈值（config.toml） | 口径性质 | 作用 |
|---|------|------|---------------------|----------|------|
| 1 | 物理内存使用率 | `mem_usage_percent` | `mem_threshold_percent = 90.0`（`>`） | 百分比 | **主信号**。覆盖「大内存机器可用 MB 仍高、但使用率已爆表」的漏报（如 32G 机用到 95% 时可用仍 >500MB）。这是本项目最初漏报 bug 的根因。 |
| 2 | 物理可用内存绝对值 | `mem_available_mb` | `mem_threshold_mb = 500`（`<`） | 绝对下限 | 兜底小内存机器：可用内存绝对不足时直接触发。 |
| 3 | 提交电荷比例 | `commit_bytes / commit_limit` | `commit_threshold_percent = 90.0`（`>`） | 瞬时比值 | 已提交虚拟内存接近提交上限（= 物理内存 + 页面文件）时，系统弹「内存不足」并强制分页，**比「可用物理内存归零」更早预警**。 |
| 4 | 分页活动速率 | `page_reads_per_sec` | `page_reads_threshold = 50.0`（`>`） | **速率（流量）** | **阶段 C**。真正的 swap 卡顿信号：物理内存耗尽时 OS 把页从 pagefile 换入，每次换页注入磁盘 I/O 延迟。 |

口径 1/2/3 为瞬时判断、无滞回（与既有硬阈值一致）；口径 4 为瞬时速率判断、无滞回。
四个口径互补、互为 OR，覆盖以下各自盲区：

- 大内存机「相对量爆了、绝对量没爆」→ 口径 1；
- 小内存机「绝对量爆了」→ 口径 2；
- 物理没满但提交（含 pagefile 预留）满了 → 口径 3；
- 物理/提交都没满、但已在实际换页（抖动） → 口径 4。

## 为什么移除 swap 使用率（存量）触发

`swap_usage_percent`（已用 pagefile / 总 pagefile）曾作为硬触发阈值，现**降级为仅展示**：

- **存量 ≠ 压力**：Windows 会预提交 / 预留 pagefile 空间，pagefile 用了 60% 但这些页静止
  （在 standby / modified 列表、没被主动访问）时完全不卡；反之 RAM 将满但 commit 还没大量
  落到 pagefile 时也会开始抖。
- 它既不因果、又易误报，还会让 `determine_severity` 按 causes 计数把一次轻微卡顿**虚高成 Critical**。
- 现仍继续采集并在 UI / 统计中展示 `swap_usage_percent`，仅不参与判定、不影响 severity。

## 阶段 C：分页速率（Page Reads/sec）—— 真正的 swap 卡顿信号

- 计数器：`\Memory\Page Reads/sec`（PDH 速率计数器），采集见 `collector.rs` 的 `PagingPdh`。
- 含义：每秒因硬页错误（hard page fault）而从磁盘（含 pagefile）读入的页数。它直接度量
  「换页活动强度」，而非「已经用了多少 pagefile」。
- 阈值 `page_reads_threshold`（默认 50.0 /s）可调；正常负载通常 < 10/s，抖动（thrashing）时
  飙升到数百 / 数千 /s。
- 作为独立 cause `"Memory paging {x}/s > {y}/s"`，归类到分析页饼图的「内存分页」。

> 取舍说明：口径 4 为瞬时速率判断、无滞回。单 tick 瞬时尖峰仅会向进行中的卡顿追加一条
> cause，不会单独凭一次尖峰就记录一次卡顿（需 `sustained_seconds` 持续才落库）；只有持续高
> 分页才会正确触发一次卡顿记录。因此瞬时尖峰基本无害。

## 阈值与可调性

所有阈值集中在 `config.toml`，不在代码中写死，便于按机型 / 场景调参：

```toml
# 或：内存使用率超过该百分比（%）即视为潜在卡顿
mem_threshold_percent = 90.0
# 或：可用内存低于该值（MB）即视为潜在卡顿
mem_threshold_mb = 500
# 提交电荷（committed / limit）超过该百分比（%）即视为潜在卡顿
commit_threshold_percent = 90.0
# 分页活动速率（Page Reads/sec）超过该值（/s）即视为换页抖动
page_reads_threshold = 50.0
# （swap_usage_percent 仅展示，不再参与判定）
```

## 数据落库

`crates/core/src/logger.rs` 的 `samples` 表持久化上述全部字段
（`mem_usage_percent`、`mem_available_mb`、`commit_bytes`、`commit_limit`、`page_reads_per_sec`、
`swap_usage_percent`），并随 `export_csv` 导出，供分析页与离线排查使用。
