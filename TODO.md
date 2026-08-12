# find-stutter TODO
需逐个实现，做完一个才能做下一个

开发流程：用子代理去实现，然后父代理验收，验收完成之后还有调用code-review技能进行review，然后有问题交给子代理去改，然后再次review，重复这个过程，直到没问题然后提交并push，更新todo文档，然后继续开发下一个

## 进程详情页增强（2026-08-11）

> 对比系统任务管理器后梳理的改进项。

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

### P7 — 卡顿根因分析（深度归因）— 2026-08-11
> 背景：卡顿分析界面-PRD(F1–F8) 已覆盖趋势/归因/资源关联，但根因只到「卡顿时谁在场/什么高」，
> 说不出「谁先动、谁是主因、是否被牵连」。本阶段补「因果链前两段」+ 归因算法 + 根因钻取 UI。
> 详细设计见 `卡顿根因分析-PRD.md`。检测/数据层（F-RC1~F-RC4）需改 service + 重装（§P5 部署约束），
> 分析层（F-RC5~F-RC13）在 GUI 只读侧。M1 与 §P5 检测精度优化强耦合，建议合并排期。
- [x] **F-RC1 结构化 CauseKind** — 枚举对齐 `detector.cause_key()` 现有 key；`StutterEvent` 新增
      `cause_kinds` + `primary_cause` + 各 cause 首触时刻 + `onset_ts` + `id` 落库；`reader` 反序列化；
      旧库空值用 `cause_key()` 可靠回填（非脆弱关键词）。
      > 实现：`types.rs` 新增 `CauseKind` 枚举（11 + `CpuSpike` 共 12 变体，字符串落库便于 GROUP BY；
      > `CpuSpike` 修正 PRD §3.3 自相矛盾——`cause_key()` 实产出 `"CPU spike"` 而清单未列）；
      > `cause_key` 从 `detector.rs` 迁移到 `types.rs`，与 `from_cause` 共用 `PREFIX_TO_KIND`
      > 单一真相（消除 R2 分类不连续）；`StutterEvent` 新增 `id`/`cause_kinds`/`primary_cause`/
      > `cause_first_touch`(相对 onset 偏移)/`onset_ts`。`detector.analyze` 记录每 cause 首触
      > `SystemTime`，事件结束时 `cause_kinds` 按首触时刻升序（触发者在前）、`primary_cause` 取首触
      > 最早者（F-RC5 再按信号强度×持续细化）。`logger` 四个 ALTER 迁移 + `write_event` 写入；
      > `reader` 列存在性探测反序列化、`id` 回读、旧库 `cause_kinds` 空时 `from_cause` 可靠回填不崩溃。
      > **`detect_core` 抽离为 PRD「建议」且验收标准未强制、F-RC12 才真正消费，故延后到 F-RC12
      > 实施**（避免过早抽离干扰 F-RC1 落库纯度）。已补单测覆盖枚举对齐/序列化/落库回读/旧库回填，
      > `rtk cargo test` 271 全过、`release` 零警告，提交 3c93df7 已 push。
- [x] **F-RC2 磁盘真繁忙度 + 系统级信号** — 采集 `% Disk Time` / `Avg Disk sec/Transfer` /
      `% DPC Time` / `% Interrupt Time` / `Context Switches/sec`（协同 §P5-B）；检测器用
      繁忙度/IO 延迟替代 B/s spike，新增 DpcInterrupt/InterruptStorm/ContextSwitchStorm；
      **`paging`(`page_reads_per_sec`) 已覆盖，不重复造词**
      > 实现：`collector.rs` 新增 `SysPdh`（单 query 挂 5 计数器，复用 `DiskPdh` 的 PDH 封装，
      > `Avg Disk sec/Transfer` 秒×1000 转 ms）；`types.rs` 的 `Sample` 加 `disk_busy_percent`/
      > `disk_avg_io_ms`/`dpc_percent`/`interrupt_percent`/`context_switches_per_sec`（均
      > `serde(default)` 兼容旧 snapshot JSON）+ `Default`；`DetectionConfig` 加 6 个阈值字段
      > （`disk_busy_threshold_percent:95`/`disk_io_threshold_ms:50`/`dpc_threshold_percent:10`/
      > `interrupt_threshold_percent:10`/`context_switch_threshold_per_sec:50000`/
      > `sys_signal_hysteresis_ratio:0.5`，均 `serde(default)` 兼容旧配置）；`detector.rs` 用
      > `disk_busy_percent>95 || disk_avg_io_ms>50` **替代**原「Disk write」B/s spike（`spike_check`
      > 删除该分支+`disk_spike_active` 字段），新增 `DiskBusy`(OR 进/AND 退 复合滞回)/`DpcInterrupt`/
      > `InterruptStorm`/`ContextSwitchStorm`（抽 `sys_signal_hysteresis` helper 复用进入>阈值/
      > 退出<阈值×比例 逻辑）；`PREFIX_TO_KIND` 补 `DiskBusy`/`DpcInterrupt`/`InterruptStorm`/
      > `ContextSwitchStorm` 映射（保留 `Disk write`→`DiskSpike` 供旧库回填）。**落库**：`logger.rs`
      > 的 `samples` 表 CREATE+5×ALTER+INSERT 补齐 5 列（`context_switches_per_sec` 为 INTEGER，
      > f32→i64 落库），新增回读单测。另补 4 个 cause 触发器单测（含「高吞吐但磁盘不忙不误报」回归）。
      > `rtk cargo test` 278 全过、`release` 零警告。service 重装生效同 F-RC1（本会话 UAC 被封，部署留手动）。
- [x] **F-RC3 前台窗口冻结检测** — `SendMessageTimeout(WM_NULL, 200ms)` 探前台窗口，`UiFrozen` cause；
      **仅已在其它 cause 帧探一次 / 独立线程 200ms 超时 + 每 2s 限频**，绝不进采集热路径（500ms 会腰斩 1Hz）。
      **实现要点**：`collector.rs` 新增 `probe_foreground_window_frozen`（SMTO_ABORTIFHUNG|SMTO_BLOCK，
      靠 `GetLastError()==ERROR_TIMEOUT` 区分「WM_NULL 正常返回 0」与「真挂起超时」，修正仅凭 LRESULT==0
      判冻结的误报经典坑；无前台窗口按未冻结）；`detector.rs` 仅在「已触发其它 cause」卡顿帧探测
      （不进热路径）+ 2s 限频 + 依赖注入 `ui_probe` 便于单测，`UiFrozen` 绝不单独成 cause；
      `types.rs` 加 `ui_freeze_timeout_ms`（默认 200，供 F-RC12 what-if）+ `PREFIX_TO_KIND` 补
      `UiFrozen` 映射。3 单测覆盖触发/不触发/不进热路径，`rtk cargo test` 282 全过、`release` 零警告；
      code-review 两轴通过（复审修复误报 bug）。service 重装生效同 F-RC1（UAC 被封，部署留手动）。
- [x] **F-RC4 温度→降频根因** — 数据源改为 `cpu_temp` + `cpu_freq_mhz` 掉档判据
      （`gpu_temp` 从未填充，不纳入），新增 `ThermalThrottle` cause（温度高 + 疑似降频）
      > 实现：`types.rs` 加 `thermal_threshold_celsius`(默认 85℃)/`thermal_freq_drop_ratio`
      > (默认 0.85) 配置+serde(default)+`PREFIX_TO_KIND` 补 `("Thermal throttle",
      > ThermalThrottle)` 映射；`detector.rs` 加 `freq_peak`（近期 CPU 频率峰值基线，max 不衰减）
      > + `thermal_active` 状态，`check_hard_thresholds` 用 `hot && dropped && load`
      > （温度>阈值 且 当前频率<峰值×比例 且 cpu_usage>50% 负载下限，常量
      > `THERMAL_LOAD_MIN_USAGE`）三条件直接重算触发 `ThermalThrottle` cause（gpu_temp
      > 不参与）；4 单测覆盖触发/温度低不触发/频率未掉档不触发/无频率读数不触发。
      > code-review 两轴通过（Standards 指出滞回块 `else if` 为激活条件补集等价 `else` +
      > 50 魔数，已简化为三条件重算+命名常量）。`rtk cargo test` 287 全过、release 零警告。
      > service 重装生效同 F-RC1（UAC 被封，部署留手动）。
- [ ] **F-RC5 主因判定 + 加权** — 权重 `duration × 主因信号强度`（**不乘 severity**，severity=并发 cause 数会重复计数），
      替代平权 COUNT；`primary_cause` 直接作主因高亮
- [ ] **F-RC6 因果方向** — 依赖 detector 落库**各 cause 首触时刻 / 事件 onset**（spike 是滑动基线+滞回，非静态阈值）；
      锚定 `onset≈t-3s`；**一次性 bulk 拉 samples 内存切片**算 leading signal，区分
      「触发者」与「放大器」（如 MemLow 先动 → DiskBusy 放大器）
- [ ] **F-RC7 基线偏离** — 非卡顿 `top_processes` 为空，**改从事件侧聚合**（`culprits`/`snapshot.top_processes`
      作为元凶时的典型占用）；只标显著偏离者，过滤常驻高占用噪声
- [ ] **F-RC8 多进程共现** — 按 culprit 进程名集合做共现统计，输出高频「卡顿组合」可下钻
- [ ] **F-RC9 因果链** — 多 cause 按首触时刻排序成有向链（根因→传导→表象），替代平铺列表
- [ ] **F-RC10 单事件根因钻取卡** — **前置：`StutterEvent` 加 `id`**（reader 当前丢 id，钻取无法关联）；
      点事件给出主因(置信度)+前导曲线(±60s)+偏离幅度+因果链
- [ ] **F-RC11 根因置信度** — 按「主因是否明显领先其余 cause（强度/时间差）」给 0–1 置信度；
      **多因并发本身是 major/critical 定义，不再单独压低**；低置信标注「主因不显著，疑多因并发」
- [ ] **F-RC12 阈值敏感性 what-if** — 客户端用 `snapshot` 信号值重算，不改 service（保持只读）；
      阈值语义须与 `detect_core` 纯函数一致
- [ ] **F-RC13 同类事件画像对比** — 按 cause_kinds+culprit+duration 聚类，给「匹配已知画像」结论
      （与 F-RC7 共用事件侧数据源）
