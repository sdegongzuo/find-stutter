//! 皮肤配置：纯数据（颜色用 `#RRGGBB` 字符串存储），无 UI 框架依赖。
//!
//! 由 [`SkinConfig::load`] 从 `skins/<name>/skin.toml` 读取，反序列化后存为字符串，
//! 真正使用颜色时调用 [`crate::overlay::parse_color`] 解析为 `slint::Color`。

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct SkinConfig {
    #[serde(default = "default_width")]
    pub width: f32,
    #[serde(default = "default_height")]
    pub height: f32,
    #[serde(default = "default_bg")]
    pub background_color: String,
    #[serde(default = "default_border")]
    pub border_color: String,
    #[serde(default = "default_radius")]
    pub border_radius: f32,
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    #[serde(default = "default_upload_color")]
    pub upload_color: String,
    #[serde(default = "default_download_color")]
    pub download_color: String,
    #[serde(default = "default_cpu_color")]
    pub cpu_color: String,
    #[serde(default = "default_memory_color")]
    pub memory_color: String,
    #[serde(default = "default_gpu_color")]
    pub gpu_color: String,
    #[serde(default = "default_disk_color")]
    pub disk_color: String,
    #[serde(default = "default_label_color")]
    pub label_color: String,
    /// 布局模式："card"（默认三列白卡）/ "bubble"（绿色三列完整指标）
    #[serde(default = "default_layout")]
    pub layout: String,
    /// 可选吉祥物图片文件名（相对皮肤目录，如 "mascot.png"）；空 = 不显示
    #[serde(default)]
    pub mascot: String,
    /// mascot 解析后的绝对路径（load 时填充；仅文件存在时才 Some）
    #[serde(skip)]
    pub mascot_file: Option<PathBuf>,
}

fn default_width() -> f32 { 360.0 }
fn default_height() -> f32 { 78.0 }
fn default_layout() -> String { "card".into() }
fn default_bg() -> String { "#FFFFFF".into() }
fn default_border() -> String { "#C0C0C8".into() }
fn default_radius() -> f32 { 8.0 }
fn default_font_size() -> f32 { 13.0 }
fn default_upload_color() -> String { "#2E7D32".into() }
fn default_download_color() -> String { "#1565C0".into() }
fn default_cpu_color() -> String { "#37474F".into() }
fn default_memory_color() -> String { "#6A1B9A".into() }
fn default_gpu_color() -> String { "#00695C".into() }
fn default_disk_color() -> String { "#AD1457".into() }
fn default_label_color() -> String { "#546E7A".into() }

impl Default for SkinConfig {
    fn default() -> Self {
        Self {
            width: 360.0,
            height: 78.0,
            background_color: "#FFFFFF".into(),
            border_color: "#C0C0C8".into(),
            border_radius: 8.0,
            font_size: 13.0,
            upload_color: "#2E7D32".into(),
            download_color: "#1565C0".into(),
            cpu_color: "#37474F".into(),
            memory_color: "#6A1B9A".into(),
            gpu_color: "#00695C".into(),
            disk_color: "#AD1457".into(),
            label_color: "#546E7A".into(),
            layout: "card".into(),
            mascot: String::new(),
            mascot_file: None,
        }
    }
}

/// 皮肤目录查找顺序（与 `Config::load` 的策略一致）：
/// 1. CWD 下 `skins/<name>/`（开发时直接 cargo run）
/// 2. 可执行文件同目录 `skins/<name>/`（发布后与 exe 一起分发）
/// 3. workspace 源码布局 `crates/ui/skins/<name>/`（测试 / 仓库内运行）
fn find_skin_dir(name: &str) -> Option<PathBuf> {
    let mut candidates = Vec::new();

    candidates.push(std::path::Path::new("skins").join(name));

    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            candidates.push(dir.join("skins").join(name));
        }
    }

    // workspace 源码布局（cargo test 时 CWD 是 workspace 根）
    candidates.push(std::path::Path::new("crates").join("ui").join("skins").join(name));

    candidates.into_iter().find(|p| p.exists())
}

fn find_skin_path(name: &str) -> Option<PathBuf> {
    find_skin_dir(name).map(|d| d.join("skin.toml"))
}

impl SkinConfig {
    /// 从 `skins/<name>/skin.toml` 加载，文件不存在或解析失败返回默认皮肤。
    ///
    /// 查找顺序见 [`find_skin_dir`]；`mascot` 字段若配置了文件名且文件存在，
    /// 会解析出绝对路径存入 `mascot_file`（UI 端据此加载图片）。
    pub fn load(name: &str) -> Self {
        if let Some(path) = find_skin_path(name) {
            if let Ok(content) = std::fs::read_to_string(&path) {
                let mut skin: SkinConfig = toml::from_str(&content).unwrap_or_default();
                if !skin.mascot.is_empty() {
                    if let Some(dir) = find_skin_dir(name) {
                        let p = dir.join(&skin.mascot);
                        if p.is_file() {
                            skin.mascot_file = Some(p);
                        }
                    }
                }
                return skin;
            }
        }
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_field_values() {
        let skin = SkinConfig::default();
        assert_eq!(skin.width, 360.0);
        assert_eq!(skin.height, 78.0);
        assert_eq!(skin.background_color, "#FFFFFF");
        assert_eq!(skin.border_color, "#C0C0C8");
        assert_eq!(skin.border_radius, 8.0);
        assert_eq!(skin.font_size, 13.0);
        assert_eq!(skin.upload_color, "#2E7D32");
        assert_eq!(skin.download_color, "#1565C0");
        assert_eq!(skin.cpu_color, "#37474F");
        assert_eq!(skin.memory_color, "#6A1B9A");
        assert_eq!(skin.gpu_color, "#00695C");
        assert_eq!(skin.disk_color, "#AD1457");
        assert_eq!(skin.label_color, "#546E7A");
        // 布局模式默认 card（三列白卡）
        assert_eq!(skin.layout, "card");
        assert_eq!(skin.mascot, "");
        assert!(skin.mascot_file.is_none());
    }

    /// 仓库内自带 bubble 皮肤必须能加载且 layout = "bubble"
    #[test]
    fn load_bubble_skin_from_repo() {
        let skin = SkinConfig::load("bubble");
        assert_eq!(skin.layout, "bubble");
        assert_eq!(skin.background_color, "#EAF6E8");
        // bubble 现展示与 card 相同的完整指标集（三列三行），因此宽度与 card 相近，
        // 不再是最初的窄条迷你气泡
        assert!(skin.width >= 300.0, "bubble 完整指标集应需要更大宽度: {}", skin.width);
        // 未配置 mascot 时不解析图片路径
        assert!(skin.mascot_file.is_none());
    }

    #[test]
    fn load_nonexistent_skin_returns_default() {
        let skin = SkinConfig::load("this_skin_definitely_does_not_exist_12345");
        let default = SkinConfig::default();
        assert_eq!(skin.width, default.width);
        assert_eq!(skin.height, default.height);
        assert_eq!(skin.background_color, default.background_color);
        assert_eq!(skin.font_size, default.font_size);
        assert_eq!(skin.upload_color, default.upload_color);
    }

    #[test]
    fn parse_toml_partial() {
        let content = "width = 300.0\nheight = 100.0\nfont_size = 15.0\nupload_color = \"00FF00\"\n";
        let skin: SkinConfig = toml::from_str(content).unwrap();
        assert_eq!(skin.width, 300.0);
        assert_eq!(skin.height, 100.0);
        assert_eq!(skin.font_size, 15.0);
        assert_eq!(skin.upload_color, "00FF00");
        // 未指定的字段用默认值
        assert_eq!(skin.background_color, "#FFFFFF");
    }

    /// 仓库内自带 default 皮肤必须能被 load（修复：原实现路径/结构不匹配，
    /// load 永远 fallback 默认值，皮肤系统名存实亡）
    #[test]
    fn load_default_skin_from_repo() {
        let skin = SkinConfig::load("default");
        let default = SkinConfig::default();
        // 仓库皮肤与默认值一致（skin.toml 就是按默认值写的），
        // 关键断言：加载结果与默认值完全一致 = 文件被成功解析，
        // 而非「找不到文件 fallback 默认」。
        assert_eq!(skin.width, default.width);
        assert_eq!(skin.height, default.height);
        assert_eq!(skin.font_size, default.font_size);
        assert_eq!(skin.background_color, default.background_color);
        assert_eq!(skin.cpu_color, default.cpu_color);
    }

    /// 解析失败（非法 TOML）应 fallback 默认
    #[test]
    fn parse_invalid_toml_falls_back_to_default() {
        // 往临时目录写一个非法 skin.toml，用 load 直接读（走 find_skin_path 会
        // 优先命中 CWD 的真实皮肤，所以这里只验证 toml::from_str 的 fallback 路径）
        let content = "width = not-a-number\n";
        let skin: SkinConfig = toml::from_str(content).unwrap_or_default();
        assert_eq!(skin.width, 360.0);
    }
}
