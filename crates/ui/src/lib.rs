//! find-stutter UI overlay (P3 read-only mode).
//!
//! ## P3 架构变化
//!
//! 不再在 GUI 内启动 Collector 线程。GUI 只做一件事：
//! 1Hz 轮询 `stutter.db`（由 find-stutter-service 后台持续写入）。
//!
//! - 删除 `spawn_collector()` / `Collector` 实例
//! - 新增 [`reader::DbReader`]：SQLite 只读连接 + 服务健康检测
//! - 1Hz 定时器 → 调 `DbReader::poll()` → 拿 `PollResult` 喂 Slint
//!
//! 服务健康检测：
//! - `Running`：心跳在 5s 内
//! - `Stale`：心跳存在但 > 5s
//! - `Stopped`：心跳表为空
//! - `NoDatabase`：stutter.db 不存在
//!
//! UI 反应：
//! - `Running` → 顶部状态条绿色 "● 服务运行中"
//! - `Stale`   → 黄色 "● 服务卡顿"
//! - `Stopped` / `NoDatabase` → 红色 "● 服务已停止"
//! - 暂停按钮在非 Running 时禁用

pub mod auto_start;
pub mod overlay;
pub mod reader;
pub mod skin;
pub mod window;

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use slint::{ComponentHandle, Timer};

use crate::overlay::OverlayState;
use crate::reader::{DbReader, PollResult};

slint::include_modules!();

/// 1Hz 轮询 tick 周期
const POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// 启动 UI overlay（只读模式）。
///
/// 加载 config.toml → 构造 `DbReader` → 起 1Hz 定时器 → 启动 Slint 事件循环。
pub fn run() -> anyhow::Result<()> {
    env_logger::init();

    let config = find_stutter_core::Config::load("config.toml").unwrap_or_else(|e| {
        log::warn!("config load failed ({}), using defaults", e);
        find_stutter_core::Config::default()
    });
    log::info!(
        "find-stutter overlay (P3 read-only) starting, db={}",
        config.storage.db_path
    );

    // 0) P3+：自动检测 + 启动后台服务（不影响 GUI 启动，失败只记日志）
    let auto = auto_start::ensure_service_running();
    if auto.is_ok() {
        log::info!("后台服务: {}", auto.message());
    } else {
        log::warn!("后台服务: {}", auto.message());
    }

    // 1) 加载皮肤
    let skin_cfg = skin::SkinConfig::load("default");
    let state = Arc::new(Mutex::new(OverlayState::new(skin_cfg)));

    // 2) 构造只读 reader（db 暂时不存在不会立即失败，下一次 tick 会重试）
    let reader = Arc::new(DbReader::new(config.storage.db_path.clone()));

    // 3) 启动 Slint 窗口
    let ui = Overlay::new()?;
    ui.show()?;

    // 4) 1Hz 轮询：reader.poll() → 推送到 Slint
    let timer = Timer::default();
    let weak_ui = ui.as_weak();
    let reader_for_tick = reader.clone();
    let state_for_tick = state.clone();
    timer.start(
        slint::TimerMode::Repeated,
        POLL_INTERVAL,
        move || {
            let poll: PollResult = reader_for_tick.poll();
            // 1) 更新共享状态
            state_for_tick.lock().update_from_poll(&poll);
            // 2) 推到 Slint（窗口已关闭时不操作）
            if let Some(ui) = weak_ui.upgrade() {
                let s = state_for_tick.lock();
                overlay::apply_metrics(&ui, &s);
            }
        },
    );

    // 5) 启动 Slint 事件循环（阻塞）
    slint::run_event_loop_until_quit()?;
    Ok(())
}
