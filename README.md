# find-stutter

Windows 桌面悬浮窗，实时监控系统卡顿（CPU / 内存 / 磁盘 / 网络 / GPU / 温度），
并在检测到卡顿时记录到本地 SQLite 数据库，便于事后回溯「刚才为什么卡了」。

## 功能特性

- **透明置顶悬浮窗**：始终浮在最上层，不抢焦点，CJK 中文字体正常显示。
- **实时指标**：上传/下载速率、CPU 使用率、内存占用、GPU 利用率、磁盘读写速率、CPU 温度。
- **卡顿检测引擎**：基于阈值 + 突变检测，区分 `minor` / `major` / `critical` 三级严重程度。
- **悬浮窗闪烁提醒**：检测到 `major` / `critical` 卡顿时，窗口边框脉冲闪烁（红/橙）。
- **详情面板**：单击窗口展开，显示今日流量、CPU 频率、温度、卡顿次数、进程/线程数。
- **原生拖拽**：在窗口上按住拖动，由系统负责重绘，无重影/闪烁。
- **右键菜单**：暂停/恢复监控、展开详情、点击穿透、退出。
- **点击穿透模式**：窗口鼠标事件穿透（看得到点不到），按 `T` 退出穿透。
- **SQLite 持久化**：采样与卡顿事件写入 `stutter.db`（WAL 模式，读写并发无锁）。
- **P3 服务化架构**（已完成）：独立 Windows 服务做采集写库，GUI 只读 SQLite 轮询；服务停止时 UI 顶部状态条变红「● 服务已停止」并禁用暂停按钮。
- **UAC 自动提权**（已完成）：首次双击 GUI 时若服务未注册 / 未运行，会自动弹 UAC 申请管理员权限完成 `install + start`，用户只需点「是」即可。
- **系统托盘图标**（P1）：后台线程 + win32 消息循环，右键菜单「显示/隐藏悬浮窗 / 暂停/恢复 / 退出」，左键单击 = 显示/隐藏；失败不阻塞 GUI。
- **配置 / 皮肤热加载**（P2）：`notify` 监听 `config.toml` 与 `skins/` 目录，保存即生效（皮肤名变更会重载皮肤，皮肤颜色/字号实时更新）。
- **卡顿通知弹窗**（P2）：检测到新的 Major/Critical 卡顿时弹 Windows 原生系统通知（气泡 toast），`[notifications]` 配置开关与最低等级。
- **任务栏嵌入**（P2）：`config.toml [ui] taskbar = true` 启用横向窄条伪任务栏窗口（默认底部中央，可拖到任务栏位置）。
- **CLI 导出**：将卡顿记录导出为 CSV，或查询当日卡顿次数。

## 架构（P3 重构后）

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
                │  暂停按钮 / 闪烁 / 详情面板 │   服务停止时变红禁用
                └──────────────────────────┘
```

**WAL 模式**保证服务持续写、UI 持续读互不阻塞；UI 不再自采，崩溃也不影响后台服务。

## 构建

需要 Rust 工具链（Windows + MSVC target）。

```bash
# 调试构建
cargo build

# 发布构建（推荐）
cargo build --release
```

产物：
- `target/release/find-stutter.exe`（UI 端）
- `target/release/find-stutter-service.exe`（Windows 服务）

## 运行

### 方式 1：纯 GUI（不安装服务）

> 纯 GUI 模式下 `find-stutter.exe` 已切换为「1Hz 读 stutter.db」只读模式。
> 若 stutter.db 不存在会显示「● 服务未注册（请运行 find-stutter-service install）」。
> 若希望 GUI 也能采集，需先安装服务（见方式 2）。

```bash
cargo run --release
# 或
target/release/find-stutter.exe
```

### 方式 2：P3 服务化（推荐生产环境）

```bash
# 1. 构建
cargo build --release

# 2. 注册为 Windows 服务（需管理员权限；只第一次需要）
target/release/find-stutter-service.exe install

# 3. 启动 GUI（普通用户即可；后台服务会自动检测 + 启动）
target/release/find-stutter.exe
```

GUI 启动时**自动**做的事（在 `crates/ui/src/auto_start.rs`）：

1. 在 GUI exe 同目录 / CWD / PATH 里找 `find-stutter-service.exe`
2. 调 `status` 子命令查 SCM（退出码 0 = 在跑）
3. 没在跑就调 `start` 子命令尝试启动
4. 失败也不阻塞 GUI 启动，只在日志 + 顶部状态条提示

> **自动测试 / CI 环境**：GUI 每次启动都会尝试检测服务，服务未注册/停止时
> 会弹 UAC 授权。不想被弹窗打断时，二选一即可完全跳过自动启动：
>
> ```bash
> # 方式 1：环境变量（推荐，不改配置）
> set FIND_STUTTER_SKIP_SERVICE=1 && find-stutter.exe
>
> # 方式 2：配置文件
> # config.toml → [ui] auto_start_service = false
> ```

| 自动启动结果 | GUI 表现 |
| --- | --- |
| `AlreadyRunning` | 1 秒后状态条变绿「● 服务运行中」 |
| `Started` | 1 秒后状态条变绿「● 服务运行中」 |
| `Skipped` | 服务自动启动已关闭（环境变量/配置），状态条显示服务状态 |
| `NotRegistered` | 状态条变红「● 服务未注册（请运行 find-stutter-service install）」 |
| `ExeNotFound` | 状态条变红「● 服务未注册（请运行 find-stutter-service install）」（先构建 service crate） |
| `StartFailed` | 状态条变红「● 服务已停止」，日志提示手动 `start` |

如果自动 `start` 失败（最常见：当前用户无 admin 权限），**手动启动**：

```bash
# 用管理员身份打开 cmd，cd 到 target/release
find-stutter-service.exe start

# 或
sc start FindStutter

# 查询
find-stutter-service.exe status
# 输出：Running (运行中) (FindStutter)
# 退出码 0 = 在跑，非 0 = 未运行/未注册

# 停止 / 卸载
find-stutter-service.exe stop
find-stutter-service.exe uninstall
```

**SCM 服务名**：`FindStutter`，显示名 `Find Stutter Monitor`。

### 方式 3：服务前台调试（不注册 SCM）

```bash
# 等同于 P3 之前的行为：在前台跑服务循环
target/release/find-stutter-service.exe run
# Ctrl-C 优雅退出
```

### 命令行子命令

#### `find-stutter.exe`（UI 端）

| 命令 | 说明 |
| --- | --- |
| （无参数） | 启动悬浮窗监控（只读 stutter.db） |

#### `find-stutter-service.exe`（服务端）

| 命令 | 说明 |
| --- | --- |
| `run` | 前台运行服务循环（开发/调试，不注册 SCM） |
| `install` | 注册为 Windows 服务（需管理员权限） |
| `uninstall` | 卸载 Windows 服务 |
| `start` | 启动已注册的服务（需管理员权限） |
| `stop` | 停止已运行的服务（需管理员权限） |
| `status` | 打印服务当前状态（退出码 0=Running / 1=Stopped·Pending / 2=NotFound / 3=Error，GUI 端按此协议走自动安装/启动逻辑） |
| `install-start` | 一次完成 `install` + `start`（GUI 端 UAC 提权路径用，需管理员权限） |
| `--config <path>` | 全局：指定配置文件路径（默认 `config.toml`） |

#### `find-stutter.exe export / stats`（CLI 工具）

| 命令 | 说明 |
| --- | --- |
| `export --from <开始> --to <结束> --output <文件>` | 将指定时间范围的卡顿记录导出为 CSV（时间格式 `YYYY-MM-DD` 或 `YYYY-MM-DD HH:MM:SS`） |
| `stats` | 打印今日卡顿次数 |

示例：

```bash
find-stutter.exe export --from 2026-07-25 --to 2026-07-26 --output today.csv
find-stutter.exe stats
```

## 配置

配置文件为 `config.toml`（与 exe 同级），各字段均有中文注释，详见文件内说明。
常用项：采样间隔、卡顿阈值、各指标显隐、皮肤、数据库路径与保留天数、提醒级别。

## 交互说明

| 操作 | 效果 |
| --- | --- |
| 拖动窗口 | 移动悬浮窗（原生拖拽，无重影） |
| 单击窗口 | 展开 / 收起详情面板 |
| **右键窗口** | **弹出菜单列表：暂停/恢复监控、点击穿透、退出** |
| 右键托盘图标 | 托盘菜单：显示/隐藏悬浮窗、暂停/恢复、退出 |
| 左键单击托盘图标 | 显示 / 隐藏悬浮窗 |
| 按 `T` | 切换点击穿透模式（穿透时鼠标无效，用于「只看不挡」） |
| 顶部状态条 | 显示「● 服务运行中（绿）/ 卡顿（黄）/ 已停止（红）」，服务断开时变红并禁用暂停按钮 |
| 修改 config.toml / skin.toml | 保存后热加载（皮肤颜色/字号实时生效；db 路径等需重启生效） |

## 目录结构

```
crates/
  core/      采集器、检测引擎、SQLite 日志、类型定义（含 P3 心跳表 + 只读接口）
  service/   find-stutter-service: Windows 服务（run / install / uninstall / start / stop / status / install-start）
  ui/        悬浮窗 UI、皮肤、DbReader（1Hz SQLite 轮询）、服务健康状态条、
             auto_start（GUI 启动时自动检测 + UAC 提权安装/启动）、
             elevate（ShellExecuteExW + runas）、hotreload（notify 配置/皮肤监听）、
             tray（系统托盘）、notify（卡顿气泡通知）、taskbar（伪任务栏窗口）
  bin/       CLI 入口（默认 UI / export / stats）
config.toml  配置（含 [ui] taskbar、[notifications] 开关）
stutter.db   运行时生成的数据库
```

## 测试

```bash
cargo test --workspace
# core:  59 unit + 13 integration
# ui:    83 unit + 10 integration
# service: 17 unit
# bin:    5 cli tests
# 合计 184 个测试（0 失败）
```

## 已知限制 / 后续计划

- 任务栏嵌入为「伪任务栏窗口」方案（可拖到任务栏位置）；DeskBand 原生注入（Win7/10/11 兼容成本高）未实现。
- 点击穿透模式下窗口无法接收鼠标，退出穿透请用 `T` 键或先聚焦于窗口。
- 服务模式下需要管理员权限 `install`（仅一次），之后运行无需管理员。
- 通知弹窗用 `Shell_NotifyIconW` 气泡（非 WinRT toast）：无需 AUMID/installer，零配置可用；WinRT toast 需注册 shortcut，暂未采用。
