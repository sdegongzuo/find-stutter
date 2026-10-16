#[cfg(not(test))]
use crate::collector::probe_foreground_window_frozen;
use crate::types::{cause_key, CauseKind, DetectionConfig, ProcessBrief, Sample, Severity, StutterEvent};
use chrono::Utc;
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// F-RC4：降频判定所需的「负载下限」（CPU 使用率 %）。
/// 降频仅在负载下才有归因意义——空闲时频率本就低（节能降频），不能算卡顿根因。
/// 取 50% 是经验值：低于此不视为「负载下频率不升反降」。
const THERMAL_LOAD_MIN_USAGE: f32 = 50.0;

/// 上下文切换风暴的连续确认参数（2026-08-30 治理）：最近 `CTX_CONFIRM_WINDOW`
/// 帧中 ≥`CTX_CONFIRM_MIN` 帧越线才算「风暴在场」，对齐 `spike_check` 的
/// 连续确认模式。机器日常基线（6~9k/core）本就骑在滞回退出线（×0.5 = 5k/core）
/// 之上，一次尖峰激活后滞回几乎不可自然退出、尖峰反复重置带保持计时即可锁死
/// 数分钟（实测凌晨连环 major/critical 误报）；占比确认让间歇尖峰既无法首次
/// 激活，也无法重置带保持计时续命。
const CTX_CONFIRM_WINDOW: usize = 10;
const CTX_CONFIRM_MIN: usize = 6;

/// cause 文案是否为「滞回保持」标记：数值已回落到进入线以下、仅靠滞回维持会话的
/// 带内帧。此类帧不构成真实越线证据（不计确认累计、不覆盖真实越线文案）。
fn is_hysteresis_hold(cause: &str) -> bool {
    cause.contains("滞回保持")
}

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
    /// 上下文切换「越线帧」滑动窗口（最近 `CTX_CONFIRM_WINDOW` 帧）：占比确认用，
    /// 防间歇尖峰激活/续命风暴（见 `CTX_CONFIRM_WINDOW` 常量注释）。
    ctx_over_recent: Vec<bool>,
    /// 当前会话内「压力类 cause 真实越线（非滞回保持）」的累计毫秒。
    /// severity 的时长判级只认这个值——墙钟 duration 会被滞回带「保持」帧拖长，
    /// 把间歇尖峰撑成的长会话虚增为 Major/Critical（2026-08-30 治理）。
    session_confirmed_ms: u64,
    /// 卡顿持续期间累积的进程快照（pid -> 取最大 CPU / 内存用量），
    /// 卡顿结束时提取 top 作为 culprits。
    current_culprits: HashMap<u32, ProcessBrief>,
    /// 各 cause（按 `CauseKind`）首次出现的时刻（SystemTime），用于落库首触时刻
    /// 与 F-RC6 因果方向（触发者 vs 放大器）。随 `current_causes` 一起在卡顿结束时清空。
    current_cause_first_touch: HashMap<CauseKind, SystemTime>,
    /// 各 cause 类型（按 `cause_key`）最近一次「真实越线」文案（非「滞回保持」）。
    /// 落库时若 `current_causes` 中同类型只剩带内「滞回保持」文案，则优先取此处的
    /// 真实触发证据，避免事件回读自相矛盾（2026-10-16 治理）。随会话一起清空。
    current_cause_real_text: HashMap<String, String>,
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
            ui_probe: Self::default_ui_probe(),
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
            ctx_over_recent: Vec::new(),
            session_confirmed_ms: 0,
            current_culprits: HashMap::new(),
            current_cause_first_touch: HashMap::new(),
            current_cause_real_text: HashMap::new(),
            need_process_snapshot: false,
            freq_peak: None,
        }
    }

    /// 下一帧 collect 是否需要构建 top_processes（卡顿进行中或刚结束一帧）
    pub fn needs_process_snapshot(&self) -> bool {
        self.need_process_snapshot
    }

    /// F-RC3 前台窗口冻结探测的默认实现（依赖注入点，见 `ui_probe` 字段）。
    ///
    /// 生产走真实 Win32 探测；单测环境默认注入「未冻结」——前台窗口是否响应
    /// 取决于运行测试时的桌面会话（agent / CI 常无交互会话，或前台窗口不泵
    /// WM_NULL），真实探测返回 true 会凭空多出一条 `UI frozen` cause，令
    /// `current_causes` 计数与 severity 用例随环境漂移（本机实测 7 例）。需要
    /// 验证冻结路径的用例自行覆盖 `ui_probe`，不受本默认值影响。
    #[cfg(not(test))]
    fn default_ui_probe() -> Box<dyn Fn(u32) -> bool + Send> {
        Box::new(probe_foreground_window_frozen)
    }

    #[cfg(test)]
    fn default_ui_probe() -> Box<dyn Fn(u32) -> bool + Send> {
        Box::new(|_| false)
    }

    pub fn analyze(&mut self, sample: &Sample) -> Option<StutterEvent> {
        // ===== 阶段 E：采样中断防护 =====
        // 与上一 tick 的墙钟间隔超过 `max_tick_gap_secs` 判定为采样中断
        // （系统睡眠/挂起/采集卡死）。所有跨 tick 状态（卡顿跟踪、滞回、
        // 滑动基线）全部失效：清空重评估、绝不落库——否则睡眠时长会被
        // `start.elapsed()` 算进卡顿 duration（实测出现过 3.9 天的巨长事件）。
        // 与 duration 同用 SystemTime 墙钟；时钟回拨（NTP）不算中断。
        let tick_now = SystemTime::now();
        // 本帧与上一帧的实际间隔（severity 确认时长累计用）：睡眠中断时归零，
        // 不得把睡眠时长算进确认累计。
        let mut frame_dt = Duration::ZERO;
        if let Some(last) = self.last_tick {
            match tick_now.duration_since(last) {
                Ok(gap)
                    if gap > Duration::from_secs(self.config.max_tick_gap_secs as u64) =>
                {
                    self.reset_after_gap();
                }
                Ok(gap) => frame_dt = gap,
                // 时钟回拨（NTP）：不计入确认累计
                Err(_) => {}
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
            // 「真实越线」判定（2026-08-30 治理）：压力类 cause 且文案非「滞回保持」。
            // 「滞回保持」是 §4 明文约定的带内标记——severity 时长判级只认真饱和
            // 累计，滞回带续命帧不算（DiskBusy 带内维持暂无带内文案标注，会按
            // 真实越线计，属可接受的高估方向）。
            let frame_confirmed = causes.iter().any(|c| {
                !is_hysteresis_hold(c)
                    && matches!(CauseKind::from_cause(c), Some(k) if k.is_pressure())
            });
            if self.stutter_start.is_none() {
                self.session_confirmed_ms = 0;
            }
            if frame_confirmed {
                self.session_confirmed_ms += frame_dt.as_millis() as u64;
            }
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
                self.current_cause_real_text.clear();
                // 记录每个 cause 的首触时刻（按 CauseKind 去重）与真实越线文案
                for c in &self.current_causes {
                    if let Some(k) = CauseKind::from_cause(c) {
                        self.current_cause_first_touch.entry(k).or_insert(now);
                    }
                    if !is_hysteresis_hold(c) {
                        self.current_cause_real_text
                            .insert(cause_key(c).to_string(), c.clone());
                    }
                }
            } else {
                for c in causes {
                    // 按 cause 类型去重：同类型（如 Commit charge / CPU usage）更新为最新文案，
                    // 避免滞回带内文案随数值变化导致字符串去重失效、cause 反复追加
                    // （一次卡顿中 current_causes 膨胀，还会虚高 severity）。
                    let key = cause_key(&c);
                    // 记录该 cause 类型的「真实越线」文案（非滞回保持），供落库时优先
                    // 展示真实触发证据（2026-10-16 治理）——否则事件落库后可能只剩
                    // 「…（滞回保持，阈值 90%）」，与「因越线而触发」自相矛盾。活跃视图
                    // `current_causes` 的口径不变（仍更新为最新文案，维持会话存活判定）。
                    if !is_hysteresis_hold(&c) {
                        self.current_cause_real_text.insert(key.to_string(), c.clone());
                    }
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

            // 落库闸门（2026-10-16 治理）：改用「真实越线累计」而非墙钟时长。
            // 墙钟 duration 会被滞回带保持帧拖长——瞬时尖峰（1~2 帧越线）+ 十余秒
            // 带内保持即凑出一条跨 sustained 的流水事件（实测一台常年骑在 88~90%
            // 边缘的机器 5 分钟内碎出 6 条 minor 内存事件，文案全是「滞回保持」）。
            // session_confirmed_ms 只累计压力类 cause 的真实越线帧（带内「滞回保持」
            // 帧不计），因此只有真实饱和持续 ≥ sustained_seconds 才落库。
            if self.session_confirmed_ms >= self.config.sustained_seconds as u64 * 1000 {
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
                    self.current_cause_real_text.clear();
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
                // 结构化根因（对齐 CauseKind）。先按 kind 去重再按首触稳定排序：
                // 同 kind 的多条文案在不同 tick 先后触达时（如「Memory usage…」@t0、
                // 「Available memory…」@t2），到达序使重复项不相邻，先 dedup() 后排序
                // 会留下 [MemLow, MemLow, …] 式重复（2026-08-24 落库数据实际出现）。
                let mut cause_kinds: Vec<CauseKind> = Vec::new();
                for c in &self.current_causes {
                    if let Some(k) = CauseKind::from_cause(c) {
                        if !cause_kinds.contains(&k) {
                            cause_kinds.push(k);
                        }
                    }
                }
                // 按首触时刻升序（最早触发者在前）：F-RC1 主因取首触最早者，
                // 同时让 cause_kinds 顺序即「触发者→放大器」走向，供 F-RC9 因果链使用；
                // F-RC5 将在此基础上按信号强度×持续细化加权。
                cause_kinds.sort_by_key(|k| cause_first_touch.get(k).copied().unwrap_or(0));
                let primary_cause = cause_kinds.first().copied();
                let onset_ts = start
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                // 落库文案（2026-10-16 治理）：同类型若当前只剩带内「滞回保持」文案，
                // 优先回填本轮记录的真实越线文案，保证事件自带「为何触发」的证据；
                // 活跃视图 `current_causes` 不动（severity 判级口径不变）。
                let stored_causes: Vec<String> = self
                    .current_causes
                    .iter()
                    .map(|c| {
                        if is_hysteresis_hold(c) {
                            self.current_cause_real_text
                                .get(cause_key(c))
                                .cloned()
                                .unwrap_or_else(|| c.clone())
                        } else {
                            c.clone()
                        }
                    })
                    .collect();
                let event = StutterEvent {
                    id: 0,
                    timestamp: Utc::now(),
                    duration_ms,
                    // severity 时长判级用「确认越线累计」而非墙钟 duration：
                    // 滞回带保持帧拖长的会话不得凭墙钟时长虚增级别（§6）
                    severity: Self::determine_severity(
                        &self.current_causes,
                        self.session_confirmed_ms,
                    ),
                    causes: stored_causes,
                    cause_kinds,
                    primary_cause,
                    cause_first_touch,
                    onset_ts: Some(onset_ts),
                    snapshot: sample.clone(),
                    culprits,
                };
                self.current_causes.clear();
                self.current_cause_first_touch.clear();
                self.current_cause_real_text.clear();
                self.session_confirmed_ms = 0;
                // 卡顿已结束：下一帧 collect 不再需要进程快照。
                // 时序权衡见上方 causes 分支：analyze 设置的标志影响下一帧。
                self.need_process_snapshot = false;
                return Some(event);
            }
            self.current_causes.clear();
            self.current_culprits.clear();
            self.current_cause_first_touch.clear();
            self.current_cause_real_text.clear();
            self.session_confirmed_ms = 0;
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
        self.current_cause_real_text.clear();
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
        self.ctx_over_recent.clear();
        self.session_confirmed_ms = 0;
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
        // 分页（paging）共用本锁存（2026-08-30 治理，见下方分页判定处注释）：
        // 水位+分页同属内存侧稳态，锁存后一并停发，会话有界。
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
            // 带内帧（低于进入线、未到退出线）必须发射「滞回保持」文案维持会话，
            // 对齐 CPU / DPC / 中断 / 磁盘 / 上下文切换信号的语义。旧实现在带内帧
            // 不发射任何 cause → 任一回落帧都令 causes 为空、立即关闭会话，水位在
            // 进入线附近每抖一次就碎出一条新事件（2026-08-24 20:55 三连事件的根源，
            // 回归测试 analyze_mem_oscillation_yields_single_session）。
            let avail_exit_line =
                self.config.mem_threshold_mb as f32 * self.config.mem_available_exit_ratio;
            if avail_under_entry {
                causes.push(format!(
                    "Available memory {}MB < {}MB",
                    sample.mem_available_mb, self.config.mem_threshold_mb
                ));
            } else if (sample.mem_available_mb as f32) < avail_exit_line {
                // 仅靠水位滞回维持激活：数值已回落但未到退出线（进入 500 / 退出 600）。
                causes.push(format!(
                    "Available memory {}MB（滞回保持，退出线 {}MB）",
                    sample.mem_available_mb, avail_exit_line as u64
                ));
            }
            // 内存使用率过高（百分比口径）：与 `mem_threshold_mb`（绝对可用下限）
            // 互补，覆盖「大内存机器上可用内存绝对值仍高、但使用率已爆表」的漏报
            // （例如 32G 机器用到 95% 时可用仍 >500MB，仅看绝对下限会漏报）。
            let usage_exit_line =
                self.config.mem_threshold_percent - self.config.mem_hysteresis_percent;
            if usage_over_entry {
                causes.push(format!(
                    "Memory usage {:.1}% > {}%",
                    sample.mem_usage_percent, self.config.mem_threshold_percent
                ));
            } else if sample.mem_usage_percent > usage_exit_line {
                causes.push(format!(
                    "Memory usage {:.1}%（滞回保持，阈值 {}%）",
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
        // 系统并不卡顿（用户无感）。故 paging 不再单指标硬触发，必须同时存在「内存侧压力
        // 证据」（commit 高 / 内存使用率高 / 可用内存不足）才记为卡顿 cause——
        // paging 退化为真卡顿的「放大器」，避免孤立分页尖峰连环误报。
        // 2026-08-29 治理：压力证据不再包含「磁盘繁忙」——分页读盘会把磁盘打忙、
        // 磁盘忙再给分页供证（循环论证），大文件冷读（Page Reads 计入预读）即可
        // 凭「分页高 + 磁盘忙」误报成 MemLow；详见 DetectionConfig 方法注释。
        let paging_has_pressure_evidence =
            self.config.paging_has_pressure_evidence(commit_ratio, sample);
        // 2026-08-30 治理：分页与水位共用稳态抑制锁存。内存高压机器上分页
        // 2000~4000/s 可常态越线（水位 90%+ 连续活跃时几乎全程在场），旧口径
        // 「分页不受抑制影响」让分页给整个会话无限续命——实测 121s 的事件里
        // 94/96 帧分页越线、稳态抑制锁存了水位却锁不住会话。内存侧（水位+分页）
        // 连续活跃超 mem_chronic_seconds 即整体判为稳态：本次越线只产生一条
        // 有界事件；真颠簸通常在最初 sustained 窗口内已落库，不受影响。
        if sample.page_reads_per_sec > self.config.page_reads_threshold
            && paging_has_pressure_evidence
            && !self.mem_level_suppressed
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

        // 磁盘真繁忙度：IO 延迟单独越线（饱和金标准），或 % Disk Time 越线且
        // IO 延迟同时超过确认线。替代原磁盘 B/s spike——吞吐高不代表磁盘饱和，
        // 延迟才是排队证据；busy% 在 SSD/NVMe 上并行读写即可打满 100% 而无排队，
        // 须由延迟确认（2026-08-29 治理）。共用 DetectionConfig::disk_busy_saturated，
        // 与 what-if 单帧重算（analytics.rs）不漂移。
        let disk_busy = self.config.disk_busy_saturated(sample);
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
            if sample.dpc_percent > self.config.dpc_threshold_percent {
                causes.push(format!(
                    "DPC time {:.1}% > {}%",
                    sample.dpc_percent, self.config.dpc_threshold_percent
                ));
            } else {
                // 滞回带内维持激活：数值已回落但未到退出线，
                // 文案标注「滞回保持」，避免出现 "8% > 10%" 的矛盾文案（标准 §4）
                causes.push(format!(
                    "DPC time {:.1}%（滞回保持，阈值 {}%）",
                    sample.dpc_percent, self.config.dpc_threshold_percent
                ));
            }
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
            if sample.interrupt_percent > self.config.interrupt_threshold_percent {
                causes.push(format!(
                    "Interrupt time {:.1}% > {}%",
                    sample.interrupt_percent, self.config.interrupt_threshold_percent
                ));
            } else {
                // 滞回带内维持激活：数值已回落但未到退出线，
                // 文案标注「滞回保持」，避免出现 "8% > 10%" 的矛盾文案（标准 §4）
                causes.push(format!(
                    "Interrupt time {:.1}%（滞回保持，阈值 {}%）",
                    sample.interrupt_percent, self.config.interrupt_threshold_percent
                ));
            }
        }

        // Context Switches/sec：上下文切换风暴会拖垮调度（系统级卡顿真信号）。
        // 机器相对口径（2026-08-17 误报治理）：原绝对阈值 50000/s 在多核开发机上
        // 骑在日常基线上（本机 14 核日常 ~45k/s，33% 样本越线；×0.5 退出线
        // 25000/s 低于机器地板，连续低于它的最长段仅 2s）——一次越线即滞回
        // 锁死数分钟，凌晨无人在场连环 major/critical 误报。改为两点：
        // 1) 按逻辑核数归一（/s ÷ 核数 > context_switch_threshold_per_core），
        //    核数取样本 cpu_per_core 长度（随机器自校准），缺失退回
        //    available_parallelism（共用 types::logical_core_count）；
        // 2) 首次激活需 CPU 侧压力证据（真风暴必伴随 CPU 饱和或 DPC/中断活跃；
        //    低 CPU 的高切换只是后台异步活动）。已激活后的滞回退出不受证据影响。
        // 归一分母与 what-if 共用 types::logical_core_count，两处口径不漂移。
        let ctx_cores = crate::types::logical_core_count(sample) as f32;
        let ctx_per_core = sample.context_switches_per_sec / ctx_cores;
        let ctx_over_entry = ctx_per_core > self.config.context_switch_threshold_per_core;
        // 连续确认（2026-08-30 治理，见 CTX_CONFIRM_WINDOW 注释）：占比确认独立于
        // 单帧越线——间歇尖峰（如每 5s 一帧）在窗口内占比远低于 6/10，既无法首次
        // 激活，也无法在下方的带保持计时中重置续命。
        self.ctx_over_recent.push(ctx_over_entry);
        if self.ctx_over_recent.len() > CTX_CONFIRM_WINDOW {
            self.ctx_over_recent.remove(0);
        }
        let ctx_confirmed =
            self.ctx_over_recent.iter().filter(|&&b| b).count() >= CTX_CONFIRM_MIN;
        let ctx_want_active = sys_signal_hysteresis(
            self.ctx_switch_active,
            ctx_per_core,
            self.config.context_switch_threshold_per_core,
            self.config.sys_signal_hysteresis_ratio,
        );
        // 未激活 → 激活的跳变必须同时有确认 + 压力证据；保持/退出交给滞回
        self.ctx_switch_active = ctx_want_active
            && (self.ctx_switch_active
                || (ctx_over_entry
                    && ctx_confirmed
                    && self.config.ctx_switch_has_pressure_evidence(sample)));
        // 带保持计时的重置条件用占比确认而非单帧越线：滞回是防抖工具，
        // 间歇尖峰不得反复重置计时把风暴锁死在滞回带内（2026-08-30 治理）。
        band_hold_step(
            &mut self.ctx_switch_active,
            &mut self.ctx_band_since,
            ctx_confirmed,
            self.config.hysteresis_hold_max_secs,
        );
        if self.ctx_switch_active {
            if ctx_over_entry {
                causes.push(format!(
                    "Context switches {:.0}/s = {:.0}/core > {:.0}/core",
                    sample.context_switches_per_sec,
                    ctx_per_core,
                    self.config.context_switch_threshold_per_core
                ));
            } else {
                // 滞回带内维持激活：数值已回落但未到退出线，
                // 文案标注「滞回保持」，避免矛盾文案（标准 §4）
                causes.push(format!(
                    "Context switches {:.0}/s（滞回保持，{:.0}/core，阈值 {:.0}/core）",
                    sample.context_switches_per_sec,
                    ctx_per_core,
                    self.config.context_switch_threshold_per_core
                ));
            }
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
        // Avg. Disk sec/Transfer）判定 `DiskBusy` cause（见 `check_hard_thresholds`），
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

    /// severity 判定（2026-08-30 治理：时长判级改用 `confirmed_ms`）。
    ///
    /// - cause 计数维度不变：只计压力类、按 CauseKind 去重（见下）。
    /// - 时长维度改用**确认越线累计**（会话内压力类 cause 真实越线、非「滞回
    ///   保持」帧的累计毫秒），不再用墙钟 duration：滞回带「保持」帧会把间歇
    ///   尖峰撑成的短会话拖长到 >10s/>30s，凭墙钟判级即虚增为 Major/Critical
    ///   （2026-08-30 实测：凌晨 6 条 ContextSwitchStorm 滞回锁死事件全部因此
    ///   升级弹窗）。落库的 `duration_ms` 仍保留墙钟值，口径不变。
    fn determine_severity(causes: &[String], confirmed_ms: u64) -> Severity {
        // 阶段 E：只计压力类 cause——非压力类（NetSpike 等纯吞吐信号）不虚增
        // 严重度（否则 CpuHigh + 网络 spike 凑 2 因即 Major）。无法映射到
        // CauseKind 的文本按压力类计（防御：宁可虚高也不漏）。
        // 计数按 CauseKind 去重：同一根因的多条文案（「Memory usage…」
        // 「Memory paging…」「Available memory…」同为 MemLow）只计 1 因，
        // 否则 90% 边缘抖动的两帧样本即可凑满 2 因被虚增为 Major
        // （2026-08-24 误报分析；回归测试 determine_severity_same_kind_not_double_counted）。
        let mut seen_kinds: Vec<CauseKind> = Vec::new();
        let count = causes
            .iter()
            .filter(|c| match CauseKind::from_cause(c) {
                Some(k) => {
                    if !k.is_pressure() {
                        return false;
                    }
                    if seen_kinds.contains(&k) {
                        return false;
                    }
                    seen_kinds.push(k);
                    true
                }
                None => true,
            })
            .count();
        if count >= 3 || confirmed_ms > 30_000 {
            Severity::Critical
        } else if count >= 2 || confirmed_ms > 10_000 {
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

    /// 回归（2026-08-24 误报分析）：内存水位在进入线附近抖动时，会话不得碎裂成
    /// 连环短事件。带内帧（低于进入线、未到退出线）必须发射「滞回保持」文案维持
    /// 会话——对齐 CPU / DPC / 磁盘信号的语义。旧实现在带内帧不发射任何 cause，
    /// 任一回落帧都会令 causes 为空、立即关闭会话：水位在 90% 附近每抖一次就碎出
    /// 一条新事件（当晚 20:55 前后 20 秒内连出 3 条 major/minor 事件即此模式）。
    #[test]
    fn analyze_mem_oscillation_yields_single_session() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        config.hysteresis_hold_max_secs = 4; // 缩短带内保持上限，控制测试时长
        let mut d = Detector::new(&config);

        let mk_over = || {
            let mut s = make_sample(30.0, 8000, 10.0);
            s.mem_usage_percent = 90.5; // 越过进入线 90
            s
        };
        let mk_band = || {
            let mut s = make_sample(30.0, 8000, 10.0);
            s.mem_usage_percent = 88.0; // 滞回带内（85 ~ 90），未到退出线
            s
        };

        let tick = std::time::Duration::from_millis(1000);
        let mut events = Vec::new();
        // 越线×2 → 带内×2 → 再越线×2 → 带内直到 band-hold 超时后收尾
        for sample in [
            mk_over(),
            mk_over(),
            mk_band(), // 旧实现：此帧 causes 为空，立即关闭会话 → 事件 #1
            mk_band(),
            mk_over(), // 旧实现：重新开始会话
            mk_over(),
            mk_band(), // 旧实现：再次关闭 → 事件 #2
        ] {
            if let Some(ev) = d.analyze(&sample) {
                events.push(ev);
            }
            std::thread::sleep(tick);
        }
        for _ in 0..6 {
            if let Some(ev) = d.analyze(&mk_band()) {
                events.push(ev);
            }
            std::thread::sleep(tick);
        }

        assert_eq!(
            events.len(),
            1,
            "水位抖动不得碎裂成多条事件，实际 {}: {:?}",
            events.len(),
            events.iter().map(|e| (e.duration_ms, &e.causes)).collect::<Vec<_>>()
        );
        assert!(
            events[0].duration_ms >= 6000,
            "单次会话应覆盖整段震荡期，实际 {}ms",
            events[0].duration_ms
        );
        // 落库文案应保留「真实越线」触发证据（非带内「滞回保持」）——带内帧只负责
        // 维持会话存活（否则会碎裂成多条），不应覆盖真实越线证据（2026-10-16 治理）。
        assert!(
            events[0]
                .causes
                .iter()
                .any(|c| c.contains("Memory usage") && c.contains('>')),
            "落库文案应保留真实越线证据，got: {:?}",
            events[0].causes
        );
    }

    /// 回归（2026-08-24 误报分析）：severity 按 CauseKind 去重计数——同一根因的
    /// 多条文案（「Memory usage…」与「Memory paging…」同为 MemLow）不得凑数把
    /// 2 帧 90.2% 的边缘抖动虚增为 Major（旧实现按文本条数计数即如此）。
    #[test]
    fn determine_severity_same_kind_not_double_counted() {
        let same_kind = vec![
            "Memory usage 90.2% > 90%".to_string(),
            "Memory paging 550.0/s > 300/s".to_string(),
        ];
        assert_eq!(
            Detector::determine_severity(&same_kind, 4000),
            Severity::Minor,
            "同 kind 的两条文案只计 1 因"
        );
        // 防御路径保持不变：无法映射到 CauseKind 的文本按压力类逐条计。
        let unmapped = vec!["未知信号甲".to_string(), "未知信号乙".to_string()];
        assert_eq!(Detector::determine_severity(&unmapped, 4000), Severity::Major);
        // 2026-08-30 治理：时长判级改用「确认越线累计」——1 因 + 确认累计
        // >10s 升 Major，≤10s 保持 Minor（墙钟 duration 不再参与判级）。
        let one_cause = vec!["CPU usage 95.0% > 90%".to_string()];
        assert_eq!(
            Detector::determine_severity(&one_cause, 4_000),
            Severity::Minor,
            "确认累计未过线应保持 Minor"
        );
        assert_eq!(
            Detector::determine_severity(&one_cause, 11_000),
            Severity::Major,
            "确认累计超 10s 应升级 Major"
        );
    }

    /// 回归（2026-08-24 数据 observed [MemLow, MemLow, DiskBusy]）：同 kind 文案在
    /// 不同 tick 先后触达时，cause_kinds 不得出现重复——旧实现先 dedup() 后按首触
    /// 排序，「使用率@t0、磁盘@t1、可用@t2」的到达序使两个 MemLow 不相邻，
    /// dedup() 失效；必须先按 kind 去重再排序。
    #[test]
    fn analyze_cause_kinds_dedup_same_kind() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // f1：仅使用率越线（MemLow 首触）
        let mut s1 = make_sample(30.0, 5000, 10.0);
        s1.mem_usage_percent = 91.0;
        d.analyze(&s1);
        // f2：叠加磁盘繁忙（DiskBusy，插到两条 MemLow 文案之间）；
        // busy% 越线须同时给 IO 延迟确认（2026-08-29 治理）
        let mut s2 = s1.clone();
        s2.disk_busy_percent = 96.0;
        s2.disk_avg_io_ms = 12.0;
        std::thread::sleep(std::time::Duration::from_millis(1200)); // 越线帧累计真实时长
        d.analyze(&s2);
        // f3：可用内存跌破绝对下限（第二条 MemLow 文案，触达晚于 DiskBusy）
        let mut s3 = s2.clone();
        s3.mem_available_mb = 100;
        d.analyze(&s3);

        std::thread::sleep(std::time::Duration::from_millis(1200));
        let normal = make_sample(20.0, 8000, 10.0);
        let ev = d.analyze(&normal).expect("应生成事件");

        let mem_count = ev.cause_kinds.iter().filter(|k| **k == CauseKind::MemLow).count();
        assert_eq!(mem_count, 1, "MemLow 只应出现一次，got: {:?}", ev.cause_kinds);
        assert!(
            ev.cause_kinds.contains(&CauseKind::DiskBusy),
            "DiskBusy 应保留，got: {:?}",
            ev.cause_kinds
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

        // 回归（2026-08-29 循环论证治理）：分页高 + 磁盘 busy 100%（延迟亚毫秒）、
        // 内存侧健康（74% / 可用 8GB / commit 41%）-> 不得触发。旧口径把「磁盘忙」
        // 当压力证据，而磁盘忙正是分页读自己造成的，大文件冷读即凭此连环误报成
        // MemLow 卡顿（实测凌晨两条 major）；磁盘真饱和由 DiskBusy 独立表达。
        let mut cold_read = make_sample(25.0, 8300, 10.0);
        cold_read.mem_usage_percent = 74.0;
        cold_read.commit_bytes = 41_000;
        cold_read.commit_limit = 100_000; // commit 41%
        cold_read.page_reads_per_sec = 1229.0;
        cold_read.disk_busy_percent = 100.0;
        cold_read.disk_avg_io_ms = 0.5;
        let mut d4 = Detector::new(&config);
        d4.analyze(&cold_read);
        assert!(
            d4.current_causes.is_empty(),
            "大文件冷读（分页高 + 磁盘忙但延迟低 + 内存健康）不应触发任何 cause，got: {:?}",
            d4.current_causes
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

    /// F-RC2：磁盘真繁忙度（IO 延迟单独越线，或 % Disk Time 越线且延迟超确认线）
    /// 应触发 `DiskBusy` cause，且落库结构化根因为 `CauseKind::DiskBusy`
    /// （替代原磁盘 B/s spike）。
    #[test]
    fn analyze_disk_busy_triggers() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        let mut s = make_sample(30.0, 2000, 10.0);
        s.disk_busy_percent = 98.0; // % Disk Time 超 95
        s.disk_avg_io_ms = 12.0; // 且延迟超确认线 10ms（2026-08-29 治理）
        d.analyze(&s);
        assert!(
            d.current_causes.iter().any(|c| c.contains("Disk busy")),
            "磁盘繁忙度超阈值应触发 DiskBusy，got: {:?}",
            d.current_causes
        );

        // IO 延迟口径单独越线即触发（延迟是饱和金标准，与 busy% 分支为「或」）
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
        d.analyze(&s); // 越线帧累计真实持续时长（落库闸门按确认越线累计）
        let event = d.analyze(&make_sample(20.0, 2000, 10.0)).unwrap();
        assert!(
            event.cause_kinds.contains(&CauseKind::DiskBusy),
            "cause_kinds 应含 DiskBusy，got: {:?}",
            event.cause_kinds
        );
    }

    /// 回归（2026-08-29 误报治理，凌晨 2329/2330 实测数据）：NVMe/SSD 并行读写
    /// 把 % Disk Time 打满 100% 而 IO 延迟亚毫秒（无排队、不饱和）——吞吐 ≠ 饱和，
    /// busy% 越线若无延迟确认不得判 DiskBusy。原「任一越线即忙」的 OR 判定把
    /// 100~150MB/s 顺序读误判成磁盘饱和，与 paging 循环互证撑起两条 major 误报。
    #[test]
    fn analyze_disk_busy_high_but_low_latency_no_trigger() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // 实测误报样本形态：busy 100% + IO 0.5ms + 150MB/s 读，内存/CPU 健康
        let mut s = make_sample(25.0, 8300, 10.0);
        s.mem_usage_percent = 74.0;
        s.disk_busy_percent = 100.0;
        s.disk_avg_io_ms = 0.5;
        s.disk_read_bps = 150 * 1024 * 1024;
        s.page_reads_per_sec = 1229.0; // Page Reads 计入大文件预读
        d.analyze(&s);
        assert!(
            !d.disk_busy_active,
            "busy 100% 但延迟亚毫秒不应激活 DiskBusy（吞吐 ≠ 饱和）"
        );
        assert!(
            d.current_causes.is_empty(),
            "NVMe 大吞吐冷读 + 内存健康不应产生任何卡顿 cause，got: {:?}",
            d.current_causes
        );

        // 持续多帧 + 回落：不得落库事件（原实现连出两条 major）
        for _ in 0..5 {
            d.analyze(&s);
        }
        let normal = make_sample(20.0, 8300, 10.0);
        assert!(
            d.analyze(&normal).is_none(),
            "未进入卡顿跟踪则不得产出事件"
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
        d.analyze(&s); // 越线帧累计真实持续时长（落库闸门按确认越线累计）
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
        d.analyze(&s); // 越线帧累计真实持续时长（落库闸门按确认越线累计）
        let event = d.analyze(&make_sample(20.0, 2000, 10.0)).unwrap();
        assert!(
            event.cause_kinds.contains(&CauseKind::InterruptStorm),
            "cause_kinds 应含 InterruptStorm，got: {:?}",
            event.cause_kinds
        );
    }

    /// F-RC2：`Context Switches/sec` 按核越线 + CPU 侧压力证据 → 触发
    /// `ContextSwitchStorm` cause 并落库 `CauseKind::ContextSwitchStorm`。
    /// 2026-08-30 治理：激活需连续确认（recent 10 中 ≥6 帧越线），故喂满确认帧。
    #[test]
    fn analyze_context_switch_triggers() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // 14 核、150k/s ≈ 10.7k/core（> 10k/core 进入线）、CPU 85%（> 80% 证据线）
        let mut s = make_sample(85.0, 2000, 10.0);
        s.cpu_per_core = vec![85.0; 14];
        s.context_switches_per_sec = 150_000.0;
        for _ in 0..6 {
            d.analyze(&s);
        }
        assert!(
            d.current_causes
                .iter()
                .any(|c| c.contains("Context switches")),
            "Context switches 按核越线 + CPU 压力证据应触发 ContextSwitchStorm，got: {:?}",
            d.current_causes
        );
        for _ in 0..2 {
            d.analyze(&s);
        }
        std::thread::sleep(std::time::Duration::from_millis(1200));
        d.analyze(&s); // 越线帧累计真实持续时长（落库闸门按确认越线累计）
        let event = d.analyze(&make_sample(20.0, 2000, 10.0)).unwrap();
        assert!(
            event.cause_kinds.contains(&CauseKind::ContextSwitchStorm),
            "cause_kinds 应含 ContextSwitchStorm，got: {:?}",
            event.cause_kinds
        );
    }

    /// 回归（2026-08-17 误报治理）：多核机日常基线不得触发 ContextSwitchStorm。
    /// 实测：14 核机凌晨基线 45k~70k/s（3~5k/core）、CPU 仅 20~50%——旧绝对阈值
    /// 50000/s 下 33% 样本越线、25k/s 退出线不可达（连续低于它的最长段仅 2s），
    /// 一次越线即滞回锁死数分钟，凌晨无人在场连环 major/critical 误报。
    /// 按核归一（10k/s/核）后这些样本远离进入线，不得产生任何 cause/事件。
    #[test]
    fn analyze_ctx_switch_no_trigger_on_multicore_baseline() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        let mut s = make_sample(50.0, 2000, 10.0);
        s.cpu_per_core = vec![50.0; 14];
        s.context_switches_per_sec = 70_000.0; // ≈5k/core，仍 < 10k/core 进入线
        for _ in 0..5 {
            assert!(d.analyze(&s).is_none());
            assert!(
                d.current_causes
                    .iter()
                    .all(|c| !c.contains("Context switches")),
                "多核日常基线不应触发 ContextSwitchStorm，got: {:?}",
                d.current_causes
            );
        }
        assert!(d.analyze(&make_sample(20.0, 2000, 10.0)).is_none());
    }

    /// 回归（2026-08-17 误报治理）：切换速率按核越线但无 CPU 侧压力证据
    /// （低 CPU 且 DPC/中断正常）——后台异步活动的孤立高切换，不得首次激活。
    #[test]
    fn analyze_ctx_switch_no_trigger_without_cpu_pressure_evidence() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        let mut s = make_sample(50.0, 2000, 10.0); // CPU 50% < 80% 证据线
        s.cpu_per_core = vec![50.0; 14];
        s.context_switches_per_sec = 180_000.0; // ≈12.9k/core，已越进入线
        for _ in 0..3 {
            d.analyze(&s);
            assert!(
                d.current_causes
                    .iter()
                    .all(|c| !c.contains("Context switches")),
                "无 CPU 压力证据的高切换不应触发，got: {:?}",
                d.current_causes
            );
        }
    }

    /// 激活后按核速率回落到滞回带内（退出线 5k/core 之上）→ cause 保持、文案标注
    /// 「滞回保持」；降到退出线以下才解除并落库事件（滞回语义不因证据门改变）。
    /// 2026-08-30 治理后激活需连续确认（recent 10 中 ≥6 帧越线），故先喂满确认帧。
    #[test]
    fn analyze_ctx_switch_hysteresis_hold_then_release() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // 进入：150k/s ≈ 10.7k/core，CPU 85%（证据成立）；喂满 6 帧确认
        let mut storm = make_sample(85.0, 2000, 10.0);
        storm.cpu_per_core = vec![85.0; 14];
        storm.context_switches_per_sec = 150_000.0;
        for _ in 0..6 {
            d.analyze(&storm);
        }
        assert!(
            d.current_causes
                .iter()
                .any(|c| c.contains("Context switches") && c.contains('>')),
            "风暴样本应产出越线文案，got: {:?}",
            d.current_causes
        );
        // 隔一晚再喂一帧越线样本：累计真实越线时长，满足落库闸门（确认越线累计）
        std::thread::sleep(std::time::Duration::from_millis(1200));
        d.analyze(&storm);

        // 带内保持：100k/s ≈ 7.1k/core（低于进入线、高于退出线），CPU 回落也不解除
        let mut band = make_sample(60.0, 2000, 10.0);
        band.cpu_per_core = vec![60.0; 14];
        band.context_switches_per_sec = 100_000.0;
        d.analyze(&band);
        assert!(
            d.current_causes
                .iter()
                .any(|c| c.contains("Context switches") && c.contains("滞回保持")),
            "带内样本应产出滞回保持文案，got: {:?}",
            d.current_causes
        );

        // 退出：50k/s ≈ 3.6k/core < 退出线 5k/core → 解除并落库
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let mut release = make_sample(20.0, 2000, 10.0);
        release.cpu_per_core = vec![20.0; 14];
        release.context_switches_per_sec = 50_000.0;
        let event = d.analyze(&release).unwrap();
        assert!(event.cause_kinds.contains(&CauseKind::ContextSwitchStorm));
    }

    /// 回归（2026-08-30 误报治理）：机器日常基线（6~9k/core）骑在滞回退出线
    /// （×0.5 = 5k/core）之上，一次尖峰激活后滞回几乎不可自然退出、间歇尖峰又
    /// 反复重置带保持计时——实测凌晨连环 major/critical 误报的根源。占比确认
    /// 之后：间歇尖峰（占比 < 6/10）既不得首次激活，也不得重置带保持计时续命。
    #[test]
    fn analyze_ctx_switch_intermittent_spikes_not_sustained() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        config.hysteresis_hold_max_secs = 1; // 缩短带保持上限，控制测试时长
        let mut d = Detector::new(&config);

        // 阶段一：交替尖峰/基线（占比 50% < 60% 确认线）→ 不得激活
        let mut spike = make_sample(85.0, 2000, 10.0); // CPU 证据成立
        spike.cpu_per_core = vec![85.0; 14];
        spike.context_switches_per_sec = 150_000.0; // ≈10.7k/core 越线
        let mut base = spike.clone();
        base.context_switches_per_sec = 70_000.0; // ≈5k/core 带内
        for i in 0..12 {
            d.analyze(if i % 2 == 0 { &spike } else { &base });
        }
        assert!(
            d.current_causes
                .iter()
                .all(|c| !c.contains("Context switches")),
            "间歇尖峰（占比 50%）不得激活风暴，got: {:?}",
            d.current_causes
        );

        // 阶段二：喂满确认帧激活后尖峰稀疏化（一帧尖峰 + 长段带内）——
        // 带保持计时不被尖峰重置，超时强制解除
        for _ in 0..6 {
            d.analyze(&spike);
        }
        assert!(d.ctx_switch_active, "持续越线应激活风暴");
        for i in 0..10 {
            std::thread::sleep(std::time::Duration::from_millis(300));
            d.analyze(if i == 0 { &spike } else { &base });
        }
        assert!(
            !d.ctx_switch_active,
            "稀疏尖峰不得重置带保持计时续命，应超时强制解除"
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
        d.analyze(&hot); // 越线帧累计真实持续时长（落库闸门按确认越线累计）
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
        d.analyze(&high); // 越线帧累计真实持续时长（落库闸门按确认越线累计）

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
        d.analyze(&make_sample_net(95.0, 2000, 10.0, 5_000_000)); // 累计真实越线时长
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
        d.analyze(&high); // 越线帧累计真实持续时长（落库闸门按确认越线累计）
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
        d.analyze(&high); // 累计真实越线时长，满足落库闸门

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
        d.analyze(&bad); // 累计真实越线时长，满足落库闸门

        let normal = make_sample(20.0, 2000, 10.0);
        let event = d.analyze(&normal).unwrap();
        assert_eq!(event.severity, Severity::Major);
    }

    #[test]
    fn analyze_severity_critical_three_causes() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // High CPU + low memory/使用率爆表 + 磁盘繁忙 → 4 条文案但恰好 3 个压力类
        // CauseKind（两条内存文案同为 MemLow，severity 按 kind 去重只计 1 因；
        // swap 已降级为仅展示）
        let mut bad = make_sample(95.0, 100, 10.0);
        bad.mem_usage_percent = 95.0;
        bad.disk_busy_percent = 96.0;
        bad.disk_avg_io_ms = 12.0; // busy% 越线须 IO 延迟确认（2026-08-29 治理）
        for _ in 0..3 {
            d.analyze(&bad);
        }
        std::thread::sleep(std::time::Duration::from_millis(1200));
        d.analyze(&bad); // 累计真实越线时长，满足落库闸门

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
    /// （防抖），带内发射「滞回保持」文案维持会话不碎裂（2026-08-24 回归修复：
    /// 旧的带内静默会让任一回落帧关闭会话，90% 附近每抖一次就碎出一条事件）；
    /// 回落 84%（低于退出线 85）才解除。
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

        // 带内 87%：状态保持（滞回防抖），发射「滞回保持」文案（数值已回落、未到退出线）
        let mut band = make_sample(30.0, 2000, 10.0);
        band.mem_usage_percent = 87.0;
        d.analyze(&band);
        assert!(d.mem_level_active, "带内应保持激活（滞回防抖）");
        assert_eq!(d.current_causes.len(), 1, "同类型文案应被去重替换而非追加");
        assert!(
            d.current_causes[0].contains("Memory usage") && d.current_causes[0].contains("滞回保持"),
            "带内应发射「滞回保持」文案维持会话，got: {:?}",
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

        // 预热：先喂一帧中性样本，使首帧越线样本带非零帧间隔（落库闸门按确认越线累计）
        d.analyze(&make_sample(30.0, 2000, 10.0));
        std::thread::sleep(std::time::Duration::from_millis(1100));

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

    /// 回归（2026-08-30 误报治理）：内存高压机器上分页可常态越线（实测 121s
    /// 事件中 94/96 帧分页 >300/s），旧口径「分页不受稳态抑制影响」让它给整个
    /// 会话无限续命。分页与水位共用 mem_chronic 锁存：稳态判立后一并停发，
    /// 会话有界；回落解锁后恢复。
    #[test]
    fn analyze_paging_chronic_suppressed_with_mem_level() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        config.mem_chronic_seconds = 1; // 测试用：1s 即判定稳态
        let mut d = Detector::new(&config);

        // 预热：先喂一帧中性样本，使首帧越线样本带非零帧间隔（落库闸门按确认越线累计）
        d.analyze(&make_sample(30.0, 2000, 10.0));
        std::thread::sleep(std::time::Duration::from_millis(1100));

        // 水位 92% + 分页 400/s（证据：使用率越线）→ 会话开始
        let mut high = make_sample(30.0, 2000, 10.0);
        high.mem_usage_percent = 92.0;
        high.page_reads_per_sec = 400.0;
        d.analyze(&high);
        assert!(d.stutter_start.is_some());

        // 连续活跃 >1s → 稳态锁存：水位与分页一并停发 → 会话有界落库
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let event = d.analyze(&high).expect("稳态锁存应有界落库");
        assert!(
            event.causes.iter().any(|c| c.contains("Memory paging")),
            "事件应携带抑制前的分页 cause，got: {:?}",
            event.causes
        );

        // 锁存期：分页仍越线、证据仍在 → 不得重开内存侧会话
        d.analyze(&high);
        assert!(
            d.stutter_start.is_none(),
            "锁存期内分页不得重开内存侧会话"
        );
        assert!(
            d.current_causes
                .iter()
                .all(|c| !c.contains("Memory paging")),
            "锁存期内分页 cause 应停发，got: {:?}",
            d.current_causes
        );

        // 回落到退出线以下 → 解锁 → 再次越线恢复触发
        let mut recover = make_sample(30.0, 2000, 10.0);
        recover.mem_usage_percent = 84.0;
        d.analyze(&recover);
        assert!(!d.mem_level_suppressed, "水位回落应解锁");
        d.analyze(&high);
        assert!(
            d.current_causes
                .iter()
                .any(|c| c.contains("Memory paging")),
            "解锁后分页应恢复发射，got: {:?}",
            d.current_causes
        );
    }

    /// 回归（2026-08-30 误报治理）：severity 时长判级用「确认越线累计」而非
    /// 墙钟 duration——滞回带「保持」帧把短会话拖过 10s 不得虚增为 Major
    /// （实测凌晨 ContextSwitchStorm 滞回锁死事件全部凭墙钟时长升级弹窗）。
    #[test]
    fn analyze_severity_uses_confirmed_duration_not_wallclock() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // 先喂 3 帧真越线（确认累计≈3s，满足落库闸门），再长期带内「滞回保持」
        // 把墙钟时长撑过 Major 线（10s）——severity 应只认确认累计，保持 Minor。
        let hot = make_sample(95.0, 2000, 10.0);
        d.analyze(&hot);
        for _ in 0..3 {
            std::thread::sleep(std::time::Duration::from_millis(1000));
            d.analyze(&hot);
        }
        let band = make_sample(85.0, 2000, 10.0); // 80~90 带内：滞回保持
        for _ in 0..10 {
            std::thread::sleep(std::time::Duration::from_millis(1000));
            d.analyze(&band);
        }
        let event = d
            .analyze(&make_sample(20.0, 2000, 10.0))
            .expect("正常帧应结束会话并落库");
        assert!(
            event.duration_ms > 10_000,
            "墙钟时长应已超过 Major 线（10s），实际 {}ms",
            event.duration_ms
        );
        assert_eq!(
            event.severity,
            Severity::Minor,
            "确认累计≈3s（<10s）不得凭墙钟时长（>10s）升级 Major，got {:?}，causes={:?}",
            event.severity,
            event.causes
        );

        // 对照：真饱和帧的确认累计应跟随帧间隔增长（时长判级对真卡顿不变）
        let mut d2 = Detector::new(&config);
        d2.analyze(&make_sample(95.0, 2000, 10.0));
        std::thread::sleep(std::time::Duration::from_millis(300));
        d2.analyze(&make_sample(95.0, 2000, 10.0));
        assert!(
            d2.session_confirmed_ms >= 200,
            "真饱和帧的确认累计应增长，实际 {}ms",
            d2.session_confirmed_ms
        );
    }

    /// 回归（2026-10-16 治理）：落库闸门按「真实越线累计」——瞬时尖峰（仅 1~2 帧
    /// 越线）+ 长时间滞回带保持，即使墙钟时长远超 sustained 也不得落库（否则一台
    /// 常年骑在 88~90% 边缘的机器会碎出大量「滞回保持」minor 事件）。
    #[test]
    fn analyze_brief_blip_within_hysteresis_is_not_stored() {
        let mut config = DetectionConfig::default();
        config.sustained_seconds = 1;
        let mut d = Detector::new(&config);

        // 2 帧真越线：首帧 dt≈0，次帧 dt≈300ms → 确认累计≈300ms < 1s
        let hot = make_sample(95.0, 2000, 10.0);
        d.analyze(&hot);
        std::thread::sleep(std::time::Duration::from_millis(300));
        d.analyze(&hot);

        // 带内「滞回保持」把墙钟时长拖到 >1s（确认累计不增长）
        let band = make_sample(85.0, 2000, 10.0);
        for _ in 0..5 {
            std::thread::sleep(std::time::Duration::from_millis(400));
            d.analyze(&band);
        }

        // 正常帧结束会话：墙钟≈2.3s > 1s，但确认累计仅≈300ms → 闸门丢弃
        let ev = d.analyze(&make_sample(20.0, 2000, 10.0));
        assert!(
            ev.is_none(),
            "瞬时尖峰（确认越线累计 < sustained）不得落库，got: {:?}",
            ev
        );
        assert!(d.stutter_start.is_none(), "会话应已结束");
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
        d.analyze(&s); // 累计真实越线时长，满足落库闸门

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
        d.analyze(&high); // 累计真实越线时长，满足落库闸门
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
        d.analyze(&high); // 累计真实越线时长，满足落库闸门
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
        d.analyze(&high); // 累计真实越线时长，满足落库闸门
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
