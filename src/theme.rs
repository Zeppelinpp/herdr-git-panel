//! Read Herdr's theme rather than maintaining a separate plugin theme.
//!
//! Palette token data follows Herdr v0.9.5 src/app/state.rs; resolution order
//! follows src/app/mod.rs. See assets/README.md for provenance and limitations.
use anyhow::{Context, Result};
use ratatui::{
    buffer::Buffer,
    style::{Color, Modifier, Style},
};
use std::{collections::BTreeMap, env, fs, path::PathBuf};
use toml_edit::{DocumentMut, Item};

// Blue in a selected background denotes the host-accent navigation fill.
// It is resolved to the inherited palette before reaching the terminal.
pub const NAVIGATION: Style = Style::new()
    .bg(Color::Blue)
    .add_modifier(Modifier::REVERSED);

// Separate insertion cursors from selection/navigation fills. Resolve these
// markers at the end of apply(), including palettes that use ANSI tokens.
pub const INPUT_CURSOR: Style = Style::new()
    .fg(Color::Black)
    .bg(Color::LightBlue)
    .add_modifier(Modifier::REVERSED);

#[derive(Clone, Debug)]
pub struct Theme {
    pub name: String,
    pub background: Color,
    pub text: Color,
    pub muted: Color,
    pub border: Color,
    pub accent: Color,
    pub selection: Color,
    pub navigation: Color,
    pub branch: Color,
    pub green: Color,
    pub red: Color,
    pub yellow: Color,
    pub blue: Color,
    pub teal: Color,
    pub peach: Color,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Appearance {
    Dark,
    Light,
}
impl Default for Theme {
    fn default() -> Self {
        Self::from_config("", Appearance::Dark).expect("Bundled palette data is valid")
    }
}
impl Theme {
    pub fn config_path() -> PathBuf {
        env::var_os("HERDR_CONFIG_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(env::var_os("HOME").unwrap_or_default())
                    .join(".config/herdr/config.toml")
            })
    }
    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        let content = if path.exists() {
            fs::read_to_string(&path)
                .with_context(|| format!("Read theme from {}", path.display()))?
        } else {
            String::new()
        };
        // Herdr has no public resolved-palette API in 0.9.5. COLORFGBG supplies
        // an appearance hint when available; otherwise match its dark startup default.
        let appearance = env::var("COLORFGBG")
            .ok()
            .and_then(|s| s.rsplit(';').next()?.parse::<u8>().ok())
            .filter(|i| matches!(i, 7 | 15))
            .map(|_| Appearance::Light)
            .unwrap_or(Appearance::Dark);
        Self::from_config(&content, appearance)
    }
    pub fn from_config(content: &str, appearance: Appearance) -> Result<Self> {
        let config = content.parse::<DocumentMut>()?;
        let read = |section: &str, key: &str| {
            config
                .get(section)
                .and_then(|v| v.get(key))
                .and_then(Item::as_str)
        };
        let manual = canonical(read("theme", "name").unwrap_or("catppuccin"));
        let auto = config
            .get("theme")
            .and_then(|v| v.get("auto_switch"))
            .and_then(Item::as_bool)
            .unwrap_or(false);
        let mode = if appearance == Appearance::Light {
            "light"
        } else {
            "dark"
        };
        let selected = if auto {
            read("theme", &format!("{mode}_name"))
                .map(canonical)
                .unwrap_or_else(|| sibling(&manual, appearance))
        } else {
            manual
        };
        let palettes: BTreeMap<String, BTreeMap<String, String>> =
            serde_json::from_str(include_str!("../assets/herdr-palettes.json"))?;
        let fallback = if auto && appearance == Appearance::Light {
            "catppuccin-latte"
        } else {
            "catppuccin"
        };
        let name = if palettes.contains_key(&selected) {
            selected
        } else {
            fallback.to_string()
        };
        let mut tokens = palettes[&name].clone();
        let custom = config.get("theme").and_then(|t| t.get("custom"));
        if let Some(table) = custom.and_then(Item::as_table_like) {
            for (key, value) in table.iter() {
                if let Some(value) = value.as_str() {
                    tokens.insert(key.into(), value.into());
                }
            }
        }
        // Herdr's legacy UI accent only applies when shared custom.accent is unset.
        if custom
            .and_then(|v| v.get("accent"))
            .and_then(Item::as_str)
            .is_none()
        {
            if let Some(accent) = read("ui", "accent").filter(|v| *v != "cyan") {
                tokens.insert("accent".into(), accent.into());
            }
        }
        if auto {
            if let Some(table) = custom
                .and_then(|v| v.get(mode))
                .and_then(Item::as_table_like)
            {
                for (key, value) in table.iter() {
                    if let Some(value) = value.as_str() {
                        tokens.insert(key.into(), value.into());
                    }
                }
            }
        }
        let color = |key: &str| {
            parse_color(tokens.get(key).map(String::as_str).unwrap_or("reset"))
                .unwrap_or(Color::Cyan)
        };
        let explicit_selection = custom
            .and_then(|v| v.get("selection_bg"))
            .and_then(Item::as_str)
            .is_some()
            || (auto
                && custom
                    .and_then(|v| v.get(mode))
                    .and_then(|v| v.get("selection_bg"))
                    .and_then(Item::as_str)
                    .is_some());
        let navigation = if explicit_selection {
            color("selection_bg")
        } else {
            match (color("panel_bg"), color("accent")) {
                (Color::Rgb(r, g, b), Color::Rgb(ar, ag, ab)) => {
                    // A restrained accent wash: visible against the panel while
                    // keeping the host's normal text and graph colors readable.
                    let mix = |base: u8, accent: u8| {
                        ((u16::from(base) * 7 + u16::from(accent) * 3 + 5) / 10) as u8
                    };
                    Color::Rgb(mix(r, ar), mix(g, ag), mix(b, ab))
                }
                _ => color("selection_bg"),
            }
        };
        Ok(Self {
            name,
            background: color("panel_bg"),
            text: color("text"),
            muted: color("subtext0"),
            border: color("surface1"),
            accent: color("accent"),
            selection: color("selection_bg"),
            navigation,
            branch: color("mauve"),
            green: color("green"),
            red: color("red"),
            yellow: color("yellow"),
            blue: color("blue"),
            teal: color("teal"),
            peach: color("peach"),
        })
    }
    /// Widget styles use a small set of semantic ANSI roles internally. Resolve
    /// those roles after layout, so every popup/editor uses the same host palette.
    pub fn apply(&self, buffer: &mut Buffer) {
        let added = diff_background(self.background, self.green);
        let deleted = diff_background(self.background, self.red);
        for cell in &mut buffer.content {
            let semantic_color = matches!(
                cell.fg,
                Color::Blue
                    | Color::Magenta
                    | Color::Green
                    | Color::Red
                    | Color::Yellow
                    | Color::Cyan
                    | Color::LightBlue
                    | Color::LightMagenta
            );
            let dim = cell.modifier.contains(Modifier::DIM);
            let selected = cell.modifier.contains(Modifier::REVERSED);
            let navigation = selected && cell.bg == Color::Blue;
            let input_cursor = selected && cell.bg == Color::LightBlue && cell.fg == Color::Black;
            cell.fg = match cell.fg {
                Color::Reset => {
                    if dim {
                        self.muted
                    } else {
                        self.text
                    }
                }
                Color::Blue => self.accent,
                Color::Magenta => self.branch,
                Color::Green => self.green,
                Color::Red => self.red,
                Color::Yellow => self.yellow,
                Color::Cyan => self.teal,
                Color::LightBlue => self.blue,
                Color::LightMagenta => self.peach,
                Color::DarkGray => self.border,
                Color::Gray => self.muted,
                other => other,
            };
            // Red/green backgrounds denote a diff wash, while the foreground
            // keeps the normal semantic color. Terminal-default backgrounds
            // remain transparent when their RGB value is unknown.
            cell.bg = match cell.bg {
                Color::Green => added,
                Color::Red => deleted,
                Color::Reset => self.background,
                other => other,
            };
            if dim {
                cell.modifier.remove(Modifier::DIM);
            }
            if input_cursor {
                let fill = if self.accent != Color::Reset && self.accent != self.background {
                    self.accent
                } else {
                    self.text
                };
                if fill == Color::Reset || fill == self.background {
                    cell.fg = Color::Reset;
                    cell.bg = Color::Reset;
                    // Let the terminal invert its actual defaults when no
                    // explicit host colors are available.
                    cell.modifier.insert(Modifier::REVERSED);
                } else {
                    cell.bg = fill;
                    cell.fg = if self.background != Color::Reset {
                        self.background
                    } else {
                        match fill {
                            Color::Rgb(r, g, b)
                                if u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114
                                    >= 128_000 =>
                            {
                                Color::Black
                            }
                            Color::White
                            | Color::Gray
                            | Color::LightYellow
                            | Color::LightGreen
                            | Color::LightCyan => Color::Black,
                            _ => Color::White,
                        }
                    };
                    cell.modifier.remove(Modifier::REVERSED);
                }
                continue;
            }
            if selected {
                if !semantic_color {
                    cell.fg = self.text;
                }
                cell.bg = if navigation {
                    self.navigation
                } else {
                    self.selection
                };
                if cell.bg != Color::Reset {
                    cell.modifier.remove(Modifier::REVERSED);
                }
            }
        }
    }
}
fn diff_background(background: Color, tint: Color) -> Color {
    match (background, tint) {
        (Color::Rgb(r, g, b), Color::Rgb(tr, tg, tb)) => {
            let mix = |base: u8, tint: u8| ((u16::from(base) * 9 + u16::from(tint) + 5) / 10) as u8;
            Color::Rgb(mix(r, tr), mix(g, tg), mix(b, tb))
        }
        _ => background,
    }
}
fn canonical(name: &str) -> String {
    let n = name.to_lowercase().replace([' ', '_'], "-");
    match n.as_str() {
        "catppuccin-mocha" => "catppuccin",
        "latte" | "light" => "catppuccin-latte",
        "tokyonight" => "tokyo-night",
        "tokyo-day" | "tokyonight-day" => "tokyo-night-day",
        "gruvbox-dark" => "gruvbox",
        "onedark" => "one-dark",
        "onelight" => "one-light",
        "solarized-dark" => "solarized",
        "lotus" => "kanagawa-lotus",
        "rosepine" => "rose-pine",
        "rosepine-dawn" | "dawn" => "rose-pine-dawn",
        _ => &n,
    }
    .to_string()
}
fn sibling(name: &str, appearance: Appearance) -> String {
    let pairs = [
        ("catppuccin", "catppuccin-latte"),
        ("tokyo-night", "tokyo-night-day"),
        ("gruvbox", "gruvbox-light"),
        ("one-dark", "one-light"),
        ("solarized", "solarized-light"),
        ("kanagawa", "kanagawa-lotus"),
        ("rose-pine", "rose-pine-dawn"),
    ];
    pairs
        .iter()
        .find(|(d, l)| name == *d || name == *l)
        .map(|(d, l)| {
            if appearance == Appearance::Light {
                *l
            } else {
                *d
            }
        })
        .unwrap_or(name)
        .into()
}
pub fn parse_color(value: &str) -> Option<Color> {
    let value = value.trim().to_lowercase();
    if matches!(value.as_str(), "reset" | "default" | "none" | "transparent") {
        return Some(Color::Reset);
    }
    if let Some(hex) = value.strip_prefix('#').filter(|h| h.is_ascii()) {
        if hex.len() == 6 {
            return Some(Color::Rgb(
                u8::from_str_radix(&hex[0..2], 16).ok()?,
                u8::from_str_radix(&hex[2..4], 16).ok()?,
                u8::from_str_radix(&hex[4..6], 16).ok()?,
            ));
        }
        if hex.len() == 3 {
            let rgb = hex
                .chars()
                .map(|c| c.to_digit(16).map(|v| v as u8 * 17))
                .collect::<Option<Vec<_>>>()?;
            return Some(Color::Rgb(rgb[0], rgb[1], rgb[2]));
        }
    }
    if let Some(rgb) = value.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        let values = rgb
            .split(',')
            .map(|s| s.trim().parse::<u8>().ok())
            .collect::<Option<Vec<_>>>()?;
        if values.len() == 3 {
            return Some(Color::Rgb(values[0], values[1], values[2]));
        }
        return None;
    }
    Some(match value.as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" | "purple" => Color::Magenta,
        "cyan" => Color::Cyan,
        "white" => Color::White,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        _ => return None,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    #[test]
    fn diff_backgrounds_follow_the_host_and_keep_terminal_defaults() {
        for name in ["catppuccin", "one-light"] {
            let theme =
                Theme::from_config(&format!("[theme]\nname='{name}'"), Appearance::Dark).unwrap();
            let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
            buffer[(0, 0)].set_style(Style::new().fg(Color::Green).bg(Color::Green));
            buffer[(1, 0)].set_style(Style::new().fg(Color::Red).bg(Color::Red));
            theme.apply(&mut buffer);
            assert_ne!(buffer[(0, 0)].bg, theme.background);
            assert_ne!(buffer[(1, 0)].bg, theme.background);
            assert_ne!(buffer[(0, 0)].bg, buffer[(1, 0)].bg);
            assert_eq!(buffer[(0, 0)].fg, theme.green);
        }
        let theme = Theme::from_config("[theme]\nname='terminal'", Appearance::Dark).unwrap();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        buffer[(0, 0)].set_style(Style::new().fg(Color::Green).bg(Color::Green));
        theme.apply(&mut buffer);
        assert_eq!(buffer[(0, 0)].bg, Color::Reset);
        assert_eq!(buffer[(0, 0)].fg, Color::Green);
    }
    #[test]
    fn input_cursor_supports_ansi_tokens_and_terminal_default_fallback() {
        let theme = Theme::from_config(
            "[theme]\nname='terminal'\n[theme.custom]\npanel_bg='green'\naccent='blue'",
            Appearance::Dark,
        )
        .unwrap();
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        buffer[(0, 0)].set_style(INPUT_CURSOR);
        theme.apply(&mut buffer);
        assert_eq!(buffer[(0, 0)].bg, Color::Blue);
        assert_eq!(buffer[(0, 0)].fg, Color::Green);
        assert!(!buffer[(0, 0)].modifier.contains(Modifier::REVERSED));
        let theme = Theme::from_config(
            "[theme]\nname='terminal'\n[theme.custom]\naccent='reset'",
            Appearance::Dark,
        )
        .unwrap();
        buffer.reset();
        buffer[(0, 0)].set_style(INPUT_CURSOR);
        theme.apply(&mut buffer);
        assert_eq!(buffer[(0, 0)].bg, Color::Reset);
        assert!(buffer[(0, 0)].modifier.contains(Modifier::REVERSED));
    }
    #[test]
    fn inherits_vesper_custom_colors_and_legacy_ui_accent() {
        let theme=Theme::from_config("[theme]\nname='vesper'\n[theme.custom]\npanel_bg='#2D2A2E'\ntext='#E3E1E4'\nsubtext0='#939293'\nsurface1='#5B595C'\ngreen='#8FBF6E'\nred='#D98E96'\n[ui]\naccent='#9A8ED9'",Appearance::Dark).unwrap();
        assert_eq!(theme.name, "vesper");
        assert_eq!(theme.background, Color::Rgb(45, 42, 46));
        assert_eq!(theme.text, Color::Rgb(227, 225, 228));
        assert_eq!(theme.accent, Color::Rgb(154, 142, 217));
        assert_eq!(theme.selection, Color::Rgb(35, 35, 35));
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        buffer[(0, 0)].set_fg(Color::Green);
        buffer[(1, 0)].set_fg(Color::Red);
        buffer[(2, 0)].modifier = Modifier::DIM;
        buffer[(3, 0)].modifier = Modifier::REVERSED;
        theme.apply(&mut buffer);
        assert_eq!(buffer[(0, 0)].fg, Color::Rgb(143, 191, 110));
        assert_eq!(buffer[(1, 0)].fg, Color::Rgb(217, 142, 150));
        assert_eq!(buffer[(2, 0)].fg, Color::Rgb(147, 146, 147));
        assert_eq!(buffer[(3, 0)].bg, theme.selection);
        assert!(!buffer[(3, 0)].modifier.contains(Modifier::REVERSED));
        buffer[(0, 0)].set_fg(Color::Green);
        buffer[(0, 0)].modifier = Modifier::REVERSED;
        theme.apply(&mut buffer);
        assert_eq!(buffer[(0, 0)].fg, theme.green);
        assert_eq!(buffer[(0, 0)].bg, theme.selection);
    }
    #[test]
    fn mode_override_wins_and_shared_accent_suppresses_legacy_accent() {
        let config="[theme]\nname='nord'\nauto_switch=true\nlight_name='one-light'\n[theme.custom]\naccent='#123456'\n[theme.custom.light]\naccent='#abcdef'\n[ui]\naccent='red'";
        let dark = Theme::from_config(config, Appearance::Dark).unwrap();
        let light = Theme::from_config(config, Appearance::Light).unwrap();
        assert_eq!(dark.accent, Color::Rgb(18, 52, 86));
        assert_eq!(light.name, "one-light");
        assert_eq!(light.accent, Color::Rgb(171, 205, 239));
        assert_eq!(
            Theme::from_config(
                &config.replace("auto_switch=true", "auto_switch=false"),
                Appearance::Light
            )
            .unwrap()
            .accent,
            dark.accent
        );
    }
    #[test]
    fn terminal_theme_and_color_formats_match_herdr() {
        let t = Theme::from_config("[theme]\nname='terminal'", Appearance::Dark).unwrap();
        assert_eq!(t.background, Color::Reset);
        assert_eq!(t.text, Color::Reset);
        assert_eq!(t.green, Color::Green);
        assert_eq!(parse_color("rgb(1, 2, 3)"), Some(Color::Rgb(1, 2, 3)));
        assert_eq!(parse_color("#abc"), Some(Color::Rgb(170, 187, 204)));
        assert_eq!(parse_color("#你好"), None);
        assert_eq!(parse_color("transparent"), Some(Color::Reset));
        let catalog: BTreeMap<String, BTreeMap<String, String>> =
            serde_json::from_str(include_str!("../assets/herdr-palettes.json")).unwrap();
        assert_eq!(catalog.len(), 18);
        for tokens in catalog.values() {
            assert_eq!(tokens.len(), 19);
            for v in tokens.values() {
                assert!(parse_color(v).is_some(), "{v}");
            }
        }
    }
}
