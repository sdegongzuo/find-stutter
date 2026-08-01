//! find-stutter-ui 可执行入口
//!
//! 主要的 `find-stutter` CLI 见 `crates/bin`；这个 bin 仅在直接 cargo run 该 crate 时使用。

use find_stutter_ui::run;

fn main() -> anyhow::Result<()> {
    // 日志由 find_stutter_ui::run() 内部 init（用 try_init 容忍重复），
    // 这里不再 init，否则 lib.rs 二次 init 会 panic。
    run()
}
