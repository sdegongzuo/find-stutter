# find-stutter 领域词汇表（CONTEXT）

> 本文件是领域术语表（glossary），只记录概念与边界，不记录实现细节。
> 实现层面的决策见 `docs/adr/`。

## 核心领域

- **卡顿（stutter / StutterEvent）**：系统某类资源出现**持续**饱和或异常，足以导致用户感知卡顿或前台窗口无响应。判定标准（信号、阈值、滞回、闸门）见《卡顿判断标准.md》。单次卡顿是一次「会话」，落库为一条 `StutterEvent`。
- **样本（Sample）**：服务每 tick 采集的一帧全量系统指标（CPU/内存/磁盘/网络/温度等），是判定的输入，也是事后分析与 what-if 重算的数据底座。
- **原因（cause）**：卡顿窗口内被确认的触发信号，分**压力类**（能直接造成无响应）与**非压力类**（纯吞吐 spike，只作附加记录不单独成事件）。
- **元凶（culprit）**：卡顿期间按 CPU/内存维度累积出的 top 进程，回答「谁干的」。
- **根因报告（root cause report）**：对某次卡顿的归因结论（可由人保存修订），与根因分析（F-RC1~16）对应。

## 进程与启动

- **服务（FindStutter 服务 / find-stutter-service）**：承载采集与检测的 Windows 服务（SCM、开机自启）。**唯一写库者**。GUI 崩溃/关闭不影响采集。
- **GUI（find-stutter）**：给人看的悬浮窗/进程列表/分析页。对 `stutter.db` **只读**（唯一例外：保存根因报告的窄写权），1Hz 心跳探测服务健康。
- **心跳（heartbeat）**：服务每 tick 写入 `service_heartbeat` 的存活信号；GUI/CLI 据此判定服务 Running / Stale / Stopped。
- **启动（launch）**：`find-stutter` 一条命令 = 拉起 GUI 并**确保服务在跑**（未装则经 UAC 安装并启动，已停则启动）。不存在需要人记住的第二条启动命令。
- **升级（upgrade）**：停服（释放 exe 锁）→ 重建 → 重装启动的流程，使新的检测逻辑进入运行态。

## 双界面（本次设计确立）

- **UI（界面轴·人）**：crates/ui —— 一切面向人类感知的呈现（悬浮窗、进程详情、分析图表）。
- **CLI（界面轴·agent）**：crates/cli —— 一切面向 coding agent 的机器可读查询（JSON、英文键、ISO8601 时间）。查询为主 + `status`；不做需要提权的控制。
- **分析纯函数（analytics）**：KPI/元凶榜/因果链等无界面依赖的聚合逻辑，**住在 core**，UI 与 CLI 共用，两边只是不同的呈现。
