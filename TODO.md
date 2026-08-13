# find-stutter TODO
需逐个实现，做完一个才能做下一个

开发流程：用子代理去实现，然后父代理验收，验收完成之后还有调用code-review技能进行review，然后有问题交给子代理去改，然后再次review，重复这个过程，直到没问题然后提交并push，更新todo文档，然后继续开发下一个

## 进程详情页增强（2026-08-11）

### P6 — 进程详情页交互增强
- [x] **结束进程树** — 右键菜单新增「结束进程树」，连子进程一起终止；复用已有
      `parent_pid`（`process_list.rs:40`）与 `kill_process`（`process_list.rs:509`），
      可用 `taskkill /T /PID <pid>` 或沿父链递归枚举子进程后逐个 kill；权限不足时
      复用现有 UAC 提权路径（`elevate_kill_process`）。
      > 实现：`window.rs` 新增 `RowMenuCmd::KillTree=3` 菜单项；`process_list.rs`
      > 新增 `collect_process_tree`（按深度降序枚举后代，保证子先于父）/ `kill_process_tree`
      > （逐个终止 + 权限错误优先级修正，确保 Permission 不被 NotFound 掩盖）/
      > `prompt_kill_tree_failure`，并提取 `confirm_elevate` 复用。已补单测，174 全过。
- [x] **应用友好名** — 列表/聚合行优先显示友好名（UWP 包显示名或 exe 的
      `GetFileVersionInfo` `FileDescription`），原始 exe 名作为悬浮/备用；对齐任务管理器
      「进程」页默认显示。需改造 `ProcessRow.name` 采集与 `row_to_slint` / `group_display`
      的展示逻辑。
      > 实现：`ProcessRow` 新增 `display_name`；`process_list.rs` 新增
      > `file_description`（`GetFileVersionInfoW`+`VerQueryValueW` 取 `FileDescription`）/
      > `friendly_name_for` / `cached_display_name`（按 pid 缓存于 `ProcessSampler.name_cache`，
      > sample 后裁剪防 pid 复用串味）；`row_display`/`group_display` 优先用 `display_name`，
      > `name_full`/聚合 `full_name` 保留原始 exe 名，`group_key` 仍用 exe 名（展开不变）。
      > UWP 包显示名按 spec「或」未单独实现（FileDescription 路径已满足），后续可用 WinRT
      > PackageManager 扩展；svchost 仍走服务名聚合（合理例外）。已补单测，176 测试全过。

