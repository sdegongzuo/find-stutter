//! 配置 / 皮肤热加载（P2）。
//!
//! 用 [`notify`] crate 监听：
//! - `config.toml` 变更 → [`HotReloadEvent::ConfigChanged`]
//! - `skins/<name>/skin.toml` 变更 → [`HotReloadEvent::SkinChanged(name)`]
//!
//! 事件通过内部 `mpsc::Receiver` 投递，调用方在 tick 里 `try_recv()` 消费。
//! watcher 启动失败（如文件不存在 / 权限不足）只 log warn，不阻塞 GUI。
//!
//! ## 单元测试策略
//! notify 后端在不同 OS 上行为差异较大（Windows ReadDirectoryChangesW
//! / macOS FSEvents / Linux inotify）。我们把「路径 → 事件类型」的解析
//! 逻辑抽到纯函数 [`classify_change`]，watcher 本身只负责把 notify 事件
//! 转成 `HotReloadEvent` 并去重，这样可以在不依赖文件系统的情况下测试。
//!
//! ## 降级 / 失败
//! - 启动时 config.toml 不存在：watcher 仍创建，每次 tick 由调用方决定
//!   如何处理（目前是已经在 lib.rs 用 `unwrap_or_default`）
//! - notify 在某 OS 失败：返回 [`ConfigWatcher`] 但 receiver 永远为空，
//!   调用方继续按现有配置运行

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, SystemTime};

use notify::{Config as NotifyConfig, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// 热加载事件
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotReloadEvent {
    /// `config.toml` 修改（路径作为标识，方便日志区分）
    ConfigChanged(PathBuf),
    /// `skins/<name>/skin.toml` 修改（name 是 skins 下的子目录名）
    SkinChanged { skin_name: String, path: PathBuf },
}

/// 把 notify 的 [`Event`] 转成 0~N 个 [`HotReloadEvent`]。
///
/// - `config.toml` 写入 / 修改 → `ConfigChanged`
/// - `skins/<dir>/skin.toml` 写入 / 修改 → `SkinChanged`
/// - 其它路径（`.swp`、`~` 临时文件、`.git` 等）忽略
///
/// 注意：notify 在某些后端（macOS FSEvents）会把重命名 / 删除 / 创建
/// 都映射到 `ModifyKind::Data`，所以我们对 `Any` / `Modify*` / `Create*`
/// 都视为「内容变更」。
pub fn classify_change(
    event: &Event,
    config_path: &Path,
    skins_dir: &Path,
) -> Vec<HotReloadEvent> {
    if !matches!(
        event.kind,
        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Any
    ) {
        return vec![];
    }

    let mut out = vec![];
    for p in &event.paths {
        // 1) config.toml 自身
        if paths_equal(p, config_path) {
            out.push(HotReloadEvent::ConfigChanged(p.clone()));
            continue;
        }

        // 2) skins/<name>/skin.toml
        if let Some(rel) = p.strip_prefix(skins_dir).ok() {
            // rel 必须是 "<name>/skin.toml" 两段
            let parts: Vec<_> = rel.components().collect();
            if parts.len() == 2 {
                let dir_name = parts[0].as_os_str().to_string_lossy().to_string();
                let file_name = parts[1].as_os_str().to_string_lossy().to_string();
                if file_name == "skin.toml" {
                    out.push(HotReloadEvent::SkinChanged {
                        skin_name: dir_name,
                        path: p.clone(),
                    });
                    continue;
                }
            }
        }

        // 其它（tmp / swap / git 等）忽略
    }
    out
}

/// 路径相等（Windows 不区分大小写，Unix 区分）
fn paths_equal(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.to_string_lossy().to_ascii_lowercase() == b.to_string_lossy().to_ascii_lowercase()
    } else {
        a == b
    }
}

/// 解析实际存在的 skins 目录：优先调用方给的路径，
/// 不存在时兜底 workspace 源码布局 `crates/ui/skins`。
fn resolve_skins_dir(preferred: &Path) -> PathBuf {
    if preferred.exists() {
        return preferred.to_path_buf();
    }
    let workspace = Path::new("crates").join("ui").join("skins");
    if workspace.exists() {
        workspace
    } else {
        preferred.to_path_buf()
    }
}

/// 防抖窗口：在该窗口内多次 `ConfigChanged` 只触发一次 reload。
/// 解决多数编辑器「保存 = 写入新文件 + 重命名覆盖」触发双事件的问题。
pub const DEBOUNCE: Duration = Duration::from_millis(150);

/// 热加载 watcher。
///
/// 内部维护 `notify::RecommendedWatcher` + `mpsc::Receiver<HotReloadEvent>`。
/// 持有 `_watcher` 防止后台线程被 drop。
pub struct ConfigWatcher {
    _watcher: RecommendedWatcher,
    rx: Receiver<HotReloadEvent>,
    /// 上次发出事件的时间（用于 DEBOUNCE）
    last_emit: parking_lot::Mutex<Option<SystemTime>>,
    /// 上次发出的事件类型（同类型 DEBOUNCE 才生效）
    last_kind: parking_lot::Mutex<Option<HotReloadEvent>>,
}

impl ConfigWatcher {
    /// 创建 watcher。
    ///
    /// - `config_path`：监听此具体文件（如果父目录存在）
    /// - `skins_dir`：监听此目录（含子目录）；若目录不存在，
    ///   自动兜底监听 workspace 源码布局 `crates/ui/skins`
    ///
    /// 失败时（如目录不存在）返回 `Err`，调用方应 log warn 后继续运行。
    pub fn new<P1: AsRef<Path>, P2: AsRef<Path>>(
        config_path: P1,
        skins_dir: P2,
    ) -> notify::Result<Self> {
        let config_path = config_path.as_ref().to_path_buf();
        let skins_dir = resolve_skins_dir(skins_dir.as_ref());
        let (tx, rx) = channel();
        let watch_config = config_path.clone();
        let watch_skins = skins_dir.clone();

        // 1) 监听 config.toml 所在的目录（notify 不直接监听单文件，
        //    要监听父目录再在 classify_change 里过滤）
        let mut watcher = RecommendedWatcher::new(
            move |res: notify::Result<Event>| {
                if let Ok(event) = res {
                    for ev in classify_change(&event, &watch_config, &watch_skins) {
                        // 发送失败只可能是 receiver 被 drop
                        let _ = tx.send(ev);
                    }
                }
            },
            NotifyConfig::default().with_poll_interval(Duration::from_secs(2)),
        )?;

        // 2) 监听 config.toml 所在目录
        if let Some(parent) = config_path.parent() {
            if parent.exists() {
                watcher.watch(parent, RecursiveMode::NonRecursive)?;
            } else {
                log::warn!(
                    "ConfigWatcher: config parent dir {:?} does not exist",
                    parent
                );
            }
        }

        // 3) 监听 skins 目录
        if skins_dir.exists() {
            watcher.watch(&skins_dir, RecursiveMode::Recursive)?;
        } else {
            log::warn!(
                "ConfigWatcher: skins dir {:?} does not exist (will not watch)",
                skins_dir
            );
        }

        Ok(Self {
            _watcher: watcher,
            rx,
            last_emit: parking_lot::Mutex::new(None),
            last_kind: parking_lot::Mutex::new(None),
        })
    }

    /// 构造一个不监听任何文件的 watcher（notify 初始化失败时的降级）。
    /// receiver 永远为空，调用方继续按现有配置运行。
    pub fn disabled() -> Self {
        let (_tx, rx) = channel();
        Self {
            _watcher: RecommendedWatcher::new(|_| {}, NotifyConfig::default()).unwrap(),
            rx,
            last_emit: parking_lot::Mutex::new(None),
            last_kind: parking_lot::Mutex::new(None),
        }
    }

    /// 消费一个事件（应用 DEBOUNCE）。
    ///
    /// - 返回 `Some(event)` 表示这是个「新的、值得触发 reload」的事件
    /// - 返回 `None` 表示队列空 / 在 DEBOUNCE 窗口内重复
    ///
    /// `clear_last` 在 reload 完成后调用，避免「下次同文件再次变更
    /// 却被上次的 debounce 抑制」。
    pub fn try_recv(&self) -> Option<HotReloadEvent> {
        // drain 队列：只保留最后一个（最新）事件
        let mut latest = None;
        while let Ok(ev) = self.rx.try_recv() {
            latest = Some(ev);
        }
        let ev = latest?;

        let now = SystemTime::now();
        let mut last_emit = self.last_emit.lock();
        let mut last_kind = self.last_kind.lock();

        // DEBOUNCE：同类型事件在窗口内忽略
        if let (Some(prev_t), Some(prev_k)) = (*last_emit, last_kind.as_ref()) {
            if prev_k == &ev {
                if now
                    .duration_since(prev_t)
                    .map(|d| d < DEBOUNCE)
                    .unwrap_or(true)
                {
                    return None;
                }
            }
        }

        *last_emit = Some(now);
        *last_kind = Some(ev.clone());
        Some(ev)
    }

    /// 强制清空 debounce 状态（reload 完成后调用，让下一次变更能立即生效）
    pub fn clear_debounce(&self) {
        *self.last_emit.lock() = None;
        *self.last_kind.lock() = None;
    }

    /// 仅用于测试：构造时不启动 notify 线程，直接注入一个 receiver
    #[cfg(test)]
    pub fn from_receiver(rx: Receiver<HotReloadEvent>) -> Self {
        Self {
            _watcher: RecommendedWatcher::new(|_| {}, NotifyConfig::default()).unwrap(),
            rx,
            last_emit: parking_lot::Mutex::new(None),
            last_kind: parking_lot::Mutex::new(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{DataChange, ModifyKind};
    use std::path::PathBuf;

    fn path(s: &str) -> PathBuf {
        PathBuf::from(s.replace('/', std::path::MAIN_SEPARATOR_STR))
    }

    #[test]
    fn classify_config_modify() {
        let config = path("./config.toml");
        let skins = path("./skins");
        let ev = Event {
            kind: EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            paths: vec![config.clone()],
            attrs: Default::default(),
        };
        let out = classify_change(&ev, &config, &skins);
        assert_eq!(out, vec![HotReloadEvent::ConfigChanged(config)]);
    }

    #[test]
    fn classify_skin_modify() {
        let config = path("./config.toml");
        let skins = path("./skins");
        let skin_path = path("./skins/dark/skin.toml");
        let ev = Event {
            kind: EventKind::Modify(ModifyKind::Any),
            paths: vec![skin_path.clone()],
            attrs: Default::default(),
        };
        let out = classify_change(&ev, &config, &skins);
        assert_eq!(
            out,
            vec![HotReloadEvent::SkinChanged {
                skin_name: "dark".into(),
                path: skin_path,
            }]
        );
    }

    #[test]
    fn classify_ignores_tmp_files() {
        let config = path("./config.toml");
        let skins = path("./skins");
        // 编辑器临时文件：.config.toml.swp
        let tmp = path("./.config.toml.swp");
        let ev = Event {
            kind: EventKind::Create(notify::event::CreateKind::Any),
            paths: vec![tmp],
            attrs: Default::default(),
        };
        let out = classify_change(&ev, &config, &skins);
        assert!(out.is_empty(), "临时文件应被忽略: {:?}", out);
    }

    #[test]
    fn classify_ignores_non_skin_files_in_skins_dir() {
        let config = path("./config.toml");
        let skins = path("./skins");
        // skins/default/preview.png 不是 skin.toml
        let png = path("./skins/default/preview.png");
        let ev = Event {
            kind: EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            paths: vec![png],
            attrs: Default::default(),
        };
        let out = classify_change(&ev, &config, &skins);
        assert!(out.is_empty());
    }

    #[test]
    fn classify_ignores_non_modify_events() {
        let config = path("./config.toml");
        let skins = path("./skins");
        let ev = Event {
            kind: EventKind::Access(notify::event::AccessKind::Open(
                notify::event::AccessMode::Any,
            )),
            paths: vec![config.clone()],
            attrs: Default::default(),
        };
        let out = classify_change(&ev, &config, &skins);
        assert!(out.is_empty(), "Access 事件不应触发 reload");
    }

    #[test]
    fn classify_handles_multiple_paths() {
        let config = path("./config.toml");
        let skins = path("./skins");
        let skin_path = path("./skins/light/skin.toml");
        let unrelated = path("./skins/light/README.md");
        let ev = Event {
            kind: EventKind::Modify(ModifyKind::Any),
            paths: vec![config.clone(), skin_path.clone(), unrelated],
            attrs: Default::default(),
        };
        let out = classify_change(&ev, &config, &skins);
        assert_eq!(out.len(), 2, "应识别 config + skin，忽略 README");
        assert!(matches!(out[0], HotReloadEvent::ConfigChanged(_)));
        assert!(matches!(out[1], HotReloadEvent::SkinChanged { .. }));
    }

    #[test]
    fn debounce_suppresses_repeats() {
        let (_tx, rx) = channel();
        let watcher = ConfigWatcher::from_receiver(rx);
        let ev = HotReloadEvent::ConfigChanged(path("./config.toml"));

        // 第一次 → 返回 Some
        _tx.send(ev.clone()).unwrap();
        let first = watcher.try_recv();
        assert!(first.is_some());

        // 立即再发同一个 → 应被 DEBOUNCE 抑制
        _tx.send(ev.clone()).unwrap();
        let second = watcher.try_recv();
        assert!(second.is_none(), "DEBOUNCE 窗口内重复事件应被抑制");
    }

    #[test]
    fn debounce_resets_after_clear() {
        let (_tx, rx) = channel();
        let watcher = ConfigWatcher::from_receiver(rx);
        let ev = HotReloadEvent::ConfigChanged(path("./config.toml"));

        _tx.send(ev.clone()).unwrap();
        assert!(watcher.try_recv().is_some());

        watcher.clear_debounce();

        _tx.send(ev.clone()).unwrap();
        assert!(watcher.try_recv().is_some(), "clear 后再次变更应立即生效");
    }

    #[test]
    fn different_kinds_not_debounced() {
        let (_tx, rx) = channel();
        let watcher = ConfigWatcher::from_receiver(rx);
        let cfg = HotReloadEvent::ConfigChanged(path("./config.toml"));
        let skin = HotReloadEvent::SkinChanged {
            skin_name: "dark".into(),
            path: path("./skins/dark/skin.toml"),
        };

        _tx.send(cfg).unwrap();
        assert!(watcher.try_recv().is_some());

        // 不同类型的事件不应被上一次的 debounce 抑制
        _tx.send(skin).unwrap();
        assert!(watcher.try_recv().is_some(), "不同类型事件应各自触发");
    }

    #[test]
    fn empty_queue_returns_none() {
        let (_tx, rx) = channel();
        let watcher = ConfigWatcher::from_receiver(rx);
        assert!(watcher.try_recv().is_none());
    }

    #[test]
    fn paths_equal_case_insensitive_on_windows() {
        let a = path("C:/Foo/Bar.TOML");
        let b = path("c:/foo/bar.toml");
        if cfg!(windows) {
            assert!(paths_equal(&a, &b));
        } else {
            assert!(!paths_equal(&a, &b));
        }
    }
}