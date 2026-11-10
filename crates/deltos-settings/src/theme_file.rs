//! Custom themes: the built-in light or dark colors as a base, with any of them replaced.
//! Theme files are TOML in the themes folder; docs/THEMES.md describes every key.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One themeable color: its key in `[colors]` and what it paints.
#[derive(Debug, Clone, Copy)]
pub struct ColorKey {
    pub key: &'static str,
    pub label: &'static str,
}

/// Every key `[colors]` accepts, in the order the docs list them.
pub const COLOR_KEYS: &[ColorKey] = &[
    ColorKey {
        key: "base",
        label: "Window background and tab strip",
    },
    ColorKey {
        key: "title_bar",
        label: "Title bar",
    },
    ColorKey {
        key: "sidebar",
        label: "Sidebars and panels",
    },
    ColorKey {
        key: "surface",
        label: "Grid and editor background",
    },
    ColorKey {
        key: "surface_alt",
        label: "Striped grid rows",
    },
    ColorKey {
        key: "grid_header",
        label: "Grid column headers",
    },
    ColorKey {
        key: "floating",
        label: "Menus, popovers and tooltips",
    },
    ColorKey {
        key: "input",
        label: "Text fields",
    },
    ColorKey {
        key: "hover",
        label: "Items under the pointer",
    },
    ColorKey {
        key: "selected",
        label: "Selected list items and rows",
    },
    ColorKey {
        key: "selection",
        label: "Selected grid cells",
    },
    ColorKey {
        key: "border",
        label: "Separators between panels",
    },
    ColorKey {
        key: "grid_line",
        label: "Lines between grid cells",
    },
    ColorKey {
        key: "text",
        label: "Text",
    },
    ColorKey {
        key: "text_muted",
        label: "Secondary text",
    },
    ColorKey {
        key: "text_faint",
        label: "Hints, placeholders and NULL",
    },
    ColorKey {
        key: "accent",
        label: "Accent; replaces the accent setting",
    },
    ColorKey {
        key: "accent_text",
        label: "Text on the accent",
    },
    ColorKey {
        key: "danger",
        label: "Errors and destructive actions",
    },
    ColorKey {
        key: "warning",
        label: "Warnings",
    },
    ColorKey {
        key: "success",
        label: "Success and connected state",
    },
    ColorKey {
        key: "syntax_keyword",
        label: "SQL keywords",
    },
    ColorKey {
        key: "syntax_string",
        label: "Strings in the editor",
    },
    ColorKey {
        key: "syntax_number",
        label: "Numbers in the editor",
    },
    ColorKey {
        key: "syntax_comment",
        label: "Comments in the editor",
    },
];

/// Which built-in colors a theme starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeBase {
    #[default]
    Light,
    Dark,
}

/// A custom theme file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeFile {
    pub name: String,
    pub author: String,
    pub base: ThemeBase,
    /// Colors as `#rrggbb` or `#rrggbbaa`, by key in [`COLOR_KEYS`].
    pub colors: BTreeMap<String, String>,
    /// Corner radius, 0 to 10; replaces the setting when set.
    pub radius: Option<f32>,
}

impl Default for ThemeFile {
    fn default() -> Self {
        Self {
            name: "My theme".into(),
            author: String::new(),
            base: ThemeBase::Light,
            colors: BTreeMap::new(),
            radius: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ThemeError {
    #[error("could not read the theme: {0}")]
    Io(#[from] std::io::Error),
    #[error("the theme file is not valid TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("could not write the theme: {0}")]
    Write(#[from] toml::ser::Error),
}

/// Parses `#rgb`, `#rrggbb` or `#rrggbbaa` into 0xRRGGBBAA.
pub fn parse_color(text: &str) -> Option<u32> {
    let hex = text.trim().trim_start_matches('#');
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    match hex.len() {
        3 => {
            let (r, g, b) = ((value >> 8) & 0xf, (value >> 4) & 0xf, value & 0xf);
            Some(((r * 17) << 24) | ((g * 17) << 16) | ((b * 17) << 8) | 0xff)
        }
        6 => Some((value << 8) | 0xff),
        8 => Some(value),
        _ => None,
    }
}

impl ThemeFile {
    pub fn from_toml(text: &str) -> Result<Self, ThemeError> {
        let mut theme: Self = toml::from_str(text)?;
        theme.sanitize();
        Ok(theme)
    }

    pub fn to_toml(&self) -> Result<String, ThemeError> {
        Ok(format!(
            "# Deltos theme. Every key is described in docs/THEMES.md.\n{}",
            toml::to_string_pretty(self)?
        ))
    }

    pub fn load(path: &std::path::Path) -> Result<Self, ThemeError> {
        Self::from_toml(&std::fs::read_to_string(path)?)
    }

    /// The color set for `key`, as 0xRRGGBBAA.
    pub fn color(&self, key: &str) -> Option<u32> {
        self.colors.get(key).and_then(|c| parse_color(c))
    }

    /// Drops unknown keys and unreadable colors, and clamps the radius.
    pub fn sanitize(&mut self) {
        self.colors
            .retain(|k, v| COLOR_KEYS.iter().any(|c| c.key == k) && parse_color(v).is_some());
        self.radius = self
            .radius
            .filter(|r| r.is_finite())
            .map(|r| r.clamp(0.0, 10.0));
        if self.name.trim().is_empty() {
            self.name = "Untitled theme".into();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_colors() {
        assert_eq!(parse_color("#fff"), Some(0xffffffff));
        assert_eq!(parse_color("2F7CF6"), Some(0x2f7cf6ff));
        assert_eq!(parse_color("#00000080"), Some(0x00000080));
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color("red"), None);
        assert_eq!(parse_color("+12"), None);
    }

    #[test]
    fn round_trips_and_sanitizes() {
        let text = r##"
            name = "Nord"
            base = "dark"
            radius = 99
            [colors]
            accent = "#88c0d0"
            surface = "#2e3440"
            nonsense = "#fff"
            text = "blue"
        "##;
        let theme = ThemeFile::from_toml(text).unwrap();
        assert_eq!(theme.base, ThemeBase::Dark);
        assert_eq!(theme.radius, Some(10.0));
        assert_eq!(theme.colors.len(), 2);
        assert_eq!(theme.color("accent"), Some(0x88c0d0ff));
        let again = ThemeFile::from_toml(&theme.to_toml().unwrap()).unwrap();
        assert_eq!(again, theme);
    }
}
