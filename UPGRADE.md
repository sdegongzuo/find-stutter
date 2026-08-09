# 升级文档（find-stutter-service 部署 / 升级指南）

> 适用场景：改动 `find-stutter-service`（后台采集服务，承载检测逻辑）后，
> 把新构建部署到正在运行的 Windows 服务上并让其生效。
>
> 本次升级目标：**C 档——卡顿事件记录「造成卡顿的进程信息」（culprit）**。
> 设计细节见 [`TODO.md`](./TODO.md) 的 **P5 — 检测精度优化 → C** 章节。

---

## 1. 为什么需要专门的升级流程

本项目采用**服务采集 + GUI 只读**架构（见 `README.md` / `TODO.md` 架构决策）：

- 检测逻辑（`Detector`）只跑在 `find-stutter-service.exe`（Windows 服务进程）里，
  GUI（`find-stutter.exe`）只从 `stutter.db` 读取，不做采集、不跑检测。
- 因此：**只要改了 `collector.rs` / `detector.rs` / `types.rs` / `logger.rs` 等
  core 检测相关代码，新逻辑就只在服务进程里生效**，必须重启服务才能用上新代码。

而重启服务有两个绕不开的障碍，正是「升级文档」存在的理由：

1. **文件锁**：服务进程运行时会独占 `find-stutter-service.exe`。直接
   `cargo build` 会报 `os error 5（拒绝访问）`，写不进新 exe。
   → 必须先**停服**释放文件锁，再构建。
2. **权限**：`sc stop` / 服务安装 / `install-start` 都要求**管理员权限**。
   普通终端会报 `Access is denied`。
   → 必须用 UAC 提权（管理员）执行。

结论：一次完整的升级 = **停服（释放锁）→ 重建 → 重启服务**，且全程需管理员。

---

## 2. 本次升级内容（C 档）

在卡顿事件里附上「谁是元凶」，便于事后定位。改动清单（落地于 `find_stutter_core`）：

| 模块 | 改动 |
|------|------|
| `types.rs` | 新增 `ProcessCulprit { pid, name, dimension, value }`；`StutterEvent.culprits: Vec<ProcessCulprit>` |
| `collector.rs` | 每 tick 采集 top 进程快照（首版覆盖 **cpu / mem** 维度），供 culprit 提取使用 |
| `detector.rs` | 判定卡顿时，按触发维度从快照挑出对应 top 进程作为 culprit |
| `logger.rs` | `stutter_events` 写入新增 `culprits` 列（JSON 文本） |
| 数据库 | `stutter_events` 表 `ALTER TABLE` 增加 `culprits` 列（旧库自动迁移） |
| `reader.rs` | GUI 读取 `culprits`（详情页展示，后续 UI 改造） |

> 磁盘 / 网络 per-process IO 较复杂，列为进阶，首版先覆盖 cpu / mem。
> 详见 `TODO.md` P5 → C。

---

## 3. 一键升级（提权脚本，推荐）

脚本：`upgrade-service.ps1`（随本次升级一同提供，见第 5 节实现）。

**用法**：

```powershell
# 方式一：右键「使用 PowerShell 运行」（脚本会自动弹 UAC 提权）
# 方式二：在普通终端直接调用，脚本检测到非管理员会自动重新以管理员启动
.\upgrade-service.ps1

# 跳过重新构建（仅用已有 release exe 重装服务，比如只改了配置或调试）
.\upgrade-service.ps1 -NoBuild

# 指定 RTK 路径（默认 D:/app/cargo/bin/rtk）
.\upgrade-service.ps1 -Rtk "D:/app/cargo/bin/rtk"
```

**脚本自动完成的步骤**：

1. **自提权**：若当前不是管理员，自动 `Start-Process -Verb RunAs` 重启自身并退出，
   弹 UAC 让用户确认（不需要手动「以管理员身份运行」）。
2. **停服**：`sc stop FindStutter`，并轮询 `sc query` 直到状态变为 `STOPPED`
   （最多等待约 15 秒），释放 `find-stutter-service.exe` 文件锁。
3. **重建**（除非 `-NoBuild`）：`rtk cargo build --release`，构建全部 workspace
   （含 service + GUI）。
4. **重装并启动**：`find-stutter-service.exe install-start`。
   `install()` 会自动比对 binary 路径——发现 exe 已更新则 `change_config` + `stop + start`，
   首次则注册并启动服务。
5. **校验**：打印 `sc query FindStutter` 状态，确认 `RUNNING`。

**成功标志**：脚本结尾显示 `STATE: 4 RUNNING`，且 GUI 运行后卡顿事件带 culprit 信息。

---

## 4. 手动升级（不用脚本）

若想逐步确认每一步，可手动执行（**全部需在管理员 PowerShell / CMD 中**）：

```powershell
# 1) 停服并等待
sc stop FindStutter
# 轮询直到 STOPPED
sc query FindStutter

# 2) 重建（release）
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

## 5. 回滚

代码回退后，用同一套流程重新部署即可，**数据不受影响**：

```powershell
git checkout <上一版 commit>        # 回退代码
.\upgrade-service.ps1 -NoBuild      # 或重新 build（不加 -NoBuild）
```

- 卡顿数据存在 `stutter.db`（WAL 模式），升级 / 回滚都不动数据库文件，
  历史 `stutter_events` 与 `samples` 完整保留。
- 新版 `stutter_events` 增加了 `culprits` 列；回滚到旧版后该列只是多余字段，
  旧代码不读它，不影响运行（SQLite 容忍未知列）。

---

## 6. 验证清单

升级完成后逐项确认：

- [ ] `sc query FindStutter` → `STATE: 4 RUNNING`
- [ ] 服务进程 PID 已变化（说明用的是新 exe，而非旧进程继续跑）
- [ ] `stutter.db` 的 `stutter_events` 表存在 `culprits` 列：
      ```sql
      PRAGMA table_info(stutter_events);
      ```
- [ ] 制造 / 等待一次卡顿后，事件 JSON 的 `culprits` 非空，含
      `pid` / `name` / `dimension` / `value`
- [ ] GUI 启动后以 `FIND_STUTTER_SKIP_SERVICE=1` 跳过自动启服、只读库，
      确认能读到服务的 culprit 数据（右上角显示「● 服务运行中」）

---

## 7. 注意事项 / 常见坑

- **不要在普通终端直接 `cargo build`**：服务在跑时必报 `os error 5`。
  永远先停服（用脚本或 `sc stop`）。
- **不要 `taskkill /F` 杀服务进程**：普通权限会 `Access is denied`；
  用 `sc stop`（脚本已封装）。
- **升级期间服务会中断几秒到构建耗时**：属正常，GUI 在此期间显示「服务已停止 / Stale」，
  重启后自动恢复。
- **UAC 提权通道**：若你的环境无法弹 UAC（如远程 / 受限会话），请直接在
  **管理员 PowerShell** 里运行 `upgrade-service.ps1`（跳过自提权那一步）。
- **架构不变**：检测逻辑始终只在 service，本升级不引入 GUI 双采集，
  保持「服务采集 + GUI 只读」设计。

---

## 附：升级相关文件索引

| 文件 | 说明 |
|------|------|
| `upgrade-service.ps1` | 一键提权升级脚本（本文第 3 节） |
| `crates/service/src/install.rs` | SCM 注册 / 升级 / 启停实现（`install()` 含 binary 变化检测） |
| `crates/service/src/cli.rs` | `install-start` 等子命令定义 |
| `TODO.md` → P5 C | C 档（进程 culprit）详细设计 |
| `README.md` | 项目说明与服务化架构 |
