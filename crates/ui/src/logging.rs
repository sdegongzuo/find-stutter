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
//! 1. [`init`]：把日志 tee 到 stderr 与 `gui.log`（追加写，跨次运行保留）；
//! 2. [`install_panic_hook`]：panic 时把时间、位置、信息、回溯压进同一个 `gui.log`。
//!
//! 日志文件与 `config.toml` / `stutter.db` 同级（工作目录），不写系统目录。

use std::io::Write;
use std::path::Path;

/// 日志文件（相对工作目录）
pub const LOG_FILE: &str = "gui.log";

/// 同时写 stderr 与文件：stderr 给 debug 构建 / 重定向场景，文件给 release 事后取证。
struct Tee {
    file: Option<std::fs::File>,
}

impl Tee {
    fn new(path: &Path) -> Self {
        // 打开失败也要能跑：退化成只写 stderr
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok();
        Self { file }
    }
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::stderr().write_all(buf);
        if let Some(f) = self.file.as_mut() {
            let _ = f.write_all(buf);
            let _ = f.flush(); // 崩溃前必须落盘，不能留在缓冲区
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let _ = std::io::stderr().flush();
        if let Some(f) = self.file.as_mut() {
            let _ = f.flush();
        }
        Ok(())
    }
}

/// 初始化日志：stderr + `gui.log` 双写。
///
/// 默认级别 `info`（GUI 关键动作：启动、菜单、托盘、窗口开关都在 info 级），
/// 可用 `RUST_LOG` 覆盖。重复初始化时静默跳过（`try_init` 语义）。
pub fn init() {
    let _ = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    )
    .target(env_logger::Target::Pipe(Box::new(Tee::new(Path::new(
        LOG_FILE,
    )))))
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
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(LOG_FILE)
        {
            let _ = f.write_all(text.as_bytes());
            let _ = f.flush();
        }
        let _ = std::io::stderr().write_all(text.as_bytes());
        prev(info);
    }));
}
