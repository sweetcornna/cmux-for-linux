//! Lenient frontend configuration.
//!
//! The shared TUI document is read as untyped JSON because this frontend only
//! owns a few presentation fields and must not reject newer TUI settings.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

const DEFAULT_FONT: &str = "monospace 11";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn cairo(self) -> (f64, f64, f64) {
        (
            f64::from(self.0) / 255.0,
            f64::from(self.1) / 255.0,
            f64::from(self.2) / 255.0,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub font: String,
    pub border_active: Rgb,
    pub border_inactive: Rgb,
    pub selection_background: Rgb,
    pub selection_foreground: Option<Rgb>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font: DEFAULT_FONT.to_string(),
            border_active: xterm_color(110),
            border_inactive: xterm_color(238),
            selection_background: Rgb(0x3a, 0x3a, 0x3a),
            selection_foreground: None,
        }
    }
}

#[derive(Default)]
struct Environment {
    tui_config: Option<PathBuf>,
    mux_config: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    home: Option<PathBuf>,
    font: Option<String>,
}

impl Environment {
    fn current() -> Self {
        Self {
            tui_config: env_path("CMUX_TUI_CONFIG"),
            mux_config: env_path("CMUX_MUX_CONFIG"),
            xdg_config_home: env_path("XDG_CONFIG_HOME"),
            home: env_path("HOME"),
            font: std::env::var("CMUX_GTK_FONT")
                .ok()
                .filter(|font| !font.trim().is_empty()),
        }
    }

    fn config_dir(&self) -> Option<PathBuf> {
        self.xdg_config_home
            .as_ref()
            .map(|path| path.join("cmux"))
            .or_else(|| self.home.as_ref().map(|path| path.join(".config/cmux")))
    }
}

pub fn load() -> Settings {
    let environment = Environment::current();
    let mut settings = Settings::default();

    if let Some(path) = tui_config_path(&environment, Path::exists) {
        if let Some(document) = read_json(&path) {
            apply_tui_theme(&mut settings, &document);
        }
    }
    if let Some(path) = gtk_config_path(&environment) {
        if let Some(document) = read_json(&path) {
            apply_gtk_config(&mut settings, &document);
        }
    }
    if let Some(font) = environment.font {
        settings.font = font;
    }
    settings
}

fn read_json(path: &Path) -> Option<Value> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn apply_tui_theme(settings: &mut Settings, document: &Value) {
    let Some(theme) = document.get("theme").and_then(Value::as_object) else {
        return;
    };
    apply_color(theme.get("border_active"), &mut settings.border_active);
    apply_color(theme.get("border_inactive"), &mut settings.border_inactive);
    apply_color(
        theme.get("selection_background"),
        &mut settings.selection_background,
    );
    if let Some(value) = theme.get("selection_foreground") {
        if value.is_null() {
            settings.selection_foreground = None;
        } else if let Some(color) = parse_color(value) {
            settings.selection_foreground = Some(color);
        }
    }
}

fn apply_gtk_config(settings: &mut Settings, document: &Value) {
    if let Some(font) = document
        .get("font")
        .and_then(Value::as_str)
        .filter(|font| !font.trim().is_empty())
    {
        settings.font = font.to_string();
    }
}

fn apply_color(value: Option<&Value>, target: &mut Rgb) {
    if let Some(color) = value.and_then(parse_color) {
        *target = color;
    }
}

fn parse_color(value: &Value) -> Option<Rgb> {
    match value {
        Value::Number(index) => u8::try_from(index.as_u64()?).ok().map(xterm_color),
        Value::String(value) => parse_color_string(value),
        _ => None,
    }
}

fn parse_color_string(value: &str) -> Option<Rgb> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        if !hex.is_ascii() {
            return None;
        }
        return match hex.len() {
            3 => {
                let mut digits = hex.chars().map(|digit| digit.to_digit(16).map(|n| n as u8));
                let red = digits.next()??;
                let green = digits.next()??;
                let blue = digits.next()??;
                Some(Rgb(red * 17, green * 17, blue * 17))
            }
            6 => Some(Rgb(
                u8::from_str_radix(&hex[0..2], 16).ok()?,
                u8::from_str_radix(&hex[2..4], 16).ok()?,
                u8::from_str_radix(&hex[4..6], 16).ok()?,
            )),
            _ => None,
        };
    }
    value.parse::<u8>().ok().map(xterm_color)
}

fn xterm_color(index: u8) -> Rgb {
    const ANSI: [Rgb; 16] = [
        Rgb(0, 0, 0),
        Rgb(128, 0, 0),
        Rgb(0, 128, 0),
        Rgb(128, 128, 0),
        Rgb(0, 0, 128),
        Rgb(128, 0, 128),
        Rgb(0, 128, 128),
        Rgb(192, 192, 192),
        Rgb(128, 128, 128),
        Rgb(255, 0, 0),
        Rgb(0, 255, 0),
        Rgb(255, 255, 0),
        Rgb(0, 0, 255),
        Rgb(255, 0, 255),
        Rgb(0, 255, 255),
        Rgb(255, 255, 255),
    ];
    const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

    match index {
        0..=15 => ANSI[usize::from(index)],
        16..=231 => {
            let offset = index - 16;
            Rgb(
                CUBE[usize::from(offset / 36)],
                CUBE[usize::from((offset % 36) / 6)],
                CUBE[usize::from(offset % 6)],
            )
        }
        232..=255 => {
            let level = 8 + (index - 232) * 10;
            Rgb(level, level, level)
        }
    }
}

fn tui_config_path(environment: &Environment, exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    if let Some(path) = environment
        .tui_config
        .as_ref()
        .or(environment.mux_config.as_ref())
    {
        return Some(path.clone());
    }
    let directory = environment.config_dir()?;
    let preferred = directory.join("cmux-tui.json");
    if exists(&preferred) {
        return Some(preferred);
    }
    let legacy = directory.join("mux.json");
    Some(if exists(&legacy) { legacy } else { preferred })
}

fn gtk_config_path(environment: &Environment) -> Option<PathBuf> {
    environment
        .config_dir()
        .map(|directory| directory.join("cmux-gtk.json"))
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(nonempty).map(PathBuf::from)
}

fn nonempty(value: &OsString) -> bool {
    !value.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_xterm_ansi_cube_and_grayscale_colors() {
        assert_eq!(xterm_color(1), Rgb(128, 0, 0));
        assert_eq!(xterm_color(16), Rgb(0, 0, 0));
        assert_eq!(xterm_color(110), Rgb(135, 175, 215));
        assert_eq!(xterm_color(231), Rgb(255, 255, 255));
        assert_eq!(xterm_color(232), Rgb(8, 8, 8));
        assert_eq!(xterm_color(255), Rgb(238, 238, 238));
        assert_eq!(
            parse_color(&Value::String("#3af".into())),
            Some(Rgb(51, 170, 255))
        );
        assert_eq!(
            parse_color(&Value::String("#123456".into())),
            Some(Rgb(18, 52, 86))
        );
        assert_eq!(
            parse_color(&Value::String("110".into())),
            Some(Rgb(135, 175, 215))
        );
    }

    #[test]
    fn resolves_tui_config_in_documented_order() {
        let environment = Environment {
            tui_config: Some("/explicit/tui.json".into()),
            mux_config: Some("/explicit/mux.json".into()),
            xdg_config_home: Some("/xdg".into()),
            home: Some("/home/user".into()),
            font: None,
        };
        assert_eq!(
            tui_config_path(&environment, |_| false),
            Some("/explicit/tui.json".into())
        );

        let environment = Environment {
            tui_config: None,
            ..environment
        };
        assert_eq!(
            tui_config_path(&environment, |_| false),
            Some("/explicit/mux.json".into())
        );

        let environment = Environment {
            mux_config: None,
            ..environment
        };
        assert_eq!(
            tui_config_path(&environment, |path| path.ends_with("mux.json")),
            Some("/xdg/cmux/mux.json".into())
        );
        assert_eq!(
            gtk_config_path(&environment),
            Some("/xdg/cmux/cmux-gtk.json".into())
        );

        let environment = Environment {
            xdg_config_home: None,
            ..environment
        };
        assert_eq!(
            tui_config_path(&environment, |_| false),
            Some("/home/user/.config/cmux/cmux-tui.json".into())
        );
    }

    #[test]
    fn ignores_unknown_and_invalid_theme_values() {
        let mut settings = Settings::default();
        let document = serde_json::json!({
            "theme": {
                "border_active": "#abc",
                "border_inactive": 999,
                "selection_background": false,
                "selection_foreground": "196",
                "future_tui_key": {"anything": true}
            },
            "unknown_section": [1, 2, 3]
        });
        apply_tui_theme(&mut settings, &document);
        assert_eq!(settings.border_active, Rgb(170, 187, 204));
        assert_eq!(settings.border_inactive, xterm_color(238));
        assert_eq!(settings.selection_background, Rgb(0x3a, 0x3a, 0x3a));
        assert_eq!(settings.selection_foreground, Some(Rgb(255, 0, 0)));
    }
}
