//! find-stutter-ui 可执行入口
//!
//! 主要的 `find-stutter` CLI 见 `crates/bin`；这个 bin 仅在直接 cargo run 该 crate 时使用。

use find_stutter_ui::run_overlay;

fn main() -> anyhow::Result<()> {
    env_logger::init();
    run_overlay()
}
