# 升级文档（find-stutter 部署 / 升级指南）

> 适用场景：改动检测 / 采集逻辑（`crates/core`、`crates/service`）后，
> 把新构建部署到正在运行的 Windows 服务上并让其生效。
>
> 一条命令：`find-stutter upgrade [--no-build]`（详见第 2 节）。

---

## 1. 为什么需要专门的升级流程

本项目采用**服务采集 + GUI 只读**架构（见 `README.md` 架构决策）：

- 检测逻辑（`Detector`）只跑在 `find-stutter-service.exe`（Windows 服务进程）里，
  GUI（`find-stutter.exe`）只从 `stutter.db` 读取，不做采集、不跑检测。
- 因此：**只要改了 `collector.rs` / `detector.rs` / `types.rs` / `logger.rs` 等
  core 检测相关代码，新逻辑就只在服务进程里生效**，必须重启服务才能用上新代码。

而重启服务有两个绕不开的障碍，正是「升级流程」存在的理由：

1. **文件锁**：服务进程运行时会独占 `find-stutter-service.exe`。直接
   `cargo build --release` 会报 `os error 5（拒绝访问）`，写不进新 exe。
   → 必须先**停服**释放文件锁，再构建。
2. **权限**：`sc stop` / 服务安装 / `install-start` 都要求**管理员权限**。
   普通终端会报 `Access is denied`。
   → 必须用 UAC 提权（管理员）执行。

结论：一次完整的升级 = **停服（释放锁）→ 重建 → 重启服务**，且全程需管理员。

---

## 2. 一键升级（CLI 子命令，推荐）

```bash
# 默认：停服（提权）→ rtk cargo build --release → 重装启动（提权）
target/release/find-stutter.exe upgrade

# 跳过重新构建（仅用已有 release exe 重装服务，比如只改了配置或手动构建过）
target/release/find-stutter.exe upgrade --no-build
```

**自动完成的步骤**（Rust 实现，位于 `crates/cli/src/upgrade.rs`，进入版本控制）：

1. **停服**：UAC 提权运行 `find-stutter-service.exe stop`，释放
   `find-stutter-service.exe` 文件锁（UAC 弹窗由系统弹出，点「是」即可）。
2. **重建**（除非 `--no-build`）：`rtk cargo build --release`（在自动发现的仓库根
   执行；**必须经 rtk**——先在 PATH 找 `rtk`，找不到回退 `D:\app\cargo\bin\rtk.exe`）。
3. **重装并启动**：UAC 提权运行 `find-stutter-service.exe install-start`。
   `install()` 会自动比对 binary 路径——发现 exe 已更新则 `change_config` +
   `stop + start`，首次则注册并启动服务。
4. **校验**：运行 `find-stutter-service.exe status`（普通权限即可），
   退出码 0 = Running。

**成功标志**：结尾输出 `升级完成：服务已用新构建运行`。

> 这是 ADR-0001 决策 6（升级收进同一条命令入口）：**替代已废弃的本地
> `upgrade-service.ps1` 脚本**（ps1 被 `.gitignore` 的 `*.ps1` 规则排除在仓库外，
> 且三次踩坑：MSYS 路径、`sc` 别名、引号转义）。启动与升级共用 `find-stutter`
> 一个入口。它也是决策 4「CLI 不做提权控制」的唯一例外。

---

## 3. 手动升级（不用子命令）

若想逐步确认每一步，可手动执行（**全部需在管理员 PowerShell / CMD 中**）：

```powershell
# 1) 停服并等待
sc stop FindStutter
# 轮询直到 STOPPED
sc query FindStutter

# 2) 重建（release；必须经 rtk）
D:/app/cargo/bin/rtk cargo build --release

# 3) 重装并启动（install 会自动升级已注册的服务）
.\target\release\find-stutter-service.exe install-start

# 4) 校验
sc query FindStutter
```

> 也可用分步命令代替 `install-start`：
> `.\target\release\find-stutter-service.exe uninstall` → 再 `install` → 再 `start`，
> 但 `install-start` 已内置「binary 变化才重启」的升级逻辑，一步到位。

---

## 4. 回滚

代码回退后，用同一套流程重新部署即可，**数据不受影响**：

```powershell
git checkout <上一版 commit>        # 回退代码
find-stutter.exe upgrade            # 或 --no-build（用已有构建）
```

- 卡顿数据存在 `stutter.db`（WAL 模式），升级 / 回滚都不动数据库文件，
  历史 `stutter_events` 与 `samples` 完整保留。
- SQLite 容忍未知列：新版本落库的新列在回滚后只是多余字段，旧代码不读它，
  不影响运行。

---

## 5. 验证清单

升级完成后逐项确认：

- [ ] `sc query FindStutter` → `STATE: 4 RUNNING`（或 `find-stutter.exe status | jq .scm.state`）
- [ ] 服务进程 PID 已变化（说明用的是新 exe，而非旧进程继续跑）
- [ ] `find-stutter.exe status` 心跳健康为 `running`（新二进制在正常写心跳）
- [ ] `find-stutter.exe events --limit 1` 能读到升级后的最新事件

---

## 6. 注意事项 / 常见坑

- **不要在普通终端直接 `cargo build`**：服务在跑时必报 `os error 5`。
  永远先停服（`upgrade` 子命令或 `sc stop`）。
- **不要 `taskkill /F` 杀服务进程**：普通权限会 `Access is denied`；
  用 `sc stop`（upgrade 子命令已封装提权停服）。
- **升级期间服务会中断几秒到构建耗时**：属正常，GUI 在此期间显示「服务已停止 / Stale」，
  重启后自动恢复。
- **构建必须走 rtk**：`upgrade` 子命令内部固定经 rtk 调 cargo（项目工具链约定，
  见 `AGENTS.md`）；找不到 rtk（PATH 与 `D:\app\cargo\bin\rtk.exe` 均无）时报错退出。
- **架构不变**：检测逻辑始终只在 service，本流程不引入 GUI 双采集，
  保持「服务采集 + GUI 只读」设计。

---

## 附：升级相关文件索引

| 文件 | 说明 |
|------|------|
| `crates/cli/src/upgrade.rs` | `find-stutter upgrade` 的步骤编排（停服 → rtk 构建 → install-start → 校验） |
| `crates/cli/src/elevate.rs` | UAC 提权 spawn（ShellExecuteExW + runas，upgrade 专用精简版） |
| `crates/service/src/install.rs` | SCM 注册 / 升级 / 启停实现（`install()` 含 binary 变化检测） |
| `crates/service/src/cli.rs` | `install-start` 等子命令定义 |
| `README.md` | 项目说明与服务化架构 |
