//! 把 .slint 文件编译进 crate
//!
//! `include_modules!()` 宏依赖 `SLINT_INCLUDE_GENERATED` 环境变量（由 `slint_build::compile` 设置）。
fn main() {
    slint_build::compile("ui/overlay.slint").expect("slint compilation failed");
}
