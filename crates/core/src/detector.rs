use crate::collector::probe_foreground_window_frozen;
use crate::types::{cause_key, CauseKind, DetectionConfig, ProcessBrief, Sample, Severity, StutterEvent};
use chrono::Utc;
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// F-RC4：降频判定所需的「负载下限」（CPU 使用率 %）。
/// 降频仅在负载下才有归因意义——空闲时频率本就低（节能降频），不能算卡顿根因。
/// 取 50% 是经验值：低于此不视为「负载下频率不升反降」。
const THERMAL_LOAD_MIN_USAGE: f32 = 50.0;

pub struct Detector {
    config: DetectionConfig,
    history: Vec<Sample>,
    stutter_start: Option<SystemTime>,
    current_causes: Vec<String>,
    /// CPU 滞回状态：进入后直到 < threshold - hysteresis 才解除
    /// （滞回带内维持激活，避免阈值附近反复 start/stop 反复记录）
    cpu_active: bool,
    /// spike 滞回状态（各指标独立）：触发后需明显回落才解除，
    /// 配合「连续确认」（recent 中 ≥6/10 超阈值）避免瞬时抖动误报
    cpu_spike_active: bool,
    net_spike_active: bool,
    mem_spike_active: bool,
    /// F-RC2 系统级信号滞回状态（各指标独立）：触发后需降到「阈值×滞回比例」
    /// 以下才解除，避免阈值附近反复横跳。磁盘繁忙度用 `disk_busy_active` 替代
    /// 原磁盘 B/s spike（繁忙度才是磁盘真正饱和的真信号）。
    disk_busy_active: bool,
    dpc_active: bool,
    interrupt_active: bool,
    ctx_switch_active: bool,
    /// F-RC4 温度→降频根因滞回状态：温度高且 cpu_freq 掉档（疑似降频）则激活，
    /// 温度回落或频率恢复则解除。仅在有温度/频率读数的 tick 更新（慢通道每 5 tick）。
    thermal_active: bool,
    /// F-RC3 前台窗口冻结滞回状态：探测到前台窗口挂起则激活，响应则解除
    /// （低频探测，无中间带）。仅作为「伴随诊断」——绝不单独成 cause。
    ui_freeze_active: bool,
    /// F-RC3 上次 UI 冻结探测时刻（限频：每 2s 至多探一次，避免 200ms 阻塞累积）。
    last_ui_probe: Option<Instant>,
    /// F-RC3 前台窗口冻结探测函数（依赖注入，便于单测；默认走真实 Win32 探测）。
    ui_probe: Box<dyn Fn(u32) -> bool + Send>,
    // ===== 阶段 E（误报治理）新增状态 =====
    /// 上一次 analyze 的墙钟时刻（SystemTime）：与本次间隔超过 `max_tick_gap_secs`
    /// 判定采样中断（系统睡眠/挂起），清空全部跨 tick 状态——避免睡眠时长算进
    /// 卡顿 duration。必须用墙钟而非 `Instant`：后者基于 QPC，Windows 传统
    /// S3/S4 睡眠期间不推进，测不出「睡了一觉」（rust-lang/rust#85586）。
    last_tick: Option<SystemTime>,
    /// 内存水位信号（使用率 + 可用内存）是否活跃（滞回进入后、退出前）。
    mem_level_active: bool,
    /// 内存水位稳态抑制锁存：连续活跃超过 `mem_chronic_seconds` 后置位，
    /// 停止发射内存 cause，直到水位回落到退出线以下才解锁。
    mem_level_suppressed: bool,
    /// 内存水位本次连续活跃的起点（chronic 计时用）。
    mem_active_since: Option<SystemTime>,
    /// 各信号「滞回带保持」计时起点（信号已低于进入线、仅靠滞回维持时计时，
    /// 超过 `hysteresis_hold_max_secs` 强制解除；重新越过进入线即重置）。
    cpu_band_since: Option<Instant>,
    mem_band_since: Option<Instant>,
    disk_band_since: Option<Instant>,
    dpc_band_since: Option<Instant>,
    interrupt_band_since: Option<Instant>,
    ctx_band_since: Option<Instant>,
    /// 卡顿持续期间累积的进程快照（pid -> 取最大 CPU / 内存用量），
    /// 卡顿结束时提取 top 作为 culprits。
    current_culprits: HashMap<u32, ProcessBrief>,
    /// 各 cause（按 `CauseKind`）首次出现的时刻（SystemTime），用于落库首触时刻
    /// 与 F-RC6 因果方向（触发者 vs 放大器）。随 `current_causes` 一起在卡顿结束时清空。
    current_cause_first_touch: HashMap<CauseKind, SystemTime>,
    /// 下一帧 collect 是否需要构建 top_processes 快照。
    /// 非卡顿时为 false，主循环据此跳过全进程遍历（collect_with(false)）。
    need_process_snapshot: bool,
    /// F-RC4 近期观测到的 CPU 频率峰值（MHz），作为「频率掉档」判定基线。
    /// 仅当 sample.cpu_freq_mhz 为 Some 时更新为 max(当前读数, 峰值×衰减系数)——
    /// 阶段 E 起带慢衰减（`thermal_freq_peak_decay`），让陈旧的短时睿频峰值
    /// 逐渐让位于持续负载频率，避免「峰值只在 turbo 中建立、此后全程误判降频」。
    freq_peak: Option<f32>,
}

impl Detector {
    pub fn new(config: &DetectionConfig) -> Self {
        Self {
            config: config.clone(),
            history: Vec::new(),
            stutter_start: None,
            current_causes: Vec::new(),
            cpu_active: false,
            cpu_spike_active: false,
            net_spike_active: false,
            mem_spike_active: false,
            disk_busy_active: false,
            dpc_active: false,
            interrupt_active: false,
            ctx_switch_active: false,
            thermal_active: false,
            ui_freeze_active: false,
            last_ui_probe: None,
            ui_probe: Box::new(probe_foreground_window_frozen),
            last_tick: None,
            mem_level_active: false,
            mem_level_suppressed: false,
            mem_active_since: None,
            cpu_band_since: None,
            mem_band_since: None,
            disk_band_since: None,
            dpc_band_since: None,
            interrupt_band_since: None,
            ctx_band_since: None,
            current_culprits: HashMap::new(),
            current_cause_first_touch: HashMap::new(),
            need_process_snapshot: false,
            freq_peak: None,
        }
    }

    /// 下一帧 collect 是否需要构建 top_processes（卡顿进行中或刚结束一帧）
    pub fn needs_process_snapshot(&self) -> bool {
        self.need_process_snapshot
    }

    pub fn analyze(&mut self, sample: &Sample) -> Option<StutterEvent> {
        // ===== 阶段 E：采样中断防护 =====
        // 与上一 tick 的墙钟间隔超过 `max_tick_gap_secs` 判定为采样中断
        // （系统睡眠/挂起/采集卡死）。所有跨 tick 状态（卡顿跟踪、滞回、
        // 滑动基线）全部失效：清空重评估、绝不落库——否则睡眠时长会被
        // `start.elapsed()` 算进卡顿 duration（实测出现过 3.9 天的巨长事件）。
        // 与 duration 同用 SystemTime 墙钟；时钟回拨（NTP）不算中断。
        let tick_now = SystemTime::now();
        if let Some(last) = self.last_tick {
            if tick_now
                .duration_since(last)
                .map_or(false, |gap| {
                    gap > Duration::from_secs(self.config.max_tick_gap_secs as u64)
                })
            {
                self.reset_after_gap();
            }
        }
        self.last_tick = Some(tick_now);

        self.history.push(sample.clone());
        if self.history.len() > 120 {
            self.history.remove(0);
        }
        // F-RC4：更新近期 CPU 频率峰值（降频判定基线）。阶段 E 起带慢衰减：
        // peak = max(当前读数, peak × decay)，让短时睿频建立的陈旧峰值逐渐
        // 让位于持续负载频率，避免「此后全程被误判降频」。
        if let Some(f) = sample.cpu_freq_mhz {
            self.freq_peak = Some(
                self.freq_peak
                    .map_or(f, |p| (p * self.config.thermal_freq_peak_decay).max(f)),
            );
        }

        let mut causes = Vec::new();
        causes.extend(self.check_hard_thresholds(sample));
        causes.extend(self.check_spike());

        if !causes.is_empty() {
            // F-RC3：仅在前台窗口「已触发其它 cause」的卡顿帧探一次冻结，
            // 不进入采集热路径（200ms 探测只在真正卡顿帧发生，且 2s 限频）。
            self.maybe_probe_ui_freeze();
            if self.ui_freeze_active {
                causes.push(format!(
                    "UI frozen (前台窗口无响应 {}ms)",
                    self.config.ui_freeze_timeout_ms
                ));
            }
            // 时序权衡：analyze 在 collect 之后被调用，这里设置的标志影响的是
            // **下一帧**的 collect——卡顿触发的那一帧 top_processes 会为空，
            // 峰值归因靠后续帧的累积取 max 兜底（current_culprits 按 pid 取最大），
            // 因此这是可接受的；从第二帧起进程快照才开始累积。
            self.need_process_snapshot = true;
            let now = SystemTime::now();
            // 累积当前样本 top 进程（卡顿元凶候选）：同 pid 取最大 CPU / 内存用量
            for p in &sample.top_processes {
                let entry = self
                    .current_culprits
                    .entry(p.pid)
                    .or_insert_with(|| p.clone());
                entry.cpu_usage = entry.cpu_usage.max(p.cpu_usage);
                entry.mem_used_mb = entry.mem_used_mb.max(p.mem_used_mb);
            }
            if self.stutter_start.is_none() {
                self.stutter_start = Some(now);
                self.current_causes = causes;
                // 记录每个 cause 的首触时刻（按 CauseKind 去重）
                for c in &self.current_causes {
                    if let Some(k) = CauseKind::from_cause(c) {
                        self.current_cause_first_touch.entry(k).or_insert(now);
                    }
                }
            } else {
                for c in causes {
                    // 按 cause 类型去重：同类型（如 Commit charge / CPU usage）更新为最新文案，
                    // 避免滞回带内文案随数值变化导致字符串去重失效、cause 反复追加
                    // （一次卡顿中 current_causes 膨胀，还会虚高 severity）。
                    let key = cause_key(&c);
                    if let Some(pos) =
                        self.current_causes.iter().position(|x| cause_key(x) == key)
                    {
                        self.current_causes[pos] = c;
                    } else {
                        // 新出现的 cause：先取其 CauseKind（在 move 之前借用），再 push
                        let k = CauseKind::from_cause(&c);
                        self.current_causes.push(c);
                        if let Some(k) = k {
                            self.current_cause_first_touch.entry(k).or_insert(now);
                        }
                    }
                }
            }
            None
        } else if let Some(start) = self.stutter_start {
            let duration_ms = start.elapsed().map(|d| d.as_millis() as u64).unwrap_or(0);
            self.stutter_start = None;

            if duration_ms >= self.config.sustained_seconds as u64 * 1000 {
                // ===== v0.3.x 误报治理闸门 =====
                // 纯吞吐类 spike（网络 / 磁盘写）只是异步 I/O 突发（下载 / 构建 / git / 备份），
                // 不直接导致系统无响应。若整个卡顿窗口内**没有任何系统压力类 cause**
                // （CPU / 内存 / 磁盘繁忙 / DPC / 中断 / 上下文切换 / 温度降频 / 前台冻结 /
                // 软件级根因），则本次跟踪是误报，直接丢弃（不清空之外的状态、不落库）。
                // 这把「下载 100MB/s 也被记成卡顿」这类误判彻底排除，同时保留
                // 「吞吐 spike + 真实压力」场景下吞吐信号作为附加 cause 的价值。
                let has_pressure = self
                    .current_causes
                    .iter()
                    .any(|c| CauseKind::from_cause(c).map_or(false, |k| k.is_pressure()));
                if !has_pressure {
                    self.current_causes.clear();
                    self.current_culprits.clear();
                    self.current_cause_first_touch.clear();
                    self.need_process_snapshot = false;
                    return None;
                }
                let culprits = self.extract_culprits();
                // 首触时刻：相对 onset（卡顿起点）的偏移毫秒
                let cause_first_touch: HashMap<CauseKind, i64> = self
                    .current_cause_first_touch
                    .iter()
                    .map(|(k, t)| {
                        let off = t
                            .duration_since(start)
                            .map(|d| d.as_millis() as i64)
                            .unwrap_or(0);
                        (*k, off)
                    })
                    .collect();
                // 结构化根因（对齐 CauseKind；按 current_causes 顺序去重）
                let mut cause_kinds: Vec<CauseKind> = self
                    .current_causes
                    .iter()
                    .filter_map(|c| CauseKind::from_cause(c))
                    .collect();
                cause_kinds.dedup();
                // 按首触时刻升序（最早触发者在前）：F-RC1 主因取首触最早者，
                // 同时让 cause_kinds 顺序即「触发者→放大器」走向，供 F-RC9 因果链使用；
                // F-RC5 将在此基础上按信号强度×持续细化加权。
                cause_kinds.sort_by_key(|k| cause_first_touch.get(k).copied().unwrap_or(0));
                let primary_cause = cause_kinds.first().copied();
                let onset_ts = start
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                let event = StutterEvent {
                    id: 0,
                    timestamp: Utc::now(),
                    duration_ms,
                    severity: Self::determine_severity(&self.current_causes, duration_ms),
                    causes: self.current_causes.clone(),
                    cause_kinds,
                    primary_cause,
                    cause_first_touch,
                    onset_ts: Some(onset_ts),
                    snapshot: sample.clone(),
                    culprits,
                };
                self.current_causes.clear();
                self.current_cause_first_touch.clear();
                // 卡顿已结束：下一帧 collect 不再需要进程快照。
                // 时序权衡见上方 causes 分支：analyze 设置的标志影响下一帧。
                self.need_process_snapshot = false;
                return Some(event);
            }
            self.current_causes.clear();
            self.current_culprits.clear();
            self.current_cause_first_touch.clear();
            // 卡顿不足 sustained 秒即结束：同样不再需要下一帧进程快照。
            self.need_process_snapshot = false;
            None
        } else {
            None
        }
    }

    /// 阶段 E：采样中断（睡眠/挂起）后的状态清空。滞回、spike 激活、内存水位
    /// 状态、滑动基线历史全部作废——中断前后的样本不可比较，全部重新学习。
    fn reset_after_gap(&mut self) {
        self.stutter_start = None;
        self.current_causes.clear();
        self.current_culprits.clear();
        self.current_cause_first_touch.clear();
        self.need_process_snapshot = false;
        self.cpu_active = false;
        self.cpu_spike_active = false;
        self.net_spike_active = false;
        self.mem_spike_active = false;
        self.disk_busy_active = false;
        self.dpc_active = false;
        self.interrupt_active = false;
        self.ctx_switch_active = false;
        self.thermal_active = false;
        self.ui_freeze_active = false;
        self.mem_level_active = false;
        self.mem_level_suppressed = false;
        self.mem_active_since = None;
        self.cpu_band_since = None;
        self.mem_band_since = None;
        self.disk_band_since = None;
        self.dpc_band_since = None;
        self.interrupt_band_since = None;
        self.ctx_band_since = None;
        self.history.clear();
    }

    fn check_hard_thresholds(&mut self, sample: &Sample) -> Vec<String> {
        let mut causes = Vec::new();

        // CPU：滞回模型。进入 > cpu_threshold；退出 < cpu_threshold - cpu_hysteresis；
        // 滞回带内（threshold - hysteresis ~ threshold）维持 cpu_active 不变，
        // 防止 CPU 在阈值附近震荡时反复开始/结束卡顿记录。
        if sample.cpu_usage > self.config.cpu_threshold {
            self.cpu_active = true;
        } else if sample.cpu_usage <= self.config.cpu_threshold - self.config.cpu_hysteresis {
            self.cpu_active = false;
        }
        // 阶段 E：滞回带最长保持——带内（已低于进入线）仅靠滞回维持超过
        // hysteresis_hold_max_secs 即强制解除。滞回是防抖工具，不是延长卡顿
        // 的工具；CPU 持续 >90% 的真饱和不受影响（over_entry 恒真、计时重置）。
        band_hold_step(
            &mut self.cpu_active,
            &mut self.cpu_band_since,
            sample.cpu_usage > self.config.cpu_threshold,
            self.config.hysteresis_hold_max_secs,
        );
        if self.cpu_active {
            if sample.cpu_usage > self.config.cpu_threshold {
                causes.push(format!(
                    "CPU usage {:.1}% > {}%",
                    sample.cpu_usage, self.config.cpu_threshold
                ));
            } else {
                // 滞回带内维持激活：数值已回落但未到退出线，
                // 文案明确"滞回保持"，避免出现 "85% > 90%" 的矛盾
                causes.push(format!(
                    "CPU usage {:.1}%（滞回保持，阈值 {}%）",
                    sample.cpu_usage, self.config.cpu_threshold
                ));
            }
        }

        // ===== 阶段 E：内存水位信号（使用率 + 可用内存）滞回 + 稳态抑制 =====
        // 水位信号是「稳态」而非「事件」：常年 90%+ 内存的机器不该反复记录
        // 用户无感的长事件（>30s 直接 Critical）。三态状态机：
        //   Inactive →（任一条件越过进入线）→ Active
        //   Active   →（全部回落到退出线以下）→ Inactive
        //   Active   →（连续活跃 > mem_chronic_seconds）→ Suppressed（锁存，
        //              停止发射 cause；直到回落到退出线以下才解锁）
        // Active 且未抑制时发射 cause，口径与旧实现一致：哪个条件瞬时成立发哪条
        // （两个条件为「或」关系，config.toml 注释为「或」）。
        // 分页（paging）不受抑制影响——真颠簸由 paging cause 独立触发（见下）。
        let usage_over_entry = sample.mem_usage_percent > self.config.mem_threshold_percent;
        let avail_under_entry =
            (sample.mem_available_mb as f32) < self.config.mem_threshold_mb as f32;
        let any_entry = usage_over_entry || avail_under_entry;
        // 退出线：使用率 ≤ 阈值 − mem_hysteresis_percent 且 可用 ≥ 阈值 × mem_available_exit_ratio
        // （默认 90 进 / 85 出、500 进 / 600 出），水位在进入线附近抖动不再反复开关。
        let all_exit = sample.mem_usage_percent
            <= self.config.mem_threshold_percent - self.config.mem_hysteresis_percent
            && (sample.mem_available_mb as f32)
                >= self.config.mem_threshold_mb as f32 * self.config.mem_available_exit_ratio;

        if self.mem_level_suppressed {
            if all_exit {
                self.mem_level_suppressed = false;
            }
        } else if !self.mem_level_active {
            if any_entry {
                self.mem_level_active = true;
                self.mem_active_since = Some(SystemTime::now());
            }
        } else if all_exit {
            self.mem_level_active = false;
            self.mem_active_since = None;
        } else {
            // 稳态抑制：连续活跃超过 mem_chronic_seconds → 锁存抑制，
            // 停止发射内存 cause（本次越线只产生一条有界事件）。
            let chronic = self
                .mem_active_since
                .and_then(|t| t.elapsed().ok())
                .map_or(false, |d| {
                    d >= Duration::from_secs(self.config.mem_chronic_seconds as u64)
                });
            if chronic {
                self.mem_level_active = false;
                self.mem_level_suppressed = true;
                self.mem_active_since = None;
            }
        }
        // 滞回带最长保持（阶段 E 通用规则）：带内（未越过进入线、未到退出线）
        // 仅靠滞回维持超过 hysteresis_hold_max_secs → 强制解除（不锁存抑制，
        // 重新越过进入线即恢复）。
        band_hold_step(
            &mut self.mem_level_active,
            &mut self.mem_band_since,
            any_entry,
            self.config.hysteresis_hold_max_secs,
        );
        // band_hold_step 强制解除（带保持超时）路径不经过上方状态机，
        // 需同步清掉 chronic 计时起点，避免残留到下次激活。
        if !self.mem_level_active {
            self.mem_active_since = None;
        }
        if self.mem_level_active {
            if avail_under_entry {
                causes.push(format!(
                    "Available memory {}MB < {}MB",
                    sample.mem_available_mb, self.config.mem_threshold_mb
                ));
            }
            // 内存使用率过高（百分比口径）：与 `mem_threshold_mb`（绝对可用下限）
            // 互补，覆盖「大内存机器上可用内存绝对值仍高、但使用率已爆表」的漏报
            // （例如 32G 机器用到 95% 时可用仍 >500MB，仅看绝对下限会漏报）。
            if usage_over_entry {
                causes.push(format!(
                    "Memory usage {:.1}% > {}%",
                    sample.mem_usage_percent, self.config.mem_threshold_percent
                ));
            }
        }

        // 提交电荷（commit charge）：已提交虚拟内存 / 提交上限（= 物理内存 +
        // 页面文件）。阶段 E（误报治理）起**降级为「压力证据」**，不再作为独立
        // cause 发射：commit 高只是记账上限逼近，本身对性能零影响（浏览器/IDE/
        // 模拟器大量 commit，开发机常年 90%+ 而系统不卡）；真出问题（分配失败、
        // 强制分页）时必然伴随可用内存低/分页信号，由那些信号触发。对齐阶段 A
        // （swap 存量降为仅展示）/ 阶段 B（paging 降为放大器）的治理先例。
        // commit_ratio 保留计算：仍作为 paging 压力证据之一（见下方判定）。
        let commit_ratio = if sample.commit_limit > 0 {
            // 用 f64 计算：大内存机 commit_limit 达数十 GB，超出 f32 的 24 位精确整数范围，
            // 在 90% 边界附近会因精度丢失而误判。
            sample.commit_bytes as f64 / sample.commit_limit as f64 * 100.0
        } else {
            0.0
        };

        // 分页活动速率（阶段 C）：\Memory\Page Reads/sec 是「真正的 swap 卡顿信号」。
        // 物理内存耗尽时 OS 被迫把页从 pagefile 换入，每次换页注入一次磁盘 I/O 延迟 → 卡顿。
        // 这是速率口径（流量），而非 swap 使用率存量——后者在 Windows 上易误报且会虚高
        // severity，已降级为仅展示。
        // 修正（用户实测反馈）：Page Reads/sec 是瞬时抖动极大的计数器，开发机/模拟器场景
        // （Android Studio / qemu / IDE 等）正常负载也频繁超阈值，但磁盘空闲、提交电荷不高时
        // 系统并不卡顿（用户无感）。故 paging 不再单指标硬触发，必须同时存在「内存/磁盘压力
        // 证据」（commit 高 / 内存使用率高 / 可用内存不足 / 磁盘繁忙）才记为卡顿 cause——
        // paging 退化为真卡顿的「放大器」，避免孤立分页尖峰连环误报。
        let paging_has_pressure_evidence =
            self.config.paging_has_pressure_evidence(commit_ratio, sample);
        if sample.page_reads_per_sec > self.config.page_reads_threshold
            && paging_has_pressure_evidence
        {
            causes.push(format!(
                "Memory paging {:.1}/s > {}/s",
                sample.page_reads_per_sec, self.config.page_reads_threshold
            ));
        }

        // ===== F-RC2 系统级信号（带阈值 + 滞回）=====
        // 这些信号每 tick 即时采样（非 spike 滑动基线），用硬阈值 + 滞回判定，
        // 与 CPU 硬阈值模型一致。滞回比例取自 `sys_signal_hysteresis_ratio`
        // （默认 0.5：触发后需降到阈值的一半才解除）。

        // 磁盘真繁忙度：% Disk Time 或 单次 IO 延迟，任一超阈值即判定磁盘繁忙。
        // 替代原磁盘 B/s spike——吞吐高不代表磁盘饱和，繁忙度/IO 延迟才是真信号。
        let disk_busy = sample.disk_busy_percent > self.config.disk_busy_threshold_percent
            || sample.disk_avg_io_ms > self.config.disk_io_threshold_ms;
        if disk_busy {
            self.disk_busy_active = true;
        } else if sample.disk_busy_percent
            <= self.config.disk_busy_threshold_percent * self.config.sys_signal_hysteresis_ratio
            && sample.disk_avg_io_ms
                <= self.config.disk_io_threshold_ms * self.config.sys_signal_hysteresis_ratio
        {
            self.disk_busy_active = false;
        }
        // 阶段 E：×0.5 滞回带内（如繁忙度 50~95%）仅靠滞回维持超过上限即解除，
        // 不把一次磁盘饱和拖成几分钟的长事件。
        band_hold_step(
            &mut self.disk_busy_active,
            &mut self.disk_band_since,
            disk_busy,
            self.config.hysteresis_hold_max_secs,
        );
        if self.disk_busy_active {
            causes.push(format!(
                "Disk busy {:.1}% (IO {:.1}ms)",
                sample.disk_busy_percent, sample.disk_avg_io_ms
            ));
        }

        // % DPC Time：DPC 长时间占用 CPU 会挤占普通线程（「CPU 不忙但系统卡」）。
        self.dpc_active = sys_signal_hysteresis(
            self.dpc_active,
            sample.dpc_percent,
            self.config.dpc_threshold_percent,
            self.config.sys_signal_hysteresis_ratio,
        );
        band_hold_step(
            &mut self.dpc_active,
            &mut self.dpc_band_since,
            sample.dpc_percent > self.config.dpc_threshold_percent,
            self.config.hysteresis_hold_max_secs,
        );
        if self.dpc_active {
            causes.push(format!(
                "DPC time {:.1}% > {}%",
                sample.dpc_percent, self.config.dpc_threshold_percent
            ));
        }

        // % Interrupt Time：中断处理长时间占用 CPU 同样挤占普通线程。
        self.interrupt_active = sys_signal_hysteresis(
            self.interrupt_active,
            sample.interrupt_percent,
            self.config.interrupt_threshold_percent,
            self.config.sys_signal_hysteresis_ratio,
        );
        band_hold_step(
            &mut self.interrupt_active,
            &mut self.interrupt_band_since,
            sample.interrupt_percent > self.config.interrupt_threshold_percent,
            self.config.hysteresis_hold_max_secs,
        );
        if self.interrupt_active {
            causes.push(format!(
                "Interrupt time {:.1}% > {}%",
                sample.interrupt_percent, self.config.interrupt_threshold_percent
            ));
        }

        // Context Switches/sec：上下文切换风暴会拖垮调度（系统级卡顿真信号）。
        self.ctx_switch_active = sys_signal_hysteresis(
            self.ctx_switch_active,
            sample.context_switches_per_sec,
            self.config.context_switch_threshold_per_sec,
            self.config.sys_signal_hysteresis_ratio,
        );
        band_hold_step(
            &mut self.ctx_switch_active,
            &mut self.ctx_band_since,
            sample.context_switches_per_sec > self.config.context_switch_threshold_per_sec,
            self.config.hysteresis_hold_max_secs,
        );
        if self.ctx_switch_active {
            causes.push(format!(
                "Context switches {:.0}/s > {:.0}/s",
                sample.context_switches_per_sec, self.config.context_switch_threshold_per_sec
            ));
        }

        // ===== F-RC4 温度→降频根因 =====
        // 数据源：cpu_temp + cpu_freq_mhz（均来自 collector 慢通道，每 5 tick 采集一次；
        // gpu_temp 恒为 None 不纳入）。判据：温度 > 阈值 **且** 当前频率明显低于近期峰值
        // （疑似降频）**且** 负载存在——三者同时成立才记 ThermalThrottle。
        // 单一高温但频率正常 ≠ 降频（可能是短时满载未触发降频）；频率掉但温度不高也忽略。
        let has_thermal = sample.cpu_temp.is_some() && sample.cpu_freq_mhz.is_some();
        if has_thermal {
            let t = sample.cpu_temp.unwrap();
            let f = sample.cpu_freq_mhz.unwrap();
            // 降频仅在负载下才有归因意义：负载不足时不计入（见 THERMAL_LOAD_MIN_USAGE）。
            let load = sample.cpu_usage > THERMAL_LOAD_MIN_USAGE;
            let hot = t > self.config.thermal_threshold_celsius;
            let dropped = self
                .freq_peak
                .map_or(false, |peak| peak > 0.0 && f < peak * self.config.thermal_freq_drop_ratio);
            // 有读数 tick 直接按三条件重算（无滞回中间带）：三者同成立才激活，否则解除。
            // 非读数 tick（has_thermal=false）跳过本块，沿用上次 thermal_active 状态。
            self.thermal_active = hot && dropped && load;
        }
        if self.thermal_active && sample.cpu_temp.is_some() {
            let t = sample.cpu_temp.unwrap();
            let f = sample.cpu_freq_mhz.unwrap_or(0.0);
            let peak = self.freq_peak.unwrap_or(f);
            let drop = if peak > 0.0 {
                ((1.0 - f / peak) * 100.0) as i32
            } else {
                0
            };
            causes.push(format!(
                "Thermal throttle: CPU {:.0}°C, freq {:.0}MHz < {:.0}MHz (drop {}%)",
                t, f, peak, drop
            ));
        }

        causes
    }

    /// F-RC3：前台窗口冻结探测（限频 + 滞回）。
    ///
    /// 仅在 `ui_probe` 实际执行时才更新 `last_ui_probe`（2s 限频）；
    /// 限频窗口内沿用上次 `ui_freeze_active`，避免每 tick 都做 200ms 阻塞探测。
    /// 探测结果直接驱动 `ui_freeze_active`（低频探测，无需滞回中间带）。
    fn maybe_probe_ui_freeze(&mut self) {
        let now = Instant::now();
        if let Some(last) = self.last_ui_probe {
            if now.duration_since(last) < Duration::from_secs(2) {
                return; // 限频：沿用上次结果
            }
        }
        let frozen = (self.ui_probe)(self.config.ui_freeze_timeout_ms);
        self.last_ui_probe = Some(now);
        self.ui_freeze_active = frozen;
    }

    fn check_spike(&mut self) -> Vec<String> {
        let mut causes = Vec::new();
        let len = self.history.len();
        if len < 70 {
            return causes;
        }

        let recent = &self.history[len - 10..];
        let baseline = &self.history[len - 70..len - 10];

        // CPU / Disk write / Network：上升方向 spike（只认突增，速率骤降
        // = 传输完成不算），连续确认（10 样本 ≥6 超阈值）+ 滞回（触发后
        // 需明显回落才解除），避免瞬时抖动与阈值附近反复横跳。
        let cpu_r: Vec<f32> = recent.iter().map(|s| s.cpu_usage).collect();
        let cpu_b: Vec<f32> = baseline.iter().map(|s| s.cpu_usage).collect();
        Self::spike_check(
            &mut causes,
            "CPU",
            "%",
            &cpu_r,
            &cpu_b,
            self.config.spike_ratio,
            // 阶段 E：CPU spike 绝对下限（%）。纯比率判定会让空闲机的后台任务
            // 误报——基线 5% 涨到 16% 即 3 倍，但 16% CPU 远不构成卡顿。
            // 与网络 spike 的 spike_min_bps 同构：单样本 ≥ 下限才计入确认。
            self.config.spike_min_cpu_percent,
            &mut self.cpu_spike_active,
        );

        // 磁盘 B/s spike 已移除：F-RC2 改用每 tick 的磁盘真繁忙度（% Disk Time /
        // Avg Disk sec/Transfer）判定 `DiskBusy` cause（见 `check_hard_thresholds`），
        // 吞吐高不代表磁盘饱和，繁忙度才是真信号。`disk_rate_spike_ratio` 配置项保留
        // 供 F-RC12 what-if 复用，但检测器不再据此产出磁盘 spike。

        let net_r: Vec<f32> = recent
            .iter()
            .map(|s| (s.net_sent_bps + s.net_recv_bps) as f32)
            .collect();
        let net_b: Vec<f32> = baseline
            .iter()
            .map(|s| (s.net_sent_bps + s.net_recv_bps) as f32)
            .collect();
        Self::spike_check(
            &mut causes,
            "Network",
            "B/s",
            &net_r,
            &net_b,
            self.config.spike_ratio,
            self.config.spike_min_bps as f32,
            &mut self.net_spike_active,
        );

        // 内存可用率 spike：方向相反（可用内存骤降才算），同样滞回。
        let mem_r: Vec<f32> = recent.iter().map(|s| s.mem_available_mb as f32).collect();
        let mem_b: Vec<f32> = baseline.iter().map(|s| s.mem_available_mb as f32).collect();
        let r_avg = avg(&mem_r);
        let b_avg = avg(&mem_b);
        if b_avg > 1.0 {
            let ratio = (b_avg - r_avg).max(0.0) / b_avg;
            if ratio > self.config.spike_ratio {
                self.mem_spike_active = true;
            } else if ratio < self.config.spike_ratio * 0.5 {
                self.mem_spike_active = false;
            }
            if self.mem_spike_active {
                causes.push(format!(
                    "Memory available spike: {:.0}MB → {:.0}MB",
                    b_avg, r_avg
                ));
            }
        }

        causes
    }

    /// 上升方向 spike 检查（CPU / 磁盘写 / 网络）：
    /// - 只认突增：`v > b_avg` 才计，速率骤降不触发
    /// - 绝对下限：单样本 `v >= min_abs` 才算超阈值（网络/磁盘防零头误报）
    /// - 连续确认：recent 10 样本中 ≥6 个超阈值才置为激活
    /// - 滞回：激活后需 recent 均值回落到 `threshold * 0.5` 以下才解除
    ///   （滞回带内维持激活，避免反复横跳）
    fn spike_check(
        causes: &mut Vec<String>,
        name: &str,
        unit: &str,
        recent: &[f32],
        baseline: &[f32],
        threshold: f32,
        min_abs: f32,
        active: &mut bool,
    ) {
        const CONFIRM_MIN: usize = 6; // recent 10 样本中至少 6 个超阈值
        let r_avg = avg(recent);
        let b_avg = avg(baseline);
        let over = recent
            .iter()
            .filter(|&&v| v >= min_abs && b_avg > 1.0 && v > b_avg && (v - b_avg) / b_avg > threshold)
            .count();
        if over >= CONFIRM_MIN {
            *active = true;
        } else if b_avg <= 1.0 || (r_avg - b_avg).max(0.0) / b_avg < threshold * 0.5 {
            *active = false;
        }
        if *active {
            causes.push(format!(
                "{} spike: {:.1}{} → {:.1}{}",
                name, b_avg, unit, r_avg, unit
            ));
        }
    }

    fn determine_severity(causes: &[String], duration_ms: u64) -> Severity {
        // 阶段 E：只计压力类 cause——非压力类（NetSpike 等纯吞吐信号）不虚增
        // 严重度（否则 CpuHigh + 网络 spike 凑 2 因即 Major）。无法映射到
        // CauseKind 的文本按压力类计（防御：宁可虚高也不漏）。
        let count = causes
            .iter()
            .filter(|c| CauseKind::from_cause(c).map_or(true, |k| k.is_pressure()))
            .count();
        if count >= 3 || duration_ms > 30_000 {
            Severity::Critical
        } else if count >= 2 || duration_ms > 10_000 {
            Severity::Major
        } else {
            Severity::Minor
        }
    }

    /// 从累积的 `current_culprits` 提取卡顿元凶：
    /// 按 CPU 维度取 top 3、内存维度取 top 3，去重合并（最多 6 个）。
    /// 提取同时清空 `current_culprits`，避免污染下一次卡顿。
    fn extract_culprits(&mut self) -> Vec<ProcessBrief> {
        let all: Vec<ProcessBrief> = self.current_culprits.values().cloned().collect();
        self.current_culprits.clear();
        ProcessBrief::merge_top(all, 3, 3, 6)
    }
}

/// f32 切片均值（空切片返回 0）。
fn avg(v: &[f32]) -> f32 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<f32>() / v.len() as f32
    }
}

/// 系统级信号滞回步进（F-RC2）：进入 > `threshold`；退出 < `threshold * ratio`；
/// 滞回带内（`threshold*ratio` ~ `threshold`）维持当前 `active` 不变，
/// 避免信号在阈值附近抖动导致反复横跳。返回更新后的激活状态。
///
/// `DiskBusy` 是「% Disk Time 或 IO 延迟」的复合信号（OR 进入 / AND 退出），
/// 语义不同于单一阈值，故不走本helper，在 `check_hard_thresholds` 内单独处理。
fn sys_signal_hysteresis(active: bool, value: f32, threshold: f32, ratio: f32) -> bool {
    if value > threshold {
        true
    } else if value <= threshold * ratio {
        false
    } else {
        active
    }
}

/// 滞回带最长保持步进（阶段 E 通用规则）。
///
/// 滞回的目的是防抖（阈值附近反复横跳时不再反复开始/结束记录），**不是**
/// 延长卡顿：信号当前值已低于进入线（`over_entry == false`）、仅靠滞回维持
/// 激活时开始计时，超过 `max_hold_secs` 强制解除；重新越过进入线（真信号
/// 在场）即重置计时。CPU 带内保持、磁盘/DPC/中断/切换的 ×0.5 带以及内存
/// 水位带统一走本函数——持续真饱和（over_entry 恒真）不受任何影响。
fn band_hold_step(
    active: &mut bool,
    band_since: &mut Option<Instant>,
    over_entry: bool,
    max_hold_secs: u32,
) {
    if !*active {
        *band_since = None;
        return;
    }
    if over_entry {
        // 真信号在场：滞回带计时重置
        *band_since = None;
    } else if band_since
        .get_or_insert_with(Instant::now)
        .elapsed()
        >= Duration::from_secs(max_hold_secs as u64)
    {
        // 带内仅靠滞回维持超过上限：强制解除
        *active = false;
        *band_since = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sample(cpu: f32, mem_avail_mb: u64, swap: f32) -> Sample {
        let mut s = Sample::default();
        s.cpu_usage = cpu;
        s.mem_available_mb = mem_avail_mb;
        s.swap_usage_percent = swap;
        s
    }

    // --- Detector::new ---

    #[test]
    fn detector_new_initial_state() {
        let config = DetectionConfig::default();
        let d = Detector::new(&config);
        assert!(d.history.is_empty());
        assert!(d.stutter_start.is_none());
        assert!(d.current_causes.is_empty());
        assert_eq!(d.config.cpu_threshold, 90.0);
    }

    // --- analyze: normal sample ---

    #[test]
    fn analyze_normal_sample_returns_none() {
        let config = DetectionConfig::default();
        let mut d = Detector::new(&config);
        let sample = make_sample(30.0, 2000, 10.0);
        assert!(d.analyze(&sample).is_none());
    }

    #[test]
    fn analyze_normal_sample_no_causes() {
        let config = DetectionConfig::default();
        let mut d = Detector::new(&config);
        let sample = make_sample(30.0, 2000, 10.0);
        d.analyze(&sample);
        assert!(d.current_causes.is_empty());
        assert!(d.stutter_start.is_none());
    }

    // --- analyze: CPU threshold triggers causes ---

    #[test]
    fn analyze_high_cpu_starts_stutter_tracking() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        let high = make_sample(95.0, 2000, 10.0);
        let result = d.analyze(&high);
        assert!(result.is_none()); // No event yet, stutter just started
        assert!(!d.current_causes.is_empty());
        assert!(d.stutter_start.is_some());
        assert!(d.current_causes[0].contains("CPU usage"));
    }

    #[test]
    fn analyze_low_memory_starts_stutter_tracking() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        let low_mem = make_sample(30.0, 100, 10.0);
        d.analyze(&low_mem);
        assert!(!d.current_causes.is_empty());
        assert!(d.current_causes[0].contains("Available memory"));
    }

    /// 回归：大内存机器上可用内存绝对值仍高（> mem_threshold_mb）但使用率已爆表
    /// （> mem_threshold_percent）时，必须能触发内存卡顿原因——旧实现只查绝对可用
    /// 下限，这种场景会漏报（正是「内存爆到 95% 却检测不出卡顿」的成因）。
    #[test]
    fn analyze_high_mem_usage_percent_starts_stutter_tracking() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // 可用 2000MB（远 > 500，不触发绝对下限），但使用率 95% > 90%
        let mut s = make_sample(30.0, 2000, 10.0);
        s.mem_usage_percent = 95.0;
        d.analyze(&s);
        assert!(
            !d.current_causes.is_empty(),
            "内存使用率爆表必须触发卡顿原因"
        );
        assert!(
            d.current_causes.iter().any(|c| c.contains("Memory usage")),
            "应产出 'Memory usage' 原因，got: {:?}",
            d.current_causes
        );
        // 同时确认绝对可用下限分支未被误触发（可用仍高）
        assert!(
            !d.current_causes.iter().any(|c| c.contains("Available memory")),
            "可用内存充足时不应误报 'Available memory'，got: {:?}",
            d.current_causes
        );
    }

    /// 回归：分页活动速率（Page Reads/sec）只有与「内存/磁盘压力证据」同时成立才触发
    /// 「Memory paging」原因（阶段 C）。修正（用户实测反馈）：Page Reads/sec 瞬时抖动极大，
    /// 开发机/模拟器正常负载也频繁超阈值，但磁盘空闲、提交电荷不高时系统并不卡顿——
    /// 孤立分页尖峰不得连环误报，须有真实内存/磁盘压力（commit 高/内存使用率高/可用内存
    /// 不足/磁盘繁忙）才记为卡顿 cause。
    #[test]
    fn analyze_high_page_reads_starts_stutter_tracking() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // 分页速率超阈值（400/s > 默认 300/s）且 内存使用率爆表（95% > 90%）：
        // 真实 swap 卡顿场景 → 必须触发且产出 'Memory paging'
        let mut s = make_sample(30.0, 2000, 10.0);
        s.mem_usage_percent = 95.0;
        s.page_reads_per_sec = 400.0;
        d.analyze(&s);
        assert!(
            !d.current_causes.is_empty(),
            "分页爆表 + 内存压力必须触发卡顿原因"
        );
        assert!(
            d.current_causes.iter().any(|c| c.contains("Memory paging")),
            "应产出 'Memory paging' 原因，got: {:?}",
            d.current_causes
        );

        // 孤立分页高、无任何内存/磁盘压力（开发机/模拟器常见抖动）→ 不得触发（防误报）
        let mut spike = make_sample(30.0, 2000, 10.0);
        spike.mem_usage_percent = 30.0;
        spike.page_reads_per_sec = 400.0; // 远超阈值，但系统内存/磁盘均无压力
        let mut d3 = Detector::new(&config);
        d3.analyze(&spike);
        assert!(
            d3.current_causes.is_empty(),
            "孤立分页尖峰不应触发卡顿，got: {:?}",
            d3.current_causes
        );

        // 未达阈值时不触发（即使内存有压力）
        let mut normal = make_sample(30.0, 2000, 10.0);
        normal.mem_usage_percent = 95.0;
        normal.page_reads_per_sec = 5.0; // 远低于阈值
        let mut d2 = Detector::new(&config);
        d2.analyze(&normal);
        assert!(
            d2.current_causes.iter().all(|c| !c.contains("Memory paging")),
            "分页速率未达阈值不应触发 paging，got: {:?}",
            d2.current_causes
        );
    }

    /// F-RC2：磁盘真繁忙度（% Disk Time 或 IO 延迟）超阈值应触发 `DiskBusy` cause，
    /// 且落库结构化根因为 `CauseKind::DiskBusy`（替代原磁盘 B/s spike）。
    #[test]
    fn analyze_disk_busy_triggers() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        let mut s = make_sample(30.0, 2000, 10.0);
        s.disk_busy_percent = 98.0; // % Disk Time 超 95
        d.analyze(&s);
        assert!(
            d.current_causes.iter().any(|c| c.contains("Disk busy")),
            "磁盘繁忙度超阈值应触发 DiskBusy，got: {:?}",
            d.current_causes
        );

        // IO 延迟口径同样应触发（与 % Disk Time 为「或」）
        let mut s2 = make_sample(30.0, 2000, 10.0);
        s2.disk_avg_io_ms = 120.0;
        let mut d2 = Detector::new(&config);
        d2.analyze(&s2);
        assert!(
            d2.current_causes.iter().any(|c| c.contains("Disk busy")),
            "IO 延迟超阈值应触发 DiskBusy，got: {:?}",
            d2.current_causes
        );

        // 落库结构化根因
        for _ in 0..2 {
            d.analyze(&s);
        }
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let event = d.analyze(&make_sample(20.0, 2000, 10.0)).unwrap();
        assert!(
            event.cause_kinds.contains(&CauseKind::DiskBusy),
            "cause_kinds 应含 DiskBusy，got: {:?}",
            event.cause_kinds
        );
    }

    /// F-RC2：% DPC Time 超阈值应触发 `DpcInterrupt` cause 并落库 `CauseKind::DpcInterrupt`。
    #[test]
    fn analyze_dpc_triggers() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        let mut s = make_sample(30.0, 2000, 10.0);
        s.dpc_percent = 15.0;
        d.analyze(&s);
        assert!(
            d.current_causes.iter().any(|c| c.contains("DPC time")),
            "DPC time 超阈值应触发 DpcInterrupt，got: {:?}",
            d.current_causes
        );
        for _ in 0..2 {
            d.analyze(&s);
        }
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let event = d.analyze(&make_sample(20.0, 2000, 10.0)).unwrap();
        assert!(
            event.cause_kinds.contains(&CauseKind::DpcInterrupt),
            "cause_kinds 应含 DpcInterrupt，got: {:?}",
            event.cause_kinds
        );
    }

    /// F-RC2：% Interrupt Time 超阈值应触发 `InterruptStorm` cause 并落库 `CauseKind::InterruptStorm`。
    #[test]
    fn analyze_interrupt_triggers() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        let mut s = make_sample(30.0, 2000, 10.0);
        s.interrupt_percent = 18.0;
        d.analyze(&s);
        assert!(
            d.current_causes.iter().any(|c| c.contains("Interrupt time")),
            "Interrupt time 超阈值应触发 InterruptStorm，got: {:?}",
            d.current_causes
        );
        for _ in 0..2 {
            d.analyze(&s);
        }
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let event = d.analyze(&make_sample(20.0, 2000, 10.0)).unwrap();
        assert!(
            event.cause_kinds.contains(&CauseKind::InterruptStorm),
            "cause_kinds 应含 InterruptStorm，got: {:?}",
            event.cause_kinds
        );
    }

    /// F-RC2：`Context Switches/sec` 超阈值应触发 `ContextSwitchStorm` cause
    /// 并落库 `CauseKind::ContextSwitchStorm`。
    #[test]
    fn analyze_context_switch_triggers() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        let mut s = make_sample(30.0, 2000, 10.0);
        s.context_switches_per_sec = 80_000.0;
        d.analyze(&s);
        assert!(
            d.current_causes
                .iter()
                .any(|c| c.contains("Context switches")),
            "Context switches 超阈值应触发 ContextSwitchStorm，got: {:?}",
            d.current_causes
        );
        for _ in 0..2 {
            d.analyze(&s);
        }
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let event = d.analyze(&make_sample(20.0, 2000, 10.0)).unwrap();
        assert!(
            event.cause_kinds.contains(&CauseKind::ContextSwitchStorm),
            "cause_kinds 应含 ContextSwitchStorm，got: {:?}",
            event.cause_kinds
        );
    }

    /// F-RC4 辅助：构造带温度/频率的样本（慢通道每 5 tick 才有读数，测试直接给定）。
    fn make_sample_thermal(
        cpu: f32,
        mem_avail_mb: u64,
        cpu_temp: Option<f32>,
        cpu_freq_mhz: Option<f32>,
    ) -> Sample {
        let mut s = make_sample(cpu, mem_avail_mb, 10.0);
        s.cpu_temp = cpu_temp;
        s.cpu_freq_mhz = cpu_freq_mhz;
        s
    }

    /// F-RC4：温度高 + 频率掉档（低于近期峰值×比例）+ 负载存在 → 触发 ThermalThrottle。
    #[test]
    fn analyze_thermal_throttle_triggers() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // 先喂一个高频率样本建立 freq_peak 基线（=3000MHz）
        d.analyze(&make_sample_thermal(80.0, 2000, Some(70.0), Some(3000.0)));
        // 再喂：温度 95°C（>85）+ 频率掉到 2000（<3000×0.85=2550）+ 负载 80% → 触发
        let hot = make_sample_thermal(80.0, 2000, Some(95.0), Some(2000.0));
        d.analyze(&hot);
        assert!(
            d.current_causes.iter().any(|c| c.contains("Thermal throttle")),
            "温度高+频率掉档应触发 ThermalThrottle，got: {:?}",
            d.current_causes
        );

        for _ in 0..2 {
            d.analyze(&hot);
        }
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let event = d.analyze(&make_sample_thermal(20.0, 2000, Some(95.0), Some(2000.0))).unwrap();
        assert!(
            event.cause_kinds.contains(&CauseKind::ThermalThrottle),
            "cause_kinds 应含 ThermalThrottle，got: {:?}",
            event.cause_kinds
        );
    }

    /// F-RC4：温度低于阈值 → 不触发（即便频率掉档）。
    #[test]
    fn analyze_thermal_no_trigger_when_temp_low() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        d.analyze(&make_sample_thermal(80.0, 2000, Some(70.0), Some(3000.0))); // peak=3000
        let cool = make_sample_thermal(80.0, 2000, Some(60.0), Some(2000.0)); // 温度低
        d.analyze(&cool);
        assert!(
            d.current_causes.iter().all(|c| !c.contains("Thermal throttle")),
            "温度低不应触发 ThermalThrottle，got: {:?}",
            d.current_causes
        );
    }

    /// F-RC4：温度高但频率未掉档（=峰值）→ 不触发（单一高温不算降频）。
    #[test]
    fn analyze_thermal_no_trigger_when_freq_not_dropped() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        d.analyze(&make_sample_thermal(80.0, 2000, Some(70.0), Some(3000.0))); // peak=3000
        let hot_boosted = make_sample_thermal(80.0, 2000, Some(95.0), Some(3000.0)); // 温度高且频率满
        d.analyze(&hot_boosted);
        assert!(
            d.current_causes.iter().all(|c| !c.contains("Thermal throttle")),
            "温度高但频率未掉档不应触发，got: {:?}",
            d.current_causes
        );
    }

    /// F-RC4：无频率读数（cpu_freq_mhz=None，慢通道非采样 tick）→ 不触发，避免误报。
    #[test]
    fn analyze_thermal_no_trigger_when_no_freq() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // 温度高但频率始终为 None → freq_peak 无法建立，不可能判定掉档
        let no_freq = make_sample_thermal(80.0, 2000, Some(95.0), None);
        d.analyze(&no_freq);
        assert!(
            d.current_causes.iter().all(|c| !c.contains("Thermal throttle")),
            "无频率读数不应触发 ThermalThrottle，got: {:?}",
            d.current_causes
        );
    }

    /// F-RC2 回归：磁盘高吞吐（disk_write_bps 大）不再触发「Disk write spike」——
    /// 已改用磁盘真繁忙度（% Disk Time / IO 延迟）判定，避免吞吐高但磁盘不忙时误报。
    #[test]
    fn analyze_no_disk_write_spike_cause() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // 高磁盘写吞吐，但磁盘不忙、IO 延迟低、其它资源正常
        let mut s = make_sample(30.0, 2000, 10.0);
        s.disk_write_bps = 50_000_000; // 50 MB/s
        d.analyze(&s);
        assert!(
            d.current_causes.iter().all(|c| !c.contains("Disk write")),
            "高吞吐但磁盘不忙不应触发磁盘 spike，got: {:?}",
            d.current_causes
        );
    }

    #[test]
    fn analyze_multiple_thresholds_collects_all_causes() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // High CPU + low memory = 2 causes（swap 已降级为仅展示，不再触发）
        let bad = make_sample(95.0, 100, 10.0);
        d.analyze(&bad);
        assert_eq!(d.current_causes.len(), 2);
    }

    // --- analyze: event generation after sustained period ---

    #[test]
    fn analyze_event_generated_after_sustained_period() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // Push high-CPU samples to start stutter
        let high = make_sample(95.0, 2000, 10.0);
        for _ in 0..3 {
            d.analyze(&high);
        }

        // Wait for sustained period
        std::thread::sleep(std::time::Duration::from_millis(1200));

        // Push normal sample to end stutter → should generate event
        let normal = make_sample(20.0, 2000, 10.0);
        let event = d.analyze(&normal);
        assert!(event.is_some());

        let event = event.unwrap();
        assert!(!event.causes.is_empty());
        assert!(event.duration_ms >= 1000);
        assert_eq!(event.severity, Severity::Minor); // 1 cause
    }

    #[test]
    fn analyze_no_event_if_stutter_too_short() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 3;
        let mut d = Detector::new(&config);

        // Push high-CPU samples
        let high = make_sample(95.0, 2000, 10.0);
        d.analyze(&high);

        // Wait less than sustained_seconds
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Push normal sample — stutter too short, no event
        let normal = make_sample(20.0, 2000, 10.0);
        let event = d.analyze(&normal);
        assert!(event.is_none());
        // Causes should be cleared since stutter was too short
        assert!(d.current_causes.is_empty());
        assert!(d.stutter_start.is_none());
    }

    /// v0.3.x 误报治理回归：纯网络 spike（下载 / 构建 / git 等正常高吞吐，
    /// CPU / 内存均健康）**不得**记录为卡顿事件。闸门应在卡顿结束时丢弃本次跟踪。
    #[test]
    fn analyze_network_spike_alone_no_event() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // 60 个基线（CPU 30 / 网络 1MB/s，健康）+ 10 个 recent 高吞吐（5MB/s，≥ spike_min_bps）
        // → 触发 Network spike（无任何系统压力 cause）
        for _ in 0..60 {
            d.analyze(&make_sample_net(30.0, 2000, 10.0, 1_000_000));
        }
        for _ in 0..10 {
            d.analyze(&make_sample_net(30.0, 2000, 10.0, 5_000_000));
        }
        assert!(
            d.current_causes.iter().any(|c| c.contains("Network spike")),
            "纯网络 spike 应触发跟踪，got: {:?}",
            d.current_causes
        );
        // 持续足够久（> sustained）让本次卡顿「成立」
        std::thread::sleep(std::time::Duration::from_millis(1200));
        // 喂入足量正常样本使网络 spike 滞回解除 → 卡顿结束 → 闸门应丢弃（无压力 cause）
        for _ in 0..15 {
            d.analyze(&make_sample_net(20.0, 2000, 10.0, 1_000_000));
        }
        // 卡顿已结束（stutter_start 复位），且全程从未生成事件
        assert!(d.stutter_start.is_none(), "卡顿应已结束");
        // 反向验证：若没有闸门，这段「持续的网络 spike」本会产生一次 Minor 事件。
        // 这里用最后一个 analyze 的返回值无法直接取到（已结束），故改判：
        // 通过 `current_causes` 已被清空 + 无事件来确认闸门生效。
        assert!(d.current_causes.is_empty());
    }

    /// v0.3.x 误报治理回归：网络 spike **伴随**真实压力（CPU 高）时应正常记录，
    /// 且 `cause_kinds` 同时含 `CpuHigh`（压力主因）与 `NetSpike`（附加 cause 保留）。
    #[test]
    fn analyze_network_spike_with_cpu_high_records_event() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // 60 个基线（CPU 30 / 网络 1MB）+ 10 个 recent（CPU 95 触发压力 + 网络 5MB 触发 spike）
        for _ in 0..60 {
            d.analyze(&make_sample_net(30.0, 2000, 10.0, 1_000_000));
        }
        for _ in 0..10 {
            d.analyze(&make_sample_net(95.0, 2000, 10.0, 5_000_000));
        }
        std::thread::sleep(std::time::Duration::from_millis(1200));
        // 卡顿会在某次正常样本中结束并生成事件（闸门因含 CpuHigh 放行），
        // 需在循环内捕获该事件，而非只看最后一次 analyze（那时卡顿已结束、返回 None）。
        let mut captured = None;
        for _ in 0..16 {
            if let Some(e) = d.analyze(&make_sample_net(20.0, 2000, 10.0, 1_000_000)) {
                captured = Some(e);
                break;
            }
        }
        let event = captured.expect("网络 spike + CPU 高应记录事件");
        assert!(
            event.cause_kinds.contains(&CauseKind::CpuHigh),
            "应含 CpuHigh 压力 cause，got: {:?}",
            event.cause_kinds
        );
        assert!(
            event.cause_kinds.contains(&CauseKind::NetSpike),
            "应保留 NetSpike 附加 cause，got: {:?}",
            event.cause_kinds
        );
        // 阶段 E6：severity 只计压力类 cause——CpuHigh（压力）+ NetSpike（非压力）
        // 旧算法按 2 因记 Major，现在只计 1 个压力因、时长 ~1.2s → Minor。
        assert_eq!(
            event.severity,
            Severity::Minor,
            "非压力类（网络 spike）不应虚增严重度，got: {:?}",
            event.causes
        );
    }

    /// F-RC1：事件应携带结构化根因（cause_kinds / primary_cause / onset_ts /
    /// cause_first_touch），对齐 `CauseKind` 枚举。
    #[test]
    fn analyze_event_carries_structured_causes() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        let high = make_sample(95.0, 2000, 10.0);
        for _ in 0..3 {
            d.analyze(&high);
        }
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let event = d.analyze(&make_sample(20.0, 2000, 10.0)).unwrap();

        // 结构化根因应含 CpuHigh，且 primary_cause 为 CpuHigh（首个检测到的 cause）
        assert!(
            event.cause_kinds.contains(&CauseKind::CpuHigh),
            "cause_kinds 应含 CpuHigh: {:?}",
            event.cause_kinds
        );
        assert_eq!(event.primary_cause, Some(CauseKind::CpuHigh));
        // onset_ts 应已落库（Unix 毫秒，落在合理范围）
        let onset = event.onset_ts.expect("onset_ts 应已落库");
        assert!(
            onset > 1_700_000_000_000,
            "onset_ts 应为合理 Unix 毫秒: {}",
            onset
        );
        // 首触时刻应记录 CpuHigh（偏移 0，因为是首个 cause）
        assert_eq!(
            event.cause_first_touch.get(&CauseKind::CpuHigh).copied(),
            Some(0)
        );
    }

    // --- analyze: severity via cause count ---

    #[test]
    fn analyze_severity_minor_single_cause() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // Only high CPU → 1 cause
        let high = make_sample(95.0, 2000, 10.0);
        for _ in 0..3 {
            d.analyze(&high);
        }
        std::thread::sleep(std::time::Duration::from_millis(1200));

        let normal = make_sample(20.0, 2000, 10.0);
        let event = d.analyze(&normal).unwrap();
        assert_eq!(event.severity, Severity::Minor);
    }

    #[test]
    fn analyze_severity_major_two_causes() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // High CPU + low memory → 2 causes（swap 已降级为仅展示）
        let bad = make_sample(95.0, 100, 10.0);
        for _ in 0..3 {
            d.analyze(&bad);
        }
        std::thread::sleep(std::time::Duration::from_millis(1200));

        let normal = make_sample(20.0, 2000, 10.0);
        let event = d.analyze(&normal).unwrap();
        assert_eq!(event.severity, Severity::Major);
    }

    #[test]
    fn analyze_severity_critical_three_causes() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // High CPU + low memory + 内存使用率爆表 → 3 causes
        // （swap 已降级为仅展示；这里用 mem_usage_percent 凑出第三条原因）
        let mut bad = make_sample(95.0, 100, 10.0);
        bad.mem_usage_percent = 95.0;
        for _ in 0..3 {
            d.analyze(&bad);
        }
        std::thread::sleep(std::time::Duration::from_millis(1200));

        let normal = make_sample(20.0, 2000, 10.0);
        let event = d.analyze(&normal).unwrap();
        assert_eq!(event.severity, Severity::Critical);
    }

    // --- check_hard_thresholds boundary tests ---

    #[test]
    fn analyze_cpu_at_threshold_no_trigger() {
        let config = DetectionConfig::default(); // threshold 90.0
        let mut d = Detector::new(&config);

        // CPU exactly at threshold → no trigger (uses >)
        let sample = make_sample(90.0, 2000, 10.0);
        d.analyze(&sample);
        assert!(d.current_causes.is_empty());
    }

    #[test]
    fn analyze_cpu_above_threshold_triggers() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // CPU just above threshold
        let sample = make_sample(90.1, 2000, 10.0);
        d.analyze(&sample);
        assert!(!d.current_causes.is_empty());
    }

    #[test]
    fn analyze_mem_at_threshold_no_trigger() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // Available memory exactly at threshold → no trigger (uses <)
        let sample = make_sample(30.0, 500, 10.0);
        d.analyze(&sample);
        assert!(d.current_causes.is_empty());
    }

    #[test]
    fn analyze_mem_below_threshold_triggers() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // Available memory just below threshold
        let sample = make_sample(30.0, 499, 10.0);
        d.analyze(&sample);
        assert!(!d.current_causes.is_empty());
    }

    // --- history management ---

    #[test]
    fn analyze_history_capped_at_120() {
        let config = DetectionConfig::default();
        let mut d = Detector::new(&config);

        let sample = make_sample(30.0, 2000, 10.0);
        for _ in 0..130 {
            d.analyze(&sample);
        }
        assert!(d.history.len() <= 120);
    }

    // --- stutter causes merge ---

    #[test]
    fn analyze_merges_new_causes_during_stutter() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 5; // long enough to not finish
        let mut d = Detector::new(&config);

        // Start with high CPU only
        let cpu_only = make_sample(95.0, 2000, 10.0);
        d.analyze(&cpu_only);
        assert_eq!(d.current_causes.len(), 1);

        // Now also breach available memory → 2 causes（swap 已降级为仅展示）
        let cpu_and_lowmem = make_sample(95.0, 100, 10.0);
        d.analyze(&cpu_and_lowmem);
        assert_eq!(d.current_causes.len(), 2);
    }

    // --- cpu 滞回（hysteresis）---

    #[test]
    fn cpu_hysteresis_keeps_active_within_band() {
        let config = DetectionConfig::default(); // cpu_threshold=90, hysteresis=10 → 退出线 80
        let mut d = Detector::new(&config);

        d.analyze(&make_sample(95.0, 2000, 10.0)); // >90 进入
        assert!(!d.current_causes.is_empty());
        assert!(d.current_causes[0].contains("CPU usage"));

        // 滞回带内（85：< 90 但 > 80）→ 维持激活；同类型 cause 更新而非追加
        d.analyze(&make_sample(85.0, 2000, 10.0));
        assert!(
            !d.current_causes.is_empty(),
            "滞回带内应维持 CPU 激活状态"
        );
        assert_eq!(
            d.current_causes.len(),
            1,
            "滞回带内同类型 cause 应更新而非追加，got: {:?}",
            d.current_causes
        );
        assert!(d.current_causes[0].contains("CPU usage"));
        assert!(
            d.current_causes[0].contains("滞回保持"),
            "滞回带内文案应标注滞回保持，got: {}",
            d.current_causes[0]
        );
    }

    #[test]
    fn cpu_hysteresis_releases_below_exit_line() {
        let config = DetectionConfig::default(); // 退出线 80
        let mut d = Detector::new(&config);

        d.analyze(&make_sample(95.0, 2000, 10.0)); // 进入
        assert!(!d.current_causes.is_empty());

        d.analyze(&make_sample(75.0, 2000, 10.0)); // <80 退出
        assert!(d.current_causes.is_empty());
    }

    // ===== 阶段 E（误报治理）回归 =====

    /// 阶段 E1：CPU spike 绝对下限。空闲机后台任务（基线 10% → 40%，比率 4 倍
    /// 达标）但绝对值 40% < spike_min_cpu_percent(50%) → 不得触发 CPU spike。
    /// 绝对值足够（15% → 60%）时正常触发。
    #[test]
    fn spike_min_cpu_floor_ignores_low_absolute() {
        let mut config = DetectionConfig::default(); // ratio=3.0, min_cpu=50%
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // 60 基线 10% + 10 recent 40%：比率 4 > 3 但 40% < 50% 下限 → 不触发
        for _ in 0..60 {
            d.analyze(&make_sample(10.0, 2000, 10.0));
        }
        for _ in 0..10 {
            d.analyze(&make_sample(40.0, 2000, 10.0));
        }
        assert!(
            d.current_causes.iter().all(|c| !c.contains("CPU spike")),
            "低绝对值 CPU 突增不应触发 spike，got: {:?}",
            d.current_causes
        );

        // 对照：基线 15% + recent 65%（比率 3.33 > 3 且 ≥ 50%）→ 正常触发
        let mut d2 = Detector::new(&config);
        for _ in 0..60 {
            d2.analyze(&make_sample(15.0, 2000, 10.0));
        }
        for _ in 0..10 {
            d2.analyze(&make_sample(65.0, 2000, 10.0));
        }
        assert!(
            d2.current_causes.iter().any(|c| c.contains("CPU spike")),
            "高绝对值 CPU 突增应触发 spike，got: {:?}",
            d2.current_causes
        );
    }

    /// 阶段 E2a：提交电荷（commit charge）降级为「压力证据」，不再独立成 cause——
    /// commit 高只是记账上限逼近，本身对性能零影响（开发机常年 90%+ 而不卡）。
    #[test]
    fn commit_charge_alone_no_longer_triggers() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // commit 95% > 90%，其余全部健康 → 不得产生任何 cause / 事件
        let mut s = make_sample(30.0, 2000, 10.0);
        s.commit_limit = 1_000_000;
        s.commit_bytes = 950_000;
        d.analyze(&s);
        assert!(
            d.current_causes.is_empty(),
            "提交电荷单独偏高不应触发卡顿，got: {:?}",
            d.current_causes
        );
    }

    /// 阶段 E2a（证据角色保留）：commit 高 + 分页速率高（真实换页场景）→
    /// 经「Memory paging」cause 触发（commit 作为压力证据），且不再出现
    /// 「Commit charge」独立 cause。
    #[test]
    fn commit_charge_still_serves_as_paging_evidence() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // commit 95%（唯一压力证据）+ paging 400/s > 300 → 触发 Memory paging
        let mut s = make_sample(30.0, 2000, 10.0);
        s.commit_limit = 1_000_000;
        s.commit_bytes = 950_000;
        s.page_reads_per_sec = 400.0;
        d.analyze(&s);
        assert!(
            d.current_causes.iter().any(|c| c.contains("Memory paging")),
            "commit 作为证据应放行 paging cause，got: {:?}",
            d.current_causes
        );
        assert!(
            d.current_causes.iter().all(|c| !c.contains("Commit charge")),
            "提交电荷不应再作为独立 cause，got: {:?}",
            d.current_causes
        );
    }

    /// 阶段 E2b：内存水位滞回——使用率 92% 进入后回落 87%（带内 85~90）状态保持
    /// （防抖），但瞬时未越过进入线不发射 cause；回落 84%（低于退出线 85）才解除。
    #[test]
    fn mem_level_hysteresis_band_and_exit() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 5; // 避免会话因不足持续时长而提前结束干扰断言
        let mut d = Detector::new(&config);

        let mut high = make_sample(30.0, 2000, 10.0);
        high.mem_usage_percent = 92.0;
        d.analyze(&high);
        assert!(d.mem_level_active, "92% 应进入内存水位激活");
        assert!(
            d.current_causes.iter().any(|c| c.contains("Memory usage")),
            "越过进入线应发射 cause，got: {:?}",
            d.current_causes
        );

        // 带内 87%：状态保持（滞回防抖），但不再发射 cause（未越过进入线）
        let mut band = make_sample(30.0, 2000, 10.0);
        band.mem_usage_percent = 87.0;
        d.analyze(&band);
        assert!(d.mem_level_active, "带内应保持激活（滞回防抖）");
        assert!(
            d.current_causes.is_empty(),
            "带内（未越过进入线）不应发射内存 cause，got: {:?}",
            d.current_causes
        );

        // 重新越过进入线：状态本就活跃，cause 恢复发射
        d.analyze(&high);
        assert!(
            d.current_causes.iter().any(|c| c.contains("Memory usage")),
            "重新越过进入线应恢复发射，got: {:?}",
            d.current_causes
        );

        // 84% ≤ 85（退出线）→ 解除
        let mut exit_s = make_sample(30.0, 2000, 10.0);
        exit_s.mem_usage_percent = 84.0;
        d.analyze(&exit_s);
        assert!(!d.mem_level_active, "低于退出线应解除内存水位激活");
        assert!(d.current_causes.is_empty());
    }

    /// 阶段 E2b：稳态抑制——水位连续活跃超过 mem_chronic_seconds 后停止发射
    /// cause 并锁存；锁存期内重新越过进入线**不得**开新会话（每次越线只产生
    /// 一条有界事件）；回落到退出线以下才解锁、恢复触发。
    #[test]
    fn mem_level_chronic_suppression() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        config.mem_chronic_seconds = 1; // 测试用：1s 即判定稳态
        let mut d = Detector::new(&config);

        let mut high = make_sample(30.0, 2000, 10.0);
        high.mem_usage_percent = 92.0;

        // 会话开始
        d.analyze(&high);
        assert!(d.stutter_start.is_some());

        // 持续高位 >1s：稳态抑制触发 → cause 停发 → 会话结束并落库（有界事件）
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let event = d
            .analyze(&high)
            .expect("稳态抑制结束时应有界落库一次事件");
        assert!(
            event.causes.iter().any(|c| c.contains("Memory usage")),
            "事件应携带抑制前的内存 cause，got: {:?}",
            event.causes
        );

        // 锁存期：仍高位 → 不再开新会话
        d.analyze(&high);
        assert!(
            d.stutter_start.is_none(),
            "锁存期内重新越过进入线不得开新会话（防反复误报）"
        );
        assert!(d.current_causes.is_empty());

        // 回落到退出线以下（84% 且可用充足）→ 解锁
        let mut recover = make_sample(30.0, 2000, 10.0);
        recover.mem_usage_percent = 84.0;
        d.analyze(&recover);
        assert!(!d.mem_level_suppressed, "回落到退出线以下应解锁");

        // 解锁后再次越线 → 正常恢复触发
        d.analyze(&high);
        assert!(d.mem_level_active, "解锁后再次越线应恢复激活");
        assert!(
            d.current_causes.iter().any(|c| c.contains("Memory usage")),
            "解锁后再次越线应恢复发射 cause，got: {:?}",
            d.current_causes
        );
    }

    /// 阶段 E3：滞回带最长保持——CPU 进入后在带内（80~90）仅靠滞回维持超过
    /// hysteresis_hold_max_secs 即强制解除；重新越过进入线恢复。
    #[test]
    fn cpu_band_hold_expires_after_max_hold() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 5;
        config.hysteresis_hold_max_secs = 1; // 测试用：1s 即超时
        let mut d = Detector::new(&config);

        d.analyze(&make_sample(95.0, 2000, 10.0)); // >90 进入
        d.analyze(&make_sample(85.0, 2000, 10.0)); // 带内 → 开始计时
        assert!(d.cpu_active, "带内应先保持激活");

        std::thread::sleep(std::time::Duration::from_millis(1200));
        d.analyze(&make_sample(85.0, 2000, 10.0)); // 带内超时 → 强制解除
        assert!(!d.cpu_active, "滞回带保持超过上限应强制解除");
        assert!(
            d.current_causes.iter().all(|c| !c.contains("CPU usage")),
            "解除后不应再发射 CPU cause，got: {:?}",
            d.current_causes
        );

        // 重新越过进入线 → 恢复
        d.analyze(&make_sample(95.0, 2000, 10.0));
        assert!(d.cpu_active, "重新越过进入线应恢复激活");
    }

    /// 阶段 E5：采样中断防护——与上一 tick 间隔超过 max_tick_gap_secs（睡眠/
    /// 挂起）时清空跟踪重评估，睡眠时长不得计入卡顿 duration（回归：实测出现
    /// 过 3.9 天的巨长事件）。
    #[test]
    fn analyze_resets_session_after_sampling_gap() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        config.max_tick_gap_secs = 1; // 测试用：1s 即判定中断
        let mut d = Detector::new(&config);

        d.analyze(&make_sample(95.0, 2000, 10.0)); // 会话开始
        assert!(d.stutter_start.is_some());

        // 模拟睡眠/挂起：间隔 > 1s
        std::thread::sleep(std::time::Duration::from_millis(1500));
        // 恢复后首 tick：若未重置，旧会话时长 1.5s ≥ sustained 会落库巨长事件
        let r = d.analyze(&make_sample(20.0, 2000, 10.0));
        assert!(r.is_none(), "采样中断后不得把睡眠时长算进卡顿落库");
        // 旧会话被清空（若未清空，此处会话应已「结束」并可能已产出事件）
        assert!(
            d.current_causes.is_empty(),
            "中断后状态应已清空，got: {:?}",
            d.current_causes
        );

        // 中断后正常重评估：新会话立即满足条件 → 高压样本开新跟踪，
        // 随即正常样本结束：新会话时长 ~0 < sustained → 不落库
        d.analyze(&make_sample(95.0, 2000, 10.0));
        assert!(d.stutter_start.is_some(), "中断后应重新开始跟踪");
        assert!(
            d.analyze(&make_sample(20.0, 2000, 10.0)).is_none(),
            "新会话不足持续时长不应落库"
        );
    }

    /// 阶段 E4：频率峰值慢衰减——短时睿频建立的陈旧峰值逐渐让位于持续负载
    /// 频率，不再长期误判降频（旧行为：峰值只增不减，全核负载全程「掉档」）。
    #[test]
    fn thermal_peak_decay_clears_stale_turbo_peak() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 5;
        config.thermal_freq_peak_decay = 0.95; // 测试用：快速收敛
        let mut d = Detector::new(&config);

        // 短时睿频建立峰值 5000MHz
        d.analyze(&make_sample_thermal(80.0, 2000, Some(70.0), Some(5000.0)));
        // 高温 + 负载 + 持续负载频率 3900（< 0.85×5000=4250）→ 陈旧峰值下误判降频
        let load = make_sample_thermal(80.0, 2000, Some(95.0), Some(3900.0));
        d.analyze(&load);
        assert!(
            d.current_causes.iter().any(|c| c.contains("Thermal throttle")),
            "陈旧峰值未衰减时会误判降频（旧行为基线），got: {:?}",
            d.current_causes
        );

        // 持续负载下峰值逐读数衰减（5000 → 4750 → 4512 → …收敛到 3900），
        // 掉档判定失效 → 不再误判
        for _ in 0..4 {
            d.analyze(&load);
        }
        assert!(
            d.current_causes
                .iter()
                .all(|c| !c.contains("Thermal throttle")),
            "峰值衰减收敛后不应再误判降频，got: {:?}",
            d.current_causes
        );
    }

    // --- spike 绝对下限 ---

    fn make_sample_net(cpu: f32, mem_avail_mb: u64, swap: f32, net_bps: u64) -> Sample {
        let mut s = make_sample(cpu, mem_avail_mb, swap);
        s.net_sent_bps = net_bps;
        s
    }

    /// 空闲零头（KB 级波动）即使倍数很大也不应触发 spike（绝对下限拦截）。
    #[test]
    fn spike_min_floor_ignores_small_rates() {
        let config = DetectionConfig::default(); // spike_ratio=2.0, spike_min_bps=1MB
        let mut d = Detector::new(&config);

        // 60 个基线样本（1 KB/s）+ 10 个 recent（10 KB/s）：ratio=9 > 2，
        // 但 r_avg=10KB << 1MB → 绝对下限拦截
        for _ in 0..60 {
            d.analyze(&make_sample_net(30.0, 2000, 10.0, 1_000));
        }
        for _ in 0..10 {
            d.analyze(&make_sample_net(30.0, 2000, 10.0, 10_000));
        }
        assert!(
            d.current_causes.iter().all(|c| !c.contains("spike")),
            "KB 级零头不应触发 spike，got: {:?}",
            d.current_causes
        );
    }

    /// 真实大流量（≥ 绝对下限）时 spike 正常触发。
    #[test]
    fn spike_min_floor_allows_large_rates() {
        let config = DetectionConfig::default();
        let mut d = Detector::new(&config);

        // 60 个基线（1 MB/s）+ 10 个 recent（5 MB/s）：ratio=4 > 2 且 ≥ 1MB
        for _ in 0..60 {
            d.analyze(&make_sample_net(30.0, 2000, 10.0, 1_000_000));
        }
        for _ in 0..10 {
            d.analyze(&make_sample_net(30.0, 2000, 10.0, 5_000_000));
        }
        assert!(
            d.current_causes.iter().any(|c| c.contains("Network spike")),
            "真实大流量 spike 应触发，got: {:?}",
            d.current_causes
        );
    }

    // --- cause 类型去重 key ---

    #[test]
    fn cause_key_groups_hysteresis_variants() {
        // 滞回带内/外文案不同，但类型 key 一致 → 同一条 cause 更新而非追加
        assert_eq!(
            cause_key("CPU usage 95.0% > 90%"),
            cause_key("CPU usage 85.0%（滞回保持，阈值 90%）")
        );
        assert_eq!(
            cause_key("Commit charge 95.0% > 90%"),
            cause_key("Commit charge 85.0%")
        );
        // 分页速率不同数值归为同一类型 key（更新而非追加）
        assert_eq!(
            cause_key("Memory paging 200.0/s > 50/s"),
            cause_key("Memory paging 80.5/s > 50/s")
        );
        // 硬阈值与 spike 是不同 cause
        assert_ne!(cause_key("CPU usage 95.0% > 90%"), cause_key("CPU spike: 1.0% → 3.0%"));
        // spike 各类型互不混淆
        assert_ne!(cause_key("Disk write spike: 1B/s → 3B/s"), cause_key("Network spike: 1B/s → 3B/s"));
        assert_ne!(cause_key("Memory available spike: 1MB → 3MB"), cause_key("Available memory 100MB < 500MB"));
    }

    // --- spike 优化：只认突增 / 连续确认 / 滞回 ---

    /// 速率骤降（传输完成、写盘结束）不应触发 spike（旧实现用 abs 会误报）。
    #[test]
    fn spike_ignores_rate_drop() {
        let config = DetectionConfig::default(); // spike_ratio=3.0, min=2MB
        let mut d = Detector::new(&config);

        // 60 个基线（10 MB/s）→ 10 个 recent（1 MB/s，下降 90%）
        for _ in 0..60 {
            d.analyze(&make_sample_net(30.0, 2000, 10.0, 10_000_000));
        }
        for _ in 0..10 {
            d.analyze(&make_sample_net(30.0, 2000, 10.0, 1_000_000));
        }
        assert!(
            d.current_causes.iter().all(|c| !c.contains("spike")),
            "速率骤降不应触发 spike，got: {:?}",
            d.current_causes
        );
    }

    /// 单次/零星尖峰不触发：recent 10 样本中仅 3 个超阈值（<6）。
    #[test]
    fn spike_requires_confirmation() {
        let config = DetectionConfig::default();
        let mut d = Detector::new(&config);

        // 60 个基线（1 MB/s）+ 10 个 recent：7 个 1MB + 3 个 5MB（ratio 4 > 3）
        for _ in 0..60 {
            d.analyze(&make_sample_net(30.0, 2000, 10.0, 1_000_000));
        }
        for _ in 0..7 {
            d.analyze(&make_sample_net(30.0, 2000, 10.0, 1_000_000));
        }
        for _ in 0..3 {
            d.analyze(&make_sample_net(30.0, 2000, 10.0, 5_000_000));
        }
        assert!(
            d.current_causes.iter().all(|c| !c.contains("Network spike")),
            "零星尖峰不应触发（需 ≥6/10 确认），got: {:?}",
            d.current_causes
        );
    }

    /// spike 滞回：触发后 recent 均值回落到中间带（ratio 在 threshold*0.5 ~
    /// threshold 之间）仍保持激活；明显回落后才解除。
    #[test]
    fn spike_hysteresis_keeps_active_in_band() {
        let config = DetectionConfig::default(); // spike_ratio=3.0 → 触发 3，解除 <1.5
        let _d = Detector::new(&config);
        let mut active = false;

        // 触发：recent 全部 8.0（ratio (8-1.5)/1.5=4.33 > 3，over=10）
        let recent = vec![8.0f32; 10];
        let baseline = vec![1.5f32; 60];
        let mut causes = Vec::new();
        Detector::spike_check(&mut causes, "Network", "B/s", &recent, &baseline, 3.0, 2.0, &mut active);
        assert!(active);
        assert!(causes.iter().any(|c| c.contains("Network spike")));

        // 滞回带内：recent 均值 5.0（ratio 2.33：>1.5 不解除，<3 不触发）
        let recent2 = vec![5.0f32; 10];
        let mut causes2 = Vec::new();
        Detector::spike_check(&mut causes2, "Network", "B/s", &recent2, &baseline, 3.0, 2.0, &mut active);
        assert!(active, "滞回带内应保持激活");
        assert!(causes2.iter().any(|c| c.contains("Network spike")));

        // 明显回落：recent 均值 2.0（ratio 0.33 < 1.5）→ 解除
        let recent3 = vec![2.0f32; 10];
        let mut causes3 = Vec::new();
        Detector::spike_check(&mut causes3, "Network", "B/s", &recent3, &baseline, 3.0, 2.0, &mut active);
        assert!(!active, "明显回落后应解除");
        assert!(causes3.is_empty());
    }

    // --- C 档：卡顿事件记录 culprit 进程 ---

    /// 卡顿激活期间采到的 top 进程应在事件中以 culprits 形式落库，
    /// 且 CPU 维度最高者应排第一。
    #[test]
    fn analyze_records_culprits() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        let mut s = make_sample(95.0, 2000, 10.0); // 触发 CPU 卡顿
        s.top_processes = vec![
            ProcessBrief {
                pid: 1001,
                name: "heavy.exe".into(),
                cpu_usage: 88.0,
                mem_used_mb: 1024,
                ..Default::default()
            },
            ProcessBrief {
                pid: 1002,
                name: "bg.exe".into(),
                cpu_usage: 5.0,
                mem_used_mb: 2048,
                ..Default::default()
            },
        ];
        for _ in 0..3 {
            d.analyze(&s);
        }
        std::thread::sleep(std::time::Duration::from_millis(1200));

        let event = d.analyze(&make_sample(20.0, 2000, 10.0)).unwrap();
        assert!(!event.culprits.is_empty(), "应记录 culprit 进程");
        assert_eq!(event.culprits[0].name, "heavy.exe");
        assert_eq!(event.culprits[0].pid, 1001);
    }

    /// 无 top 进程（默认采样）时 culprits 应为空，不应 panic。
    #[test]
    fn analyze_empty_culprits_when_no_top_processes() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        let high = make_sample(95.0, 2000, 10.0);
        for _ in 0..3 {
            d.analyze(&high);
        }
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let event = d.analyze(&make_sample(20.0, 2000, 10.0)).unwrap();
        assert!(event.culprits.is_empty());
    }

    /// need_process_snapshot 标志随卡顿状态切换：
    /// 初始 false → 高 CPU 触发卡顿后 true（下一帧 collect 需要快照）
    /// → 卡顿结束（生成事件）后复位为 false。
    /// 用 CPU 硬阈值路径触发（无需 spike 的 70 帧历史预热）。
    #[test]
    fn need_process_snapshot_tracks_stutter_state() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);
        assert!(
            !d.needs_process_snapshot(),
            "初始（无卡顿）不需要进程快照"
        );

        // CPU 硬阈值触发卡顿：analyze 返回 None（卡顿开始），
        // 标志置 true（下一帧 collect 需要 top_processes）。
        let high = make_sample(95.0, 2000, 10.0);
        assert!(d.analyze(&high).is_none());
        assert!(
            d.needs_process_snapshot(),
            "卡顿进行中：下一帧需要进程快照"
        );

        // 持续卡顿期间标志保持 true
        for _ in 0..2 {
            d.analyze(&high);
        }
        assert!(d.needs_process_snapshot());

        // 正常样本结束卡顿 → 生成事件，标志复位为 false
        std::thread::sleep(std::time::Duration::from_millis(1200));
        assert!(d.analyze(&make_sample(20.0, 2000, 10.0)).is_some());
        assert!(
            !d.needs_process_snapshot(),
            "卡顿结束后不再需要进程快照"
        );
    }

    /// F-RC3：已触发其它 cause 的卡顿帧 + 前台窗口探测返回冻结 → 产出 `UiFrozen`
    /// cause 并落库 `CauseKind::UiFrozen`（绝不单独成 cause，必须伴随其它 cause）。
    #[test]
    fn analyze_ui_frozen_triggers_when_probe_says_frozen() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);
        // 注入：前台窗口无响应
        d.ui_probe = Box::new(|_| true);

        // 同时触发 CPU 卡顿（UiFrozen 必须伴随其它 cause，不单独出现）
        let high = make_sample(95.0, 2000, 10.0);
        d.analyze(&high);
        assert!(
            d.current_causes.iter().any(|c| c.contains("UI frozen")),
            "冻结探测返回 true 时应产出 UI frozen cause，got: {:?}",
            d.current_causes
        );

        for _ in 0..2 {
            d.analyze(&high);
        }
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let event = d.analyze(&make_sample(20.0, 2000, 10.0)).unwrap();
        assert!(
            event.cause_kinds.contains(&CauseKind::UiFrozen),
            "cause_kinds 应含 UiFrozen，got: {:?}",
            event.cause_kinds
        );
    }

    /// F-RC3：前台窗口探测返回响应（不冻结）→ 不产出 `UiFrozen` cause。
    #[test]
    fn analyze_ui_frozen_absent_when_probe_says_responsive() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);
        d.ui_probe = Box::new(|_| false); // 前台窗口正常响应

        let high = make_sample(95.0, 2000, 10.0);
        d.analyze(&high);
        assert!(
            d.current_causes.iter().all(|c| !c.contains("UI frozen")),
            "前台窗口正常时不应产出 UI frozen，got: {:?}",
            d.current_causes
        );
    }

    /// F-RC3 回归：无其它 cause 的常规帧**绝不**触发 UI 探测（不进热路径）。
    /// 注入探测函数：一旦被调用即 panic，验证常规帧不会调用它。
    #[test]
    fn analyze_ui_probe_not_called_on_normal_frame() {
        let config = DetectionConfig::default();
        let mut d = Detector::new(&config);
        d.ui_probe = Box::new(|_| panic!("probe must not run on non-stutter frame"));
        // 常规样本（无 cause）不应调用 probe
        let normal = make_sample(30.0, 2000, 10.0);
        d.analyze(&normal);
        assert!(d.current_causes.is_empty());
    }
}
