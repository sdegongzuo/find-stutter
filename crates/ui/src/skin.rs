use egui::Color32;
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

fn parse_color(hex: &str) -> Color32 {
    let hex = hex.trim_start_matches('#');
    if hex.len() == 6 {
        let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
        let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
        let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
        Color32::from_rgb(r, g, b)
    } else {
        Color32::WHITE
    }
}

impl SkinConfig {
    pub fn bg_color(&self) -> Color32 { parse_color(&self.background_color) }
    pub fn border_color(&self) -> Color32 { parse_color(&self.border_color) }
    pub fn upload_color(&self) -> Color32 { parse_color(&self.upload_color) }
    pub fn download_color(&self) -> Color32 { parse_color(&self.download_color) }
    pub fn cpu_color(&self) -> Color32 { parse_color(&self.cpu_color) }
    pub fn memory_color(&self) -> Color32 { parse_color(&self.memory_color) }
    pub fn gpu_color(&self) -> Color32 { parse_color(&self.gpu_color) }
    pub fn disk_color(&self) -> Color32 { parse_color(&self.disk_color) }
    pub fn label_color(&self) -> Color32 { parse_color(&self.label_color) }
}

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

#[allow(dead_code)]
pub fn load_skin(name: &str) -> SkinConfig {
    let path = format!("skins/{}/skin.toml", name);
    if let Ok(content) = std::fs::read_to_string(&path) {
        toml::from_str(&content).unwrap_or_default()
    } else {
        SkinConfig::default()
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
    fn parse_color_red() {
        let skin = SkinConfig { upload_color: "#FF0000".into(), ..SkinConfig::default() };
        assert_eq!(skin.upload_color(), Color32::from_rgb(255, 0, 0));
    }

    #[test]
    fn parse_color_green() {
        let skin = SkinConfig { download_color: "#00FF00".into(), ..SkinConfig::default() };
        assert_eq!(skin.download_color(), Color32::from_rgb(0, 255, 0));
    }

    #[test]
    fn parse_color_blue() {
        let skin = SkinConfig { cpu_color: "#0000FF".into(), ..SkinConfig::default() };
        assert_eq!(skin.cpu_color(), Color32::from_rgb(0, 0, 255));
    }

    #[test]
    fn parse_color_invalid_returns_white() {
        let skin = SkinConfig { upload_color: "#XYZ".into(), ..SkinConfig::default() };
        assert_eq!(skin.upload_color(), Color32::WHITE);
    }

    #[test]
    fn parse_color_no_hash_prefix() {
        let skin = SkinConfig { memory_color: "FF00FF".into(), ..SkinConfig::default() };
        assert_eq!(skin.memory_color(), Color32::from_rgb(255, 0, 255));
    }

    #[test]
    fn bg_color_matches_default_hex() {
        let skin = SkinConfig::default();
        assert_eq!(skin.bg_color(), Color32::from_rgb(0x1E, 0x1E, 0x2E));
    }

    #[test]
    fn border_color_matches_default_hex() {
        let skin = SkinConfig::default();
        assert_eq!(skin.border_color(), Color32::from_rgb(0x45, 0x47, 0x5A));
    }

    #[test]
    fn upload_color_matches_default_hex() {
        let skin = SkinConfig::default();
        assert_eq!(skin.upload_color(), Color32::from_rgb(0xA6, 0xE3, 0xA1));
    }

    #[test]
    fn download_color_matches_default_hex() {
        let skin = SkinConfig::default();
        assert_eq!(skin.download_color(), Color32::from_rgb(0x89, 0xB4, 0xFA));
    }

    #[test]
    fn cpu_color_matches_default_hex() {
        let skin = SkinConfig::default();
        assert_eq!(skin.cpu_color(), Color32::from_rgb(0xF9, 0xE2, 0xAF));
    }

    #[test]
    fn memory_color_matches_default_hex() {
        let skin = SkinConfig::default();
        assert_eq!(skin.memory_color(), Color32::from_rgb(0xF3, 0x8B, 0xA8));
    }

    #[test]
    fn gpu_color_matches_default_hex() {
        let skin = SkinConfig::default();
        assert_eq!(skin.gpu_color(), Color32::from_rgb(0xCB, 0xA6, 0xF7));
    }

    #[test]
    fn disk_color_matches_default_hex() {
        let skin = SkinConfig::default();
        assert_eq!(skin.disk_color(), Color32::from_rgb(0x94, 0xE2, 0xD5));
    }

    #[test]
    fn label_color_matches_default_hex() {
        let skin = SkinConfig::default();
        assert_eq!(skin.label_color(), Color32::from_rgb(0xBA, 0xC2, 0xDE));
    }

    #[test]
    fn load_nonexistent_skin_returns_default() {
        let skin = load_skin("this_skin_definitely_does_not_exist_12345");
        let default = SkinConfig::default();
        assert_eq!(skin.width, default.width);
        assert_eq!(skin.height, default.height);
        assert_eq!(skin.background_color, default.background_color);
        assert_eq!(skin.font_size, default.font_size);
        assert_eq!(skin.upload_color, default.upload_color);
    }
}
