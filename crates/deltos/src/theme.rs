//! Colors and metrics, derived from the appearance settings. Rebuilt only when a setting
//! or the system appearance changes, then read by every view as a global.
//!
//! The two built-in palettes follow the quiet look of native database tools: neutral
//! light gray or dark surfaces, one accent, and thin separators whose strength the
//! border contrast setting scales.

use deltos_settings::{
    Appearance, COLOR_KEYS, DateFormat, Editor, NullStyle, ThemeBase, ThemeFile, ThemeMode,
};
use gpui::{App, Global, Hsla, Pixels, Rgba, SharedString, px, rgb, rgba};

#[derive(Debug, Clone)]
pub struct Theme {
    /// Behind everything; the tab strip.
    pub base: Hsla,
    pub title_bar: Hsla,
    /// Sidebars and tool panels.
    pub sidebar: Hsla,
    /// The grid and the editor.
    pub surface: Hsla,
    /// Every other grid row when stripes are on.
    pub surface_alt: Hsla,
    pub grid_header: Hsla,
    pub floating: Hsla,
    pub input: Hsla,
    pub hover: Hsla,
    pub selected: Hsla,
    /// Selected grid cells.
    pub selection: Hsla,
    /// The row that holds the selection.
    pub selected_row: Hsla,
    pub border: Hsla,
    /// A stronger line, for field outlines.
    pub border_strong: Hsla,
    pub grid_line: Hsla,
    pub text: Hsla,
    pub text_muted: Hsla,
    pub text_faint: Hsla,
    pub accent: Hsla,
    pub accent_hover: Hsla,
    pub accent_text: Hsla,
    /// The accent at low strength: drop zones, focus rings.
    pub accent_soft: Hsla,
    pub danger: Hsla,
    pub warning: Hsla,
    pub success: Hsla,
    pub syntax_keyword: Hsla,
    pub syntax_string: Hsla,
    pub syntax_number: Hsla,
    pub syntax_comment: Hsla,
    pub radius: Pixels,
    pub radius_small: Pixels,
    pub ui_font: SharedString,
    pub mono_font: SharedString,
    pub editor_font: SharedString,
    /// Interface text size.
    pub ui_size: Pixels,
    /// Small labels: headers, the status bar.
    pub small_size: Pixels,
    pub mono_size: Pixels,
    pub editor_size: Pixels,
    /// Grid row height.
    pub row_height: Pixels,
    /// Sidebar and list row height.
    pub list_row: Pixels,
    pub tab_height: Pixels,
    pub bar_height: Pixels,
    pub zebra: bool,
    pub grid_lines: bool,
    pub null_text: &'static str,
    pub date_format: DateFormat,
    pub scale: f32,
}

impl Global for Theme {}

struct Palette {
    base: u32,
    title_bar: u32,
    sidebar: u32,
    surface: u32,
    surface_alt: u32,
    grid_header: u32,
    floating: u32,
    input: u32,
    border: u32,
    grid_line: u32,
    text: u32,
    text_muted: u32,
    text_faint: u32,
    keyword: u32,
    string: u32,
    number: u32,
    comment: u32,
}

const LIGHT: Palette = Palette {
    base: 0xefeff1,
    title_bar: 0xe8e8ea,
    sidebar: 0xf6f6f7,
    surface: 0xffffff,
    surface_alt: 0xf6f7f9,
    grid_header: 0xf3f3f5,
    floating: 0xffffff,
    input: 0xffffff,
    border: 0xd9d9dc,
    grid_line: 0xe9e9ec,
    text: 0x1d1d20,
    text_muted: 0x66666d,
    text_faint: 0xa1a1a8,
    keyword: 0x0b57d0,
    string: 0xb3261e,
    number: 0x1e7f45,
    comment: 0x8a8a90,
};

const DARK: Palette = Palette {
    base: 0x252527,
    title_bar: 0x2a2a2c,
    sidebar: 0x222224,
    surface: 0x1b1b1d,
    surface_alt: 0x202022,
    grid_header: 0x252527,
    floating: 0x2c2c2f,
    input: 0x1b1b1d,
    border: 0x38383b,
    grid_line: 0x2b2b2e,
    text: 0xe2e2e5,
    text_muted: 0x9b9ba2,
    text_faint: 0x66666c,
    keyword: 0x7aa7ff,
    string: 0xf0907c,
    number: 0x8fd19e,
    comment: 0x77777e,
};

fn hsla(color: u32) -> Hsla {
    rgb(color).into()
}

fn with_alpha(color: Hsla, a: f32) -> Hsla {
    Hsla { a, ..color }
}

/// `a` moved toward `b` by `t` (0 is `a`, 1 is `b`, above 1 goes past `b`), in RGB.
fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let (a, b) = (Rgba::from(a), Rgba::from(b));
    let lerp = |x: f32, y: f32| (x + (y - x) * t).clamp(0.0, 1.0);
    Hsla::from(Rgba {
        r: lerp(a.r, b.r),
        g: lerp(a.g, b.g),
        b: lerp(a.b, b.b),
        a: lerp(a.a, b.a),
    })
}

impl Theme {
    /// Builds the theme. `system_dark` is the system's appearance, used when the theme
    /// mode is System; `custom` is the theme file named in the settings, if it loaded.
    pub fn new(
        a: &Appearance,
        editor: &Editor,
        system_dark: bool,
        custom: Option<&ThemeFile>,
    ) -> Self {
        let dark = match custom.map(|c| c.base) {
            Some(ThemeBase::Dark) => true,
            Some(ThemeBase::Light) => false,
            None => match a.theme {
                ThemeMode::System => system_dark,
                ThemeMode::Light => false,
                ThemeMode::Dark => true,
            },
        };
        let p = if dark { &DARK } else { &LIGHT };
        let scale = a.ui_scale;
        let s = |v: f32| px((v * scale).round());
        let accent = custom
            .and_then(|c| c.color("accent"))
            .map_or_else(|| hsla(a.accent), |c| rgba(c).into());
        let accent_rgb = Rgba::from(accent);
        let luminance = 0.2126 * accent_rgb.r + 0.7152 * accent_rgb.g + 0.0722 * accent_rgb.b;
        let surface = hsla(p.surface);
        // Border contrast scales how far separators sit from the surface they divide.
        let line = |c: u32| mix(surface, hsla(c), a.border_contrast);
        let overlay = |alpha: f32| {
            if dark {
                rgba(0xffffff00 | (alpha * 255.0) as u32).into()
            } else {
                Hsla::from(Rgba {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: alpha,
                })
            }
        };
        let row_base = a.density.row_height();
        let font_extra = (a.mono_font_size - 12.0).max(0.0) * 1.4;
        let pick = |setting: &Option<String>, default: &'static str| -> SharedString {
            setting
                .as_deref()
                .map(str::trim)
                .filter(|f| !f.is_empty())
                .map_or_else(|| default.into(), |f| f.to_owned().into())
        };
        let mono_font = pick(&a.mono_font, "JetBrains Mono");
        let mut theme = Self {
            base: hsla(p.base),
            title_bar: hsla(p.title_bar),
            sidebar: hsla(p.sidebar),
            surface,
            surface_alt: a.zebra_color.map_or_else(|| hsla(p.surface_alt), hsla),
            grid_header: hsla(p.grid_header),
            floating: hsla(p.floating),
            input: hsla(p.input),
            hover: overlay(if dark { 0.05 } else { 0.045 }),
            selected: with_alpha(accent, if dark { 0.30 } else { 0.16 }),
            selection: with_alpha(accent, if dark { 0.38 } else { 0.22 }),
            selected_row: with_alpha(accent, if dark { 0.13 } else { 0.07 }),
            border: line(p.border),
            border_strong: mix(surface, hsla(p.border), (a.border_contrast * 1.5).min(2.5)),
            grid_line: line(p.grid_line),
            text: hsla(p.text),
            text_muted: hsla(p.text_muted),
            text_faint: hsla(p.text_faint),
            accent,
            accent_hover: Hsla {
                l: (accent.l - 0.06).max(0.0),
                ..accent
            },
            accent_text: if luminance > 0.6 {
                hsla(0x000000)
            } else {
                hsla(0xffffff)
            },
            accent_soft: with_alpha(accent, 0.16),
            danger: hsla(if dark { 0xff6961 } else { 0xd93025 }),
            warning: hsla(if dark { 0xffb340 } else { 0xb86e00 }),
            success: hsla(if dark { 0x5fd38a } else { 0x1e8e3e }),
            syntax_keyword: hsla(p.keyword),
            syntax_string: hsla(p.string),
            syntax_number: hsla(p.number),
            syntax_comment: hsla(p.comment),
            radius: px(a.corner_radius),
            radius_small: px((a.corner_radius * 0.7)
                .max(2.0)
                .min(a.corner_radius.max(2.0))),
            ui_font: pick(&a.ui_font, "Inter"),
            editor_font: editor
                .font
                .as_deref()
                .map(str::trim)
                .filter(|f| !f.is_empty())
                .map_or_else(|| mono_font.clone(), |f| f.to_owned().into()),
            mono_font,
            ui_size: s(a.ui_font_size),
            small_size: s((a.ui_font_size - 1.5).max(10.0)),
            mono_size: s(a.mono_font_size),
            editor_size: s(editor.font_size),
            row_height: s(row_base + font_extra),
            list_row: s(match a.density {
                deltos_settings::Density::Compact => 22.0,
                deltos_settings::Density::Regular => 24.0,
                deltos_settings::Density::Comfortable => 28.0,
            } + (a.ui_font_size - 13.0).max(0.0) * 1.4),
            tab_height: s(30.0),
            bar_height: s(30.0),
            zebra: a.zebra_stripes,
            grid_lines: a.grid_lines,
            null_text: match a.null_style {
                NullStyle::Label => NullStyle::Label.text(),
                NullStyle::Symbol => NullStyle::Symbol.text(),
                NullStyle::Empty => NullStyle::Empty.text(),
            },
            date_format: a.date_format,
            scale,
        };
        if let Some(custom) = custom {
            theme.apply(custom);
        }
        theme
    }

    /// Replaces the colors a theme file sets.
    fn apply(&mut self, custom: &ThemeFile) {
        for key in COLOR_KEYS {
            let Some(value) = custom.color(key.key) else {
                continue;
            };
            let color: Hsla = rgba(value).into();
            let slot = match key.key {
                "base" => &mut self.base,
                "title_bar" => &mut self.title_bar,
                "sidebar" => &mut self.sidebar,
                "surface" => &mut self.surface,
                "surface_alt" => &mut self.surface_alt,
                "grid_header" => &mut self.grid_header,
                "floating" => &mut self.floating,
                "input" => &mut self.input,
                "hover" => &mut self.hover,
                "selected" => &mut self.selected,
                "selection" => &mut self.selection,
                "border" => &mut self.border,
                "grid_line" => &mut self.grid_line,
                "text" => &mut self.text,
                "text_muted" => &mut self.text_muted,
                "text_faint" => &mut self.text_faint,
                "accent" => {
                    self.accent_hover = Hsla {
                        l: (color.l - 0.06).max(0.0),
                        ..color
                    };
                    self.accent_soft = with_alpha(color, 0.16);
                    &mut self.accent
                }
                "accent_text" => &mut self.accent_text,
                "danger" => &mut self.danger,
                "warning" => &mut self.warning,
                "success" => &mut self.success,
                "syntax_keyword" => &mut self.syntax_keyword,
                "syntax_string" => &mut self.syntax_string,
                "syntax_number" => &mut self.syntax_number,
                "syntax_comment" => &mut self.syntax_comment,
                _ => continue,
            };
            *slot = color;
        }
        if let Some(r) = custom.radius {
            self.radius = px(r);
            self.radius_small = px((r * 0.7).max(2.0).min(r.max(2.0)));
        }
    }

    /// A length in interface pixels, scaled by the zoom setting.
    pub fn s(&self, v: f32) -> Pixels {
        px((v * self.scale).round())
    }
}

pub fn theme(cx: &App) -> &Theme {
    cx.global::<Theme>()
}

/// Whether the system is in dark mode right now.
pub fn system_is_dark(cx: &App) -> bool {
    matches!(
        cx.window_appearance(),
        gpui::WindowAppearance::Dark | gpui::WindowAppearance::VibrantDark
    )
}

/// A tag or accent color as 0xRRGGBB.
pub fn color(rgb_value: u32) -> Hsla {
    hsla(rgb_value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(c: Hsla) -> u32 {
        let c = Rgba::from(c);
        let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
        (b(c.r) << 16) | (b(c.g) << 8) | b(c.b)
    }

    fn is_dark(t: &Theme) -> bool {
        Rgba::from(t.base).r < 0.5
    }

    #[test]
    fn system_mode_follows_the_system() {
        let a = Appearance::default();
        assert!(is_dark(&Theme::new(&a, &Editor::default(), true, None)));
        assert!(!is_dark(&Theme::new(&a, &Editor::default(), false, None)));
        let a = Appearance {
            theme: ThemeMode::Dark,
            ..Appearance::default()
        };
        assert!(is_dark(&Theme::new(&a, &Editor::default(), false, None)));
    }

    #[test]
    fn custom_theme_overrides() {
        let custom = ThemeFile::from_toml(
            "base = \"dark\"\nradius = 2\n[colors]\nsurface = \"#102030\"\naccent = \"#ffffff\"\n",
        )
        .unwrap();
        let t = Theme::new(
            &Appearance::default(),
            &Editor::default(),
            false,
            Some(&custom),
        );
        assert!(is_dark(&t));
        assert_eq!(hex(t.surface), 0x102030);
        assert_eq!(hex(t.accent_text), 0x000000);
        assert_eq!(t.radius, px(2.0));
    }

    #[test]
    fn density_and_scale_grow_rows() {
        let mut a = Appearance::default();
        let regular = Theme::new(&a, &Editor::default(), false, None).row_height;
        a.density = deltos_settings::Density::Comfortable;
        a.ui_scale = 1.25;
        let big = Theme::new(&a, &Editor::default(), false, None).row_height;
        assert!(big > regular);
    }
}
