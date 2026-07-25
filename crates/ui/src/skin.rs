//! 皮肤配置：纯数据（颜色用 `#RRGGBB` 字符串存储），无 UI 框架依赖。
//!
//! 由 [`SkinConfig::load`] 从 `skins/<name>/skin.toml` 读取，反序列化后存为字符串，
//! 真正使用颜色时调用 [`crate::overlay::parse_color`] 解析为 `slint::Color`。

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
}

fn default_width() -> f32 { 260.0 }
fn default_height() -> f32 { 80.0 }
fn default_bg() -> String { "#1E1E2E".into() }
fn default_border() -> String { "#45475A".into() }
fn default_radius() -> f32 { 8.0 }
fn default_font_size() -> f32 { 13.0 }
fn default_upload_color() -> String { "#A6E3A1".into() }
fn default_download_color() -> String { "#89B4FA".into() }
fn default_cpu_color() -> String { "#F9E2AF".into() }
fn default_memory_color() -> String { "#F38BA8".into() }
fn default_gpu_color() -> String { "#CBA6F7".into() }
fn default_disk_color() -> String { "#94E2D5".into() }
fn default_label_color() -> String { "#BAC2DE".into() }

impl Default for SkinConfig {
    fn default() -> Self {
        Self {
            width: 260.0,
            height: 80.0,
            background_color: "#1E1E2E".into(),
            border_color: "#45475A".into(),
            border_radius: 8.0,
            font_size: 13.0,
            upload_color: "#A6E3A1".into(),
            download_color: "#89B4FA".into(),
            cpu_color: "#F9E2AF".into(),
            memory_color: "#F38BA8".into(),
            gpu_color: "#CBA6F7".into(),
            disk_color: "#94E2D5".into(),
            label_color: "#BAC2DE".into(),
        }
    }
}

impl SkinConfig {
    /// 从 `skins/<name>/skin.toml` 加载，文件不存在或解析失败返回默认皮肤
    pub fn load(name: &str) -> Self {
        let path = format!("skins/{}/skin.toml", name);
        if let Ok(content) = std::fs::read_to_string(&path) {
            toml::from_str(&content).unwrap_or_default()
        } else {
            Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_field_values() {
        let skin = SkinConfig::default();
        assert_eq!(skin.width, 260.0);
        assert_eq!(skin.height, 80.0);
        assert_eq!(skin.background_color, "#1E1E2E");
        assert_eq!(skin.border_color, "#45475A");
        assert_eq!(skin.border_radius, 8.0);
        assert_eq!(skin.font_size, 13.0);
        assert_eq!(skin.upload_color, "#A6E3A1");
        assert_eq!(skin.download_color, "#89B4FA");
        assert_eq!(skin.cpu_color, "#F9E2AF");
        assert_eq!(skin.memory_color, "#F38BA8");
        assert_eq!(skin.gpu_color, "#CBA6F7");
        assert_eq!(skin.disk_color, "#94E2D5");
        assert_eq!(skin.label_color, "#BAC2DE");
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
        assert_eq!(skin.background_color, "#1E1E2E");
    }
}
