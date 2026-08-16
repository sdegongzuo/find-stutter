# find-stutter 构建与运行指南

## 环境要求

- **OS**: Windows 10/11
- **Rust**: 1.75+ (stable，本机 1.97.1)
- **rtk**: 0.42+（cargo 工具链包装，本机位于 `/d/app/cargo/bin/rtk`；项目约定所有
  cargo 命令用 `rtk cargo ...` 包裹，裸 cargo 亦可）

## 快速开始

### 1. 克隆项目

```bash
git clone https://github.com/sdegongzuo/find-stutter.git
cd find-stutter
```

### 2. 编译

```bash
# 使用 rtk（项目约定）
rtk cargo build --release

# 或直接用 cargo
cargo build --release
```

产物（均在 `target/release/`）：

| 文件 | 说明 |
| --- | --- |
| `find-stutter.exe` | **唯一入口**（无参数 = GUI 悬浮窗；子命令 = agent CLI 查询 / CSV 导出 / 升级。由 `crates/bin` 编译，链接 `ui` + `cli` 库） |
| `find-stutter-service.exe` | Windows 服务（`run` / `install` / `uninstall` / `start` / `stop` / `status` / `install-start`） |

> **重要**：改完 `crates/ui` 的代码后，必须重新编译 GUI 入口
> `rtk cargo build --release -p find-stutter`。只编 `-p find-stutter-ui` 只会编译
> ui 库本身（ADR-0001 后 ui 不再产出独立的 `find-stutter-ui.exe`），
> **不会更新用户实际运行的 `find-stutter.exe`**。
>
> 若编译报「拒绝访问 os error 5」，是 `find-stutter.exe` 正被运行中的 GUI 占用：
> 先 `Stop-Process -Name "find-stutter" -Force` 再编译。

### 3. 运行

```bash
# 一条命令启动（带悬浮窗；自动确保后台服务在跑，首次会弹 UAC）
target/release/find-stutter.exe
```

GUI 启动时自动检测并启动后台服务（`crates/ui/src/auto_start.rs`）；不想弹 UAC 时可设
`FIND_STUTTER_SKIP_SERVICE=1` 或 `config.toml [ui] auto_start_service = false`。

### 4. 查看数据（agent CLI，单行 JSON 便于 jq）

```bash
target/release/find-stutter.exe events --limit 5      # 最近 5 次卡顿
target/release/find-stutter.exe status                # 服务状态 + 心跳健康 + db 路径

# 导出 CSV（中文表头）
target/release/find-stutter.exe export --from 2026-07-25 --to 2026-07-26 -o report.csv
```

## 项目结构

```
find-stutter/
├── Cargo.toml                    # Workspace 根配置（core / ui / cli / bin / service 五成员）
├── config.toml                   # 运行时配置（热加载）
├── README.md                     # 说明
├── stutter.db                    # 运行时生成的 SQLite 数据库（WAL 模式）
├── crates/
│   ├── core/                     # 核心库：collector（sysinfo+PDH+WMI 采集）、
│   │                             #   detector（阈值+突变检测+滞回）、logger（SQLite+CSV）、
│   │                             #   types、analytics（分析聚合+根因纯函数，UI/CLI 共用）
│   ├── cli/                      # find-stutter-cli：agent 查询界面（events/samples/
│   │                             #   analysis/config/status/process JSON + export CSV +
│   │                             #   upgrade 升级编排；含 clap 子命令定义）
│   ├── service/                  # find-stutter-service：Windows 服务 + SCM 管理 CLI
│   ├── ui/                       # find-stutter-ui 库：overlay.slint（悬浮窗 + 进程列表 UI）、
│   │                             #   lib.rs（事件接线）、overlay.rs、skin.rs、reader.rs（1Hz 轮询）、
│   │                             #   auto_start / elevate / hotreload / tray / notify / taskbar /
│   │                             #   window / process_list（进程详情页）
│   └── bin/                      # find-stutter：唯一入口（无参数 → GUI；子命令 → 转发 cli）
└── skins/default/skin.toml       # 默认皮肤配置
```

## 运行测试

```bash
# 运行全部测试
rtk cargo test --workspace

# 各 crate（core / cli / ui / service / bin）
rtk cargo test -p find-stutter-core
rtk cargo test -p find-stutter-cli
rtk cargo test -p find-stutter-ui
rtk cargo test -p find-stutter-service
rtk cargo test -p find-stutter

# 运行特定测试
rtk cargo test -p find-stutter-core detector_cpu_threshold
```

## 配置说明

编辑 `config.toml`（保存即热加载，部分字段需重启生效）：

```toml
[sampling]
interval_ms = 1000              # 采样间隔（毫秒）

[detection]
cpu_threshold = 90.0            # CPU 告警阈值（%）；滞回 cpu_hysteresis 防抖
mem_threshold_mb = 500          # 可用内存告警阈值（MB）
swap_threshold = 50.0           # Swap 告警阈值（%）；滞回 swap_hysteresis 防抖
spike_ratio = 3.0               # 综合指标突增倍数（网络/磁盘 spike 判据）
spike_min_bps = 2000000         # spike 绝对下限（B/s），防空闲零头误报
sustained_seconds = 3           # 持续秒数阈值

[ui]
skin = "default"                # 皮肤名称
taskbar = false                 # 伪任务栏窗口开关
process_highlight_pct = 30.0    # 进程详情页 CPU/内存高亮阈值（%）
process_refresh_ms = 30000      # 进程详情页自动刷新间隔（毫秒）

[storage]
db_path = "stutter.db"          # 数据库路径
retention_days = 30             # 数据保留天数

[notifications]
stutter_alert = true            # 卡顿气泡通知开关
min_severity = "major"          # 最低提醒等级 minor/major/critical

[logging]
level = "info"                  # error/warn/info/debug/trace
```

## 皮肤自定义

在 `skins/<name>/skin.toml` 创建自定义皮肤（扁平结构，字段带中文注释）：

```toml
width = 260.0                   # 悬浮窗宽度
height = 78.0                   # 悬浮窗高度
background_color = "#1E1E2E"
border_color = "#45475A"
border_radius = 8.0
font_size = 13.0
upload_color = "#A6E3A1"        # 上传速度颜色
download_color = "#89B4FA"      # 下载速度颜色
cpu_color = "#F9E2AF"
memory_color = "#F38BA8"
gpu_color = "#CBA6F7"
disk_color = "#94E2D5"
label_color = "#BAC2DE"
```

## 已知问题 / 注意

1. **服务更新需管理员重启**：服务/守护进程更新代码后必须重启进程才生效（重命名 exe
   只是绕过文件占用，运行中进程仍跑旧映像）。推荐直接 `find-stutter.exe upgrade`
   （停服 → rtk 构建 → 重装启动，自动 UAC 提权，见 `UPGRADE.md`）；
   手动路径 `sc stop FindStutter && sc start FindStutter` 需管理员权限。
2. **进程详情页内存双口径**：「内存」列为提交大小（Commit Size，任务管理器「详细信息」
   页口径），「物理内存」列为工作集（Working Set），两者并存展示。
3. **进程详情页首次打开**：窗口立即出现，数据在 1~2 秒内由快速 tick 补齐（服务端
   sysinfo 首轮初始化较慢）。
