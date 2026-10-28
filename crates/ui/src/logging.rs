//! GUI 日志落盘与崩溃现场捕获。
//!
//! ## 为什么需要
//!
//! release 下 GUI 启动前会调 [`crate::window::hide_console_for_gui`] 解除控制台关联
//! （避免弹黑框），而 `env_logger` 默认只写 stderr —— 控制台一没了，日志就彻底不可见。
//! 更糟的是 Rust panic 走的是 `std::process::exit(101)`，Windows 的 WER 只记录未处理的
//! 结构化异常，**不会**为 panic 留下任何事件日志。结果就是：进程突然消失，查无实据。
//!
//! 本模块做两件事：
//! 1. [`init`]：release 下把日志写进 `gui.log`（追加写、每行 flush；debug 仍走 stderr）；
//! 2. [`install_panic_hook`]：panic 时把时间、位置、信息、回溯压进同一个 `gui.log`。
//!
//! 日志文件与 `config.toml` / `stutter.db` 同级（锚定 config.toml 所在目录，
//! 不跟随 CWD —— 双击 / explorer 启动时 CWD 并不是项目根），不写系统目录。

use std::io::Write;
use std::path::PathBuf;

/// 日志文件名
pub const LOG_FILE: &str = "gui.log";

/// init 实际选中的落点（panic 钩子复用同一个文件，避免写出两个文件）
static LOG_PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// config.toml 所在目录：从 exe 目录逐级向上找（与 `Config::load` 的回退一致）。
///
/// **全部运行时推导，没有任何硬编码绝对路径**：程序装到哪个目录，
/// 就以哪个目录为锚点。
fn config_dir() -> Option<PathBuf> {
    let me = std::env::current_exe().ok()?;
    let dir = me.parent()?;
    dir.ancestors()
        .find(|anc| anc.join("config.toml").exists())
        .map(|anc| anc.to_path_buf())
}

/// 候选落点，按优先级排列：
/// 1. config.toml 所在目录（与 stutter.db 同级，排查时一眼对照）
/// 2. exe 所在目录（打包布局：exe + config 同目录）
/// 3. 当前工作目录（最后兜底，等价旧行为）
///
/// 之所以要多个候选：装到 `C:\Program Files\...` 这类只读目录时，
/// 候选 1/2 打不开，必须能自动退到可写位置，而不是让日志静默消失。
fn log_candidates() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    if let Some(dir) = config_dir() {
        push(dir.join(LOG_FILE));
    }
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            push(dir.join(LOG_FILE));
        }
    }
    push(PathBuf::from(LOG_FILE));
    out
}

/// 依次尝试以「追加」方式打开候选路径，返回第一个可写的文件与路径。
///
/// 全部失败时返回 (None, 首选候选)：调用方照常运行，只是没有落盘日志。
fn open_first_writable() -> (Option<std::fs::File>, PathBuf) {
    let cands = log_candidates();
    for p in &cands {
        if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            return (Some(f), p.clone());
        }
    }
    (None, cands.into_iter().next().unwrap_or_else(|| PathBuf::from(LOG_FILE)))
}

/// 只写文件的日志落点。
///
/// **刻意不 tee 到 stderr**：release 下 GUI 已经 `FreeConsole`，stderr 无处可去；
/// 早期版本同时写 stderr 与文件，实测出现过「同一条记录在文件里出现两遍」
/// （同一 PID、同一秒、相邻两行），排查成本大于收益，故 release 只落盘。
struct FileSink {
    file: Option<std::fs::File>,
}

impl FileSink {
    fn from_file(file: Option<std::fs::File>) -> Self {
        Self { file }
    }
}

impl Write for FileSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Some(f) = self.file.as_mut() {
            let _ = f.write_all(buf);
            let _ = f.flush(); // 崩溃前必须落盘，不能留在缓冲区
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(f) = self.file.as_mut() {
            let _ = f.flush();
        }
        Ok(())
    }
}

/// 初始化日志：release 写 `gui.log`（锚定 config.toml 目录），debug 走 stderr。
///
/// 默认级别 `info`（GUI 关键动作：启动、菜单、托盘、窗口开关都在 info 级），
/// 可用 `RUST_LOG` 覆盖。重复初始化时静默跳过（`try_init` 语义）。
pub fn init() {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if cfg!(debug_assertions) {
        // 开发态保留控制台输出，改代码时直接看 stderr（不落盘，避免和文件打架）
        builder.write_style(env_logger::WriteStyle::Auto);
    } else {
        let (file, path) = open_first_writable();
        let _ = LOG_PATH.set(path);
        builder.target(env_logger::Target::Pipe(Box::new(FileSink::from_file(
            file,
        ))));
    }
    let _ = builder
        // 带上 PID：同一文件被多个实例追加时，能一眼看出是「重复写」还是「双实例」。
        .format(|buf, record| {
            writeln!(
                buf,
                "[{} PID={} {} {}] {}",
                buf.timestamp(),
                std::process::id(),
                record.level(),
                record.target(),
                record.args()
            )
        })
        .try_init();
}

/// 安装 panic 钩子：把崩溃现场追加到 `gui.log`，再交给默认钩子。
///
/// 记录内容：本地时间、panic 位置（文件:行:列）、panic 信息、完整回溯。
/// 写文件失败不影响默认行为（继续走 `prev`）。
pub fn install_panic_hook() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "未知位置".to_string());
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "非字符串 panic 负载".to_string()
        };
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
        // force_capture：不依赖 RUST_BACKTRACE 环境变量
        let bt = std::backtrace::Backtrace::force_capture();
        let text = format!(
            "[{ts}] PANIC 位置={loc}\n[{ts}] PANIC 信息={payload}\n[{ts}] 线程={:?}\n{bt}\n",
            std::thread::current().name()
        );
        // 与 logger 写同一个文件：优先用 init 选定的落点，未 init 时取首选候选
        let path = LOG_PATH
            .get()
            .cloned()
            .or_else(|| log_candidates().into_iter().next())
            .unwrap_or_else(|| PathBuf::from(LOG_FILE));
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = f.write_all(text.as_bytes());
            let _ = f.flush();
        }
        let _ = std::io::stderr().write_all(text.as_bytes());
        prev(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 候选列表非空、都以 gui.log 结尾，且首选是绝对路径（或最后兜底的相对名）
    #[test]
    fn candidates_named_gui_log() {
        let c = log_candidates();
        assert!(!c.is_empty());
        for p in &c {
            assert_eq!(p.file_name().unwrap(), LOG_FILE);
        }
        // 首选候选是运行时推导的绝对路径（config 目录 / exe 目录），
        // 只有两者都拿不到时才退回相对名 —— 绝不能是硬编码字符串。
        assert!(c[0].is_absolute() || c[0] == PathBuf::from(LOG_FILE));
    }

    /// 至少有一个候选可写（开发机上 config 目录 / exe 目录必然可写）
    #[test]
    fn at_least_one_candidate_writable() {
        let (f, p) = open_first_writable();
        assert!(f.is_some(), "所有候选都不可写: {:?}", log_candidates());
        assert!(p.exists(), "选定路径应已创建: {:?}", p);
    }

    /// 首选落点应跟随 exe 向上找到的 config.toml 目录，而不是固定字符串
    #[test]
    fn preferred_candidate_follows_exe_layout() {
        let c = log_candidates();
        if let Some(dir) = config_dir() {
            assert_eq!(c[0], dir.join(LOG_FILE));
        }
    }
}
