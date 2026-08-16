# find-stutter

Windows 桌面悬浮窗，实时监控系统卡顿（CPU / 内存 / 磁盘 / 网络 / GPU），
并在检测到卡顿时记录到本地 SQLite 数据库，便于事后回溯「刚才为什么卡了」。

## 目录

- [快速开始](#快速开始)
- [功能特性](#功能特性)
- [架构](#架构)
- [安装与构建](#安装与构建)
  - [构建产物](#构建产物)
  - [开发者注意](#开发者注意)
- [使用指南](#使用指南)
  - [服务管理与排障](#服务管理与排障)
  - [交互操作](#交互操作)
  - [进程详情页](#进程详情页)
- [命令行参考](#命令行参考)
- [配置](#配置)
- [开发指南](#开发指南)
- [已知限制](#已知限制)

## 快速开始

最短路径：构建后只记一条命令 `find-stutter`——启动悬浮窗并自动确保后台采集服务在跑
（首次运行自动弹 UAC 注册并启动服务，之后开机自启）。

```bash
# 1. 构建（需 Windows + MSVC target 的 Rust 工具链）
rtk cargo build --release

# 2. 启动（唯一入口：GUI + 自动确保服务；CLI 子命令也走同一个 exe）
target/release/find-stutter.exe
```

之后每次只需双击 `find-stutter.exe`；若服务未运行，GUI 会自动尝试启动。
升级二进制 / 改判定逻辑后，用 `find-stutter upgrade` 一条命令完成停服 → 重建 → 重装启动，
见 [UPGRADE.md](./UPGRADE.md)。

## 功能特性

- **透明置顶悬浮窗**：始终浮在最上层，不抢焦点，CJK 中文字体正常显示。
- **实时指标**：上传/下载速率、CPU 使用率、内存占用、GPU 利用率、磁盘读写速率。
- **卡顿检测引擎**：基于阈值 + 突变检测，区分 `minor` / `major` / `critical` 三级严重程度。
- **无重影拖动**：在窗口上按住拖动即可移动（TouchArea + set_position 实时跟随），无重影/闪烁。
- **右键菜单**：暂停/恢复监控、进程详情、点击穿透、退出。
- **点击穿透模式**：窗口鼠标事件完全穿透（看得到点不到），右键菜单「点击穿透」开启。
- **SQLite 持久化**：采样与卡顿事件写入 `stutter.db`（WAL 模式，读写并发无锁）。
- **服务化架构**：独立 Windows 服务做采集写库，GUI 只读 SQLite 轮询；服务停止时 UI 右上角
  服务角标变红「● 服务已停止」并禁用暂停按钮。
- **UAC 自动提权**：首次启动 GUI 时若服务未注册 / 未运行，自动弹 UAC 申请管理员权限完成
  `install + start`，用户只需点「是」即可。
- **系统托盘图标**：后台线程 + win32 消息循环，右键菜单「显示/隐藏悬浮窗 / 暂停/恢复 / 退出」，
  左键单击 = 显示/隐藏；失败不阻塞 GUI。
- **配置 / 皮肤热加载**：监听 `config.toml` 与 `skins/` 目录，保存即生效（皮肤名变更会重载皮肤，
  皮肤颜色/字号实时更新）。
- **卡顿通知弹窗**：检测到新的 Major/Critical 卡顿时弹 Windows 原生气泡通知（toast），
  `[notifications]` 配置开关与最低等级。
- **任务栏嵌入**：`config.toml [ui] taskbar = true` 启用横向窄条伪任务栏窗口
  （默认底部中央，可拖到任务栏位置）。
- **Agent CLI**：`find-stutter` 子命令输出单行 JSON（卡顿事件 / 样本区间 / 聚合分析 /
  配置 / 服务状态 / 进程快照），供 coding agent 与脚本直接消费；采样数据可导出 CSV。
- **进程详情页**：右键菜单「进程详情」打开任务管理器风格进程列表——同名进程按 PPID
  聚合分组（孙进程扁平化到所属 root），显示 PID / 名称 / CPU / 内存（提交大小）/
  物理内存（专用工作集）/ 磁盘 / 网络 / 累计网络；点击列头排序、聚合行点击展开/收起、
  关键字 / PID / 端口号搜索、双击行打开进程详情面板（路径 / 命令行 / 线程 / 句柄 /
  内存明细等，文本可拖选 + Ctrl+C 复制，标题栏「复制」按钮全量复制）。
- **Chromium 家族标注**：进程详情页对 msedgewebview2.exe / msedge.exe /
  chrome.exe 按任务管理器口径标注——内嵌 WebView2 的浏览器进程显示宿主
  「WebView2: MarkFlowy」，子进程显示角色「WebView2 实用工具: Network Service」、
  「Chrome GPU 进程」等（按命令行 --type / --utility-sub-type 解析 + 父进程链）；
  搜索框同样支持按这些展示名查找（输入宿主名即可找到对应 WebView2 组）。
- **WebView2 按宿主分组**：msedgewebview2.exe 进程按宿主应用分组——同一宿主
  的全部 WebView2 进程（含多个浏览器实例）合并为一个组（如「WebView2: MarkFlowy」），
  不同宿主各自独立成组；与宿主自身的非 WebView2 进程互不混组。

## 架构

```
                ┌──────────────────────────┐
                │  find-stutter-service    │  Windows 服务
                │  (后台常驻, 开机自启)     │
                │  Collector + Detector    │  ← 每秒采集 / 检测
                │  + touch_heartbeat()     │  ← 写心跳 (id=1)
                │  + write_sample()        │  ← 写 sample 表
                │  + write_event()         │  ← 写 stutter_events 表
                └─────────────┬────────────┘
                              │ SQLite WAL
                              ▼
                       ┌──────────────┐
                       │ stutter.db   │  ← shared file
                       └──────┬───────┘
                              │ 1Hz SELECT (只读)
                              ▼
                ┌──────────────────────────┐
                │  find-stutter (UI)        │  用户启动时手动运行
                │  DbReader + 1Hz timer     │  ← 读 summary / heartbeat / events
                │  OverlayState             │  ← 显示「● 服务运行中」
                │  暂停按钮 / 服务角标     │   服务停止时变红禁用
                └──────────────────────────┘
```

**WAL 模式**保证服务持续写、UI 持续读互不阻塞；UI 不再自采，崩溃也不影响后台服务。

## 安装与构建

### 构建产物

- `target/release/find-stutter.exe`（唯一入口：无参数 = GUI 悬浮窗；子命令 = agent CLI 查询）
- `target/release/find-stutter-service.exe`（Windows 服务）

> ADR-0001 后不再有独立的 `find-stutter-ui.exe`——GUI 库（`crates/ui`）只被
> `find-stutter.exe` 链接，不再单独产出可执行文件。

### 开发者注意

> 单独改 UI 代码后请用 `rtk cargo build --release -p find-stutter` 重新编译 GUI 入口
> （`-p find-stutter-ui` 只编译 ui 库本身，不会更新用户实际运行的 `find-stutter.exe`）。

## 使用指南

### 服务管理与排障

**自动启动结果 → GUI 表现**（状态条）：

| 自动启动结果 | GUI 表现 |
| --- | --- |
| `AlreadyRunning` / `Started` | 1 秒后状态条变绿「● 服务运行中」 |
| `Skipped` | 服务自动启动已关闭（环境变量/配置），状态条显示服务状态 |
| `NotRegistered` / `ExeNotFound` | 状态条变红「● 服务未注册（请运行 find-stutter-service install）」 |
| `StartFailed` | 状态条变红「● 服务已停止」，日志提示手动 `start` |

**跳过自动启动（自动测试 / CI 环境）**：GUI 每次启动都会尝试检测服务，服务未注册/停止时
会弹 UAC 授权。不想被弹窗打断时，二选一即可完全跳过自动启动：

```bash
# 方式 1：环境变量（推荐，不改配置）
set FIND_STUTTER_SKIP_SERVICE=1 && find-stutter.exe

# 方式 2：配置文件
# config.toml → [ui] auto_start_service = false
```

**手动管理**（自动 `start` 失败，最常见原因：当前用户无 admin 权限）：

```bash
# 用管理员身份打开 cmd，cd 到 target/release
find-stutter-service.exe start      # 或 sc start FindStutter
find-stutter-service.exe status     # 查询；退出码 0=在跑，非 0=未运行/未注册
find-stutter-service.exe stop       # 或 sc stop FindStutter
find-stutter-service.exe uninstall

# 重启服务（升级二进制推荐直接用 CLI 子命令，一条命令完成停服 → 重建 → 重装启动）
find-stutter.exe upgrade               # 或 --no-build 跳过构建只重装
# 也可手动：sc stop FindStutter && sc start FindStutter
# 或：find-stutter-service.exe install-start
```

> **SCM 服务名**：`FindStutter`，显示名 `Find Stutter Monitor`。

### 交互操作

| 操作 | 效果 |
| --- | --- |
| 拖动窗口 | 移动悬浮窗（实时跟随，无重影） |
| 单击右列暂停按钮 | 暂停 / 恢复监控（暂停时指标冻结，显示「⏸ 已暂停」） |
| **右键窗口** | **弹出菜单：暂停/恢复监控、进程详情、点击穿透、退出** |
| 右键托盘图标 | 托盘菜单：显示/隐藏悬浮窗、暂停/恢复、退出 |
| 左键单击托盘图标 | 显示 / 隐藏悬浮窗 |
| 右键菜单「点击穿透」 | 开启点击穿透模式（穿透时鼠标无效；退出需重启，见[已知限制](#已知限制)） |
| 右上角服务角标 | 显示「● 服务运行中（绿）/ 已停止（红）」，服务断开时变红并禁用暂停按钮 |
| 修改 config.toml / skin.toml | 保存后热加载（皮肤颜色/字号实时生效；db 路径等需重启生效） |

### 进程详情页

| 操作 | 效果 |
| --- | --- |
| 点击列头 | 切换排序列 / 方向（PID / 名称 / CPU / 内存 / 物理内存 / 磁盘 / 网络 / 累计网络） |
| 点击聚合行 | 展开 / 收起该组的子进程（服务宿主 / 子进程） |
| 搜索框输入 | 按名称 / PID / 端口号过滤（保留组结构） |
| 双击进程行 | 打开进程详情面板（路径 / 命令行 / 用户 / 线程数 / 句柄数 / 启动时间 / 内存明细） |
| 详情面板拖选文本 | 选中部分内容，Ctrl+C 复制 |
| 详情面板「复制」按钮 | 一键复制全部详情文本 |

## 命令行参考

### `find-stutter.exe`（唯一入口：无参数 = GUI；子命令 = agent CLI）

无参数（或 `run`）启动悬浮窗监控（只读 stutter.db，自动确保服务在跑）。
面向 coding agent 的子命令输出**单行紧凑 JSON**（英文键、ISO8601 时间，
便于 jq 管道）；`--from/--to` 支持 RFC3339 / `YYYY-MM-DD HH:MM:SS` /
`YYYY-MM-DD`（无时区后缀按本地时区解释），缺省为「本地今日零点 → 现在」。

| 命令 | 说明 |
| --- | --- |
| `events [--from][--to][--limit]` | 卡顿事件列表（最新 N 条，时间升序；字段含 `causes` / `cause_kinds` / `primary_cause` / `severity` / `duration_ms` / `culprits` / `onset_ts`；`--limit` 默认 100） |
| `samples [--from][--to][--limit]` | 样本区间查询（最新 N 条，时间升序；样本量大（1Hz、保留 30 天），`--limit` 默认 1000，可调大） |
| `analysis [--from][--to]` | 聚合分析一次输出全部：`kpi` / `trend` / `culprits`（元凶榜）/ `cause_types` / `root_cause`（最近事件根因报告） |
| `config` | 当前生效配置（config.toml 加载结果，含默认值回退后的有效值） |
| `status` | 服务状态：SCM 状态 + 心跳健康（Running/Stale/Stopped）+ db 路径 |
| `process [--limit]` | 现场采集一次 top 进程快照（不写库；`--limit` 默认 10） |
| `export --from <开始> --to <结束> [-o <文件>]` | 将时间范围采样数据导出为 CSV（中文表头；时间格式 `YYYY-MM-DD` 或 `YYYY-MM-DD HH:MM:SS`） |
| `upgrade [--no-build]` | 升级：停服（提权）→ `rtk cargo build --release` → 重装启动（提权）；`--no-build` 跳过构建。详见 [UPGRADE.md](./UPGRADE.md) |

示例：

```bash
find-stutter.exe events --limit 5                 # 最近 5 次卡顿
find-stutter.exe events --from 2026-08-16         # 今日卡顿（零点起）
find-stutter.exe samples --from 2026-08-16 --limit 100
find-stutter.exe analysis | jq .kpi
find-stutter.exe status | jq .heartbeat.health
find-stutter.exe export --from 2026-07-25 --to 2026-07-26 --output today.csv
```

### `find-stutter-service.exe`（service 端）

| 命令 | 说明 |
| --- | --- |
| `run` | 前台运行服务循环（开发/调试，不注册 SCM） |
| `install` | 注册为 Windows 服务（需管理员权限） |
| `uninstall` | 卸载 Windows 服务 |
| `start` | 启动已注册的服务（需管理员权限） |
| `stop` | 停止已运行的服务（需管理员权限） |
| `status` | 打印服务当前状态；退出码 `0`=Running / `1`=Stopped·Pending / `2`=NotFound / `3`=Error（GUI 端按此协议走自动安装/启动逻辑） |
| `install-start` | 一次完成 `install` + `start`（GUI 端 UAC 提权路径用，需管理员权限） |
| `--config <path>` | 全局：指定配置文件路径（默认 `config.toml`） |

## 配置

配置文件为 `config.toml`（与 exe 同级），各字段均有中文注释，详见文件内说明。
常用项：采样间隔、卡顿阈值、各指标显隐、皮肤、数据库路径与保留天数、提醒级别。
增强功能相关开关：`[ui] taskbar`（任务栏嵌入）、`[ui] auto_start_service`（GUI 自动启动服务）、
`[notifications]`（卡顿通知弹窗开关与最低等级）。

## 开发指南

### 目录结构

```
crates/
  core/      采集器、检测引擎、SQLite 日志、类型定义、analytics（分析聚合与
             根因纯函数，ADR-0001 下沉：UI 与 CLI 共用同一分析口径）
  cli/       find-stutter-cli：agent 查询界面（events / samples / analysis /
             config / status / process 六件套 JSON 子命令 + export CSV +
             upgrade 升级编排；clap 定义也在这里，bin 只做分发）
  service/   find-stutter-service: Windows 服务（run / install / uninstall / start / stop / status / install-start）
  ui/        悬浮窗 UI、皮肤、DbReader（1Hz SQLite 轮询）、服务健康角标、
             auto_start（GUI 启动时自动检测 + UAC 提权安装/启动）、
             elevate（ShellExecuteExW + runas）、hotreload（notify 配置/皮肤监听）、
             tray（系统托盘）、notify（卡顿气泡通知）、taskbar（伪任务栏窗口）、
             process_list（进程详情页：采样/聚合/排序/搜索/详情面板）
  bin/       find-stutter 入口（只做分发：无参数 = GUI；子命令 = 转发 cli）
config.toml  配置（含 [ui] taskbar、[notifications] 开关）
stutter.db   运行时生成的数据库
```

### 测试

```bash
rtk cargo test --workspace
```

各 crate 均包含单元测试：`core`（检测、日志与分析聚合）、`cli`（时间解析 / JSON
查询 / 升级编排 / 参数解析）、`ui`（进程列表聚合/排序/搜索、皮肤、热加载、自动启动等）、
`service`（服务生命周期）、`bin`（分发行为）。运行后查看各 crate 实时测试计数。

## 已知限制

- 任务栏嵌入为「伪任务栏窗口」方案（可拖到任务栏位置）；DeskBand 原生注入（Win7/10/11 兼容成本高）未实现。
- 点击穿透模式开启后窗口鼠标事件完全穿透，右键菜单随之失效，且当前没有 `T` 键 / 热键 / 托盘菜单入口可退出（窗口也不在任务栏与 Alt-Tab 列表），开启前请确认确需穿透；如误开启需退出并重启应用恢复。
- 服务模式下需要管理员权限 `install`（仅一次），之后运行无需管理员。
- 通知弹窗用 `Shell_NotifyIconW` 气泡（非 WinRT toast）：无需 AUMID/installer，零配置可用；WinRT toast 需注册 shortcut，暂未采用。
