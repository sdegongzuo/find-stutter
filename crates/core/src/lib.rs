pub mod collector;
pub mod detector;
pub mod logger;
pub mod software_root_cause;
pub mod stack_sampler;
pub use stack_sampler::StackSampler;
pub mod types;
pub mod win32;

// ADR-0001：卡顿分析聚合（KPI/元凶榜/因果链等）下沉 core，UI 与 CLI 共用同一份分析口径。
pub mod analytics;
// ADR-0001：UAC 提权封装下沉 core（无界面依赖），ui（auto_start）与 cli（upgrade）共用。
pub mod elevate;

pub use types::*;
pub use logger::Logger;
pub use collector::Collector;
pub use detector::Detector;
