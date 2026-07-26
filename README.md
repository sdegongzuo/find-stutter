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

# 2. 注册为 Windows 服务（需管理员权限）
target/release/find-stutter-service.exe install

# 3. 启动服务（也可通过服务管理器 / sc start FindStutter）
target/release/find-stutter-service.exe start

# 4. 启动 GUI（只读库）
target/release/find-stutter.exe

# 5. 查询服务状态
target/release/find-stutter-service.exe status
# 输出：Running (运行中) (FindStutter)
# 退出码 0 = 在跑，非 0 = 未运行/未注册

# 6. 停止 / 卸载
target/release/find-stutter-service.exe stop
target/release/find-stutter-service.exe uninstall
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
| `start` | 启动已注册的服务 |
| `stop` | 停止已运行的服务 |
| `status` | 打印服务当前状态（退出码 0 = 运行中） |
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
| 右键窗口 | 弹出菜单：暂停监控、展开详情、点击穿透、退出 |
| 按 `T` | 切换点击穿透模式（穿透时鼠标无效，用于「只看不挡」） |
| 顶部状态条 | 显示「● 服务运行中（绿）/ 卡顿（黄）/ 已停止（红）」，服务断开时变红并禁用暂停按钮 |

## 目录结构

```
crates/
  core/      采集器、检测引擎、SQLite 日志、类型定义（含 P3 心跳表 + 只读接口）
  service/   find-stutter-service: Windows 服务（run / install / uninstall / start / stop / status）
  ui/        悬浮窗 UI、皮肤、DbReader（1Hz SQLite 轮询）、服务健康状态条
  bin/       CLI 入口（默认 UI / export / stats）
config.toml  配置
stutter.db   运行时生成的数据库
```

## 测试

```bash
cargo test --workspace
# core:  52 unit + 13 integration
# ui:    20 unit + 10 integration  (含 7 个 reader 健康检测 + 4 个 service_status 格式化)
# service: 16 unit  (CLI 解析 + 心跳 roundtrip + 状态文本 + 状态机)
# 合计 111 个测试
```

## 已知限制 / 后续计划

- 系统托盘图标、原生 toast 通知、配置热加载、任务栏嵌入尚未实现。
- 点击穿透模式下窗口无法接收鼠标，退出穿透请用 `T` 键或先聚焦于窗口。
- 服务模式下需要管理员权限 `install`（仅一次），之后运行无需管理员。
