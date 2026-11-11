//! Every Deltos setting, with defaults, range checks and an atomic TOML file.
//!
//! Every struct is `#[serde(default)]`, so a file written by an older or newer version
//! still opens: missing keys take their defaults and unknown keys are ignored. Numbers are
//! clamped into their ranges after a load by [`Settings::sanitize`], so a hand-edited file
//! cannot put the app into a state the settings screen could not.
//!
//! Passwords are never written here. [`secrets`] keeps them in the system keychain, under
//! the id of the saved connection they belong to.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

mod layout;
pub mod secrets;
mod theme_file;

pub use layout::{
    CONNECTIONS_PANEL, INSPECTOR_PANEL, OBJECTS_PANEL, PanelKind, SavedLayout, SavedPanel,
    WindowPlacement,
};
pub use theme_file::{COLOR_KEYS, ColorKey, ThemeBase, ThemeError, ThemeFile, parse_color};

/// The settings file, section by section.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Which one-time upgrades in [`Settings::from_toml`] this file has had.
    pub revision: u32,
    pub appearance: Appearance,
    pub updates: Updates,
    pub editor: Editor,
    pub data: Data,
    pub keybinds: Keybinds,
    pub connections: Vec<SavedConnection>,
    pub layout: SavedLayout,
}

/// 2: Obsidian replaced Graphite as the default dark palette.
const REVISION: u32 = 2;

impl Default for Settings {
    fn default() -> Self {
        Self {
            revision: REVISION,
            appearance: Appearance::default(),
            updates: Updates::default(),
            editor: Editor::default(),
            data: Data::default(),
            keybinds: Keybinds::default(),
            connections: Vec::new(),
            layout: SavedLayout::default(),
        }
    }
}

/// Light, dark, or whichever the system is using.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeMode {
    pub const ALL: [Self; 3] = [Self::System, Self::Light, Self::Dark];

    pub const fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }
}

/// How tall grid rows and list items are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Density {
    Compact,
    #[default]
    Regular,
    Comfortable,
}

impl Density {
    pub const ALL: [Self; 3] = [Self::Compact, Self::Regular, Self::Comfortable];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Compact => "Compact",
            Self::Regular => "Regular",
            Self::Comfortable => "Comfortable",
        }
    }

    /// Grid row height in pixels at a 12px grid font, before the font size adds to it.
    pub const fn row_height(self) -> f32 {
        match self {
            Self::Compact => 20.0,
            Self::Regular => 24.0,
            Self::Comfortable => 30.0,
        }
    }
}

/// How a NULL cell reads in the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NullStyle {
    /// The word NULL, dimmed.
    #[default]
    Label,
    /// `∅`, dimmed.
    Symbol,
    /// An empty cell.
    Empty,
}

impl NullStyle {
    pub const ALL: [Self; 3] = [Self::Label, Self::Symbol, Self::Empty];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Label => "NULL",
            Self::Symbol => "∅",
            Self::Empty => "Empty",
        }
    }

    /// The text drawn for a NULL cell.
    pub const fn text(self) -> &'static str {
        match self {
            Self::Label => "NULL",
            Self::Symbol => "∅",
            Self::Empty => "",
        }
    }
}

/// How dates and times are shown in the grid and the inspector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DateFormat {
    /// As the database sent them.
    #[default]
    Raw,
    /// `2024-03-09 14:05:00`: ISO 8601 with a space, no fractional seconds.
    Iso,
    /// `2024-03-09T14:05:00.000Z` style, kept to the full precision sent.
    Rfc3339,
}

impl DateFormat {
    pub const ALL: [Self; 3] = [Self::Raw, Self::Iso, Self::Rfc3339];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Raw => "As stored",
            Self::Iso => "2024-03-09 14:05:00",
            Self::Rfc3339 => "2024-03-09T14:05:00Z",
        }
    }
}

/// A color scheme. Light ones are offered for light mode and dark ones for dark mode, so
/// following the system switches between the two the person picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Palette {
    /// Neutral light gray.
    Classic,
    /// Warm off-white.
    Paper,
    /// Cool blue-gray.
    Frost,
    /// Soft green-gray.
    Sage,
    /// Neutral dark gray.
    Graphite,
    /// Deep blue-black.
    Midnight,
    /// Warm brown-black.
    Ember,
    /// Green-black.
    Forest,
    /// Violet-black.
    Plum,
    /// Pure black, for OLED screens.
    Obsidian,
}

impl Palette {
    pub const LIGHT: [Self; 4] = [Self::Classic, Self::Paper, Self::Frost, Self::Sage];
    pub const DARK: [Self; 6] = [
        Self::Graphite,
        Self::Midnight,
        Self::Ember,
        Self::Forest,
        Self::Plum,
        Self::Obsidian,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::Paper => "Paper",
            Self::Frost => "Frost",
            Self::Sage => "Sage",
            Self::Graphite => "Graphite",
            Self::Midnight => "Midnight",
            Self::Ember => "Ember",
            Self::Forest => "Forest",
            Self::Plum => "Plum",
            Self::Obsidian => "Obsidian",
        }
    }

    pub const fn is_dark(self) -> bool {
        !matches!(self, Self::Classic | Self::Paper | Self::Frost | Self::Sage)
    }
}

/// How the title bar stands apart from the window below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TitleBarStyle {
    /// The palette's own bar color.
    #[default]
    Plain,
    /// The bar is washed with the accent color.
    Tinted,
    /// A plain bar with an accent line along its bottom edge.
    Line,
}

impl TitleBarStyle {
    pub const ALL: [Self; 3] = [Self::Plain, Self::Tinted, Self::Line];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Plain => "Plain",
            Self::Tinted => "Tinted",
            Self::Line => "Accent line",
        }
    }
}

/// How the tabs of a panel stack are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TabStyle {
    /// The active tab is a raised card joined to the panel below it.
    #[default]
    Card,
    /// Flat tabs; the active one is underlined in the accent color.
    Underline,
    /// The active tab is a rounded pill.
    Pill,
}

impl TabStyle {
    pub const ALL: [Self; 3] = [Self::Card, Self::Underline, Self::Pill];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Card => "Card",
            Self::Underline => "Underline",
            Self::Pill => "Pill",
        }
    }
}

/// How grid column headers are lettered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeaderStyle {
    #[default]
    Bold,
    Regular,
    /// Capitals, as many dashboards letter them.
    Caps,
}

impl HeaderStyle {
    pub const ALL: [Self; 3] = [Self::Bold, Self::Regular, Self::Caps];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Bold => "Bold",
            Self::Regular => "Regular",
            Self::Caps => "CAPS",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub theme: ThemeMode,
    /// The file stem of a theme in the themes folder to draw on top of the light or dark
    /// base. `None` uses the built-in colors.
    pub custom_theme: Option<String>,
    /// Accent color as 0xRRGGBB: selection, focus rings, the primary button.
    pub accent: u32,
    /// The scheme used in light mode. Must be a light palette.
    pub light_palette: Palette,
    /// The scheme used in dark mode. Must be a dark palette.
    pub dark_palette: Palette,
    /// How much of the accent color washes into the window's surfaces, 0 to 0.3.
    pub accent_tint: f32,
    pub title_bar_style: TitleBarStyle,
    pub tab_style: TabStyle,
    pub header_style: HeaderStyle,
    /// Brighter secondary text, for low-contrast screens and tired eyes.
    pub high_contrast: bool,
    /// Interface font family. `None` is Adwaita Sans, which ships with the app.
    pub ui_font: Option<String>,
    /// Interface font size in pixels, 11 to 16.
    pub ui_font_size: f32,
    /// Code font family, for the query editor, the inspector and values that line up.
    /// `None` is Go Mono, which ships with the app.
    pub mono_font: Option<String>,
    /// Grid font family. `None` is Malgun Gothic where it is installed, as it is on
    /// Windows, and the interface font elsewhere.
    pub grid_font: Option<String>,
    /// Grid font size in pixels, 10 to 18.
    pub mono_font_size: f32,
    pub density: Density,
    /// Corner radius of buttons, fields and popovers, 0 to 10.
    pub corner_radius: f32,
    /// How strong separators are, 0.5 (faint) to 2 (strong).
    pub border_contrast: f32,
    /// Shades every other grid row.
    pub zebra_stripes: bool,
    /// The shade for striped rows as 0xRRGGBB, over the theme's own.
    pub zebra_color: Option<u32>,
    /// Draws lines between grid cells.
    pub grid_lines: bool,
    pub null_style: NullStyle,
    pub date_format: DateFormat,
    /// Interface zoom, 0.75 to 1.5.
    pub ui_scale: f32,
    /// Turns off animations.
    pub reduced_motion: bool,
    /// Draws Deltos' own title bar instead of the system's.
    pub custom_title_bar: bool,
    /// Window opacity, 0.5 to 1. Only some systems honor it.
    pub window_opacity: f32,
    /// Shows row estimates beside tables in the sidebar.
    pub sidebar_row_counts: bool,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: ThemeMode::System,
            custom_theme: None,
            accent: 0x2F7CF6,
            light_palette: Palette::Classic,
            dark_palette: Palette::Obsidian,
            accent_tint: 0.0,
            title_bar_style: TitleBarStyle::Plain,
            tab_style: TabStyle::Card,
            header_style: HeaderStyle::Bold,
            high_contrast: false,
            ui_font: None,
            ui_font_size: 13.0,
            mono_font: None,
            grid_font: None,
            mono_font_size: 12.0,
            density: Density::Regular,
            corner_radius: 5.0,
            border_contrast: 1.0,
            zebra_stripes: true,
            zebra_color: None,
            grid_lines: true,
            null_style: NullStyle::Label,
            date_format: DateFormat::Raw,
            ui_scale: 1.0,
            reduced_motion: false,
            custom_title_bar: true,
            window_opacity: 1.0,
            sidebar_row_counts: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannel {
    #[default]
    Stable,
}

impl UpdateChannel {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Stable => "stable",
        }
    }
}

/// Seamless updates: check, download in the background, and install when Deltos quits
/// or when the person clicks "Restart to update".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Updates {
    /// Looks for a new version at start and every six hours.
    pub auto_check: bool,
    /// Downloads a new version as soon as it is found.
    pub auto_download: bool,
    /// Installs a downloaded version when Deltos quits.
    pub install_on_quit: bool,
    pub channel: UpdateChannel,
}

impl Default for Updates {
    fn default() -> Self {
        Self {
            auto_check: true,
            auto_download: true,
            install_on_quit: true,
            channel: UpdateChannel::Stable,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Editor {
    /// `None` uses the grid font.
    pub font: Option<String>,
    /// Font size in pixels, 10 to 22.
    pub font_size: f32,
    /// Spaces per tab, 1 to 8.
    pub tab_size: u8,
    pub line_numbers: bool,
    pub wrap: bool,
    pub highlight: bool,
    /// Offers tables, columns and keywords in a popup while typing.
    pub autocomplete: bool,
    /// Format SQL writes keywords in uppercase.
    pub format_uppercase: bool,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            font: None,
            font_size: 13.0,
            tab_size: 4,
            line_numbers: true,
            wrap: false,
            highlight: true,
            autocomplete: true,
            format_uppercase: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Data {
    /// Rows read per request while browsing, 50 to 5,000.
    pub page_size: u32,
    /// Megabytes of rows kept in memory per open table, 32 to 4,096.
    pub cache_memory_mb: u32,
    /// Megabytes a query result may spill to disk before reading stops, 0 for no limit,
    /// at most 1,048,576.
    pub spill_limit_mb: u32,
    /// Asks before running statements that drop, truncate or delete without a WHERE.
    pub confirm_destructive: bool,
    /// Asks before every statement that writes on a connection tagged Staging or
    /// Production.
    pub confirm_production_writes: bool,
    /// Counts rows exactly instead of using the server's estimate.
    pub exact_counts: bool,
    /// Seconds before a statement is cancelled, 0 for never.
    pub query_timeout_secs: u32,
    /// Writes each cell edit, deletion and duplicate to the database at once, instead of
    /// staging changes in the table until they are committed.
    pub commit_immediately: bool,
}

impl Default for Data {
    fn default() -> Self {
        Self {
            page_size: 500,
            cache_memory_mb: 256,
            spill_limit_mb: 8192,
            confirm_destructive: true,
            confirm_production_writes: true,
            exact_counts: false,
            query_timeout_secs: 0,
            commit_immediately: false,
        }
    }
}

/// Shortcuts by action name, such as `"run-query" = "ctrl-enter"`. Actions missing from
/// the file keep their defaults; unknown names are ignored by the app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Keybinds(pub BTreeMap<String, String>);

/// Every bindable action, its default keys and what the settings screen calls it.
pub const KEYBIND_ACTIONS: &[(&str, &str, &str)] = &[
    ("run-query", "ctrl-enter", "Run statement"),
    ("run-all", "ctrl-shift-enter", "Run all statements"),
    ("explain-query", "ctrl-e", "Explain statement"),
    ("format-sql", "ctrl-shift-f", "Format SQL"),
    ("autocomplete", "ctrl-space", "Show completions"),
    ("save-query", "ctrl-shift-s", "Save query"),
    ("new-query", "ctrl-t", "New query tab"),
    ("close-tab", "ctrl-w", "Close tab"),
    ("next-tab", "ctrl-tab", "Next tab"),
    ("previous-tab", "ctrl-shift-tab", "Previous tab"),
    ("refresh", "ctrl-r", "Reload data"),
    ("search-objects", "ctrl-p", "Search objects"),
    ("toggle-sidebar", "ctrl-b", "Show or hide the sidebar"),
    ("toggle-inspector", "ctrl-i", "Show or hide the inspector"),
    ("settings", "ctrl-,", "Settings"),
    ("zoom-in", "ctrl-=", "Zoom in"),
    ("zoom-out", "ctrl--", "Zoom out"),
    ("zoom-reset", "ctrl-0", "Actual size"),
    ("reset-layout", "ctrl-shift-0", "Reset panel layout"),
    ("commit-changes", "ctrl-s", "Commit staged changes"),
    ("review-changes", "ctrl-shift-s", "Review staged changes"),
    ("discard-changes", "ctrl-shift-z", "Discard staged changes"),
    ("add-row", "ctrl-shift-n", "Add a row"),
    ("command-palette", "ctrl-k", "Command palette"),
    (
        "command-palette-alt",
        "ctrl-shift-p",
        "Command palette (second shortcut)",
    ),
    ("duplicate-rows", "ctrl-d", "Duplicate the selected rows"),
    (
        "clear-cells",
        "delete backspace",
        "Set the selected cells to NULL or empty",
    ),
    ("paste-cells", "ctrl-v", "Paste cells into the grid"),
    ("next-cell", "tab", "Save and go to the next cell"),
    (
        "previous-cell",
        "shift-tab",
        "Save and go to the previous cell",
    ),
    ("jump-up", "ctrl-up", "Go to the first row"),
    ("jump-down", "ctrl-down", "Go to the last row"),
    ("jump-left", "ctrl-left", "Go to the first column"),
    ("jump-right", "ctrl-right", "Go to the last column"),
    ("focus-filter", "ctrl-f", "Filter rows"),
];

impl Default for Keybinds {
    fn default() -> Self {
        Self(
            KEYBIND_ACTIONS
                .iter()
                .map(|(a, k, _)| ((*a).to_owned(), (*k).to_owned()))
                .collect(),
        )
    }
}

impl Keybinds {
    /// The keys bound to `action`, with the default for actions the file leaves out.
    pub fn get(&self, action: &str) -> Option<&str> {
        self.0.get(action).map(String::as_str).or_else(|| {
            KEYBIND_ACTIONS
                .iter()
                .find(|(a, _, _)| *a == action)
                .map(|(_, k, _)| *k)
        })
    }

    fn sanitize(&mut self) {
        for (action, keys, _) in KEYBIND_ACTIONS {
            self.0
                .entry((*action).to_owned())
                .or_insert_with(|| (*keys).to_owned());
        }
        for keys in self.0.values_mut() {
            *keys = keys.trim().to_lowercase();
        }
    }
}

/// A label for a connection, as TablePlus-style tools color them so production stands out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionTag {
    #[default]
    None,
    Local,
    Development,
    Testing,
    Staging,
    Production,
}

impl ConnectionTag {
    pub const ALL: [Self; 6] = [
        Self::None,
        Self::Local,
        Self::Development,
        Self::Testing,
        Self::Staging,
        Self::Production,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Local => "Local",
            Self::Development => "Development",
            Self::Testing => "Testing",
            Self::Staging => "Staging",
            Self::Production => "Production",
        }
    }

    /// The tag's color as 0xRRGGBB.
    pub const fn color(self) -> Option<u32> {
        match self {
            Self::None => None,
            Self::Local => Some(0x8E8E93),
            Self::Development => Some(0x34C759),
            Self::Testing => Some(0x5E5CE6),
            Self::Staging => Some(0xFF9F0A),
            Self::Production => Some(0xFF453A),
        }
    }
}

/// A connection the person saved. The password is in the keychain, not here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedConnection {
    /// Stable id: the keychain entry and open panels refer to it.
    pub id: String,
    pub name: String,
    pub kind: deltos_data::DriverKind,
    pub config: deltos_data::ConnectionConfig,
    pub tag: ConnectionTag,
    /// Whether the password is kept in the keychain or asked for on each connect.
    pub save_password: bool,
    /// Refuses every statement that could change data, and row edits.
    pub read_only: bool,
}

impl Default for SavedConnection {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            kind: deltos_data::DriverKind::Postgres,
            config: deltos_data::ConnectionConfig::default(),
            tag: ConnectionTag::None,
            save_password: true,
            read_only: false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not read or write {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the settings file is not valid TOML: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("could not encode the settings: {0}")]
    Encode(#[from] toml::ser::Error),
}

fn finite_or(v: f32, fallback: f32) -> f32 {
    if v.is_finite() { v } else { fallback }
}

impl Settings {
    /// Clamps every number into its range and repairs the layout and connection list.
    pub fn sanitize(&mut self) {
        let a = &mut self.appearance;
        let d = Appearance::default();
        a.accent &= 0xFF_FFFF;
        a.accent_tint = finite_or(a.accent_tint, d.accent_tint).clamp(0.0, 0.3);
        if a.light_palette.is_dark() {
            a.light_palette = d.light_palette;
        }
        if !a.dark_palette.is_dark() {
            a.dark_palette = d.dark_palette;
        }
        a.zebra_color = a.zebra_color.map(|c| c & 0xFF_FFFF);
        a.ui_font_size = finite_or(a.ui_font_size, d.ui_font_size).clamp(11.0, 16.0);
        a.mono_font_size = finite_or(a.mono_font_size, d.mono_font_size).clamp(10.0, 18.0);
        a.corner_radius = finite_or(a.corner_radius, d.corner_radius).clamp(0.0, 10.0);
        a.border_contrast = finite_or(a.border_contrast, 1.0).clamp(0.5, 2.0);
        a.ui_scale = finite_or(a.ui_scale, 1.0).clamp(0.75, 1.5);
        a.window_opacity = finite_or(a.window_opacity, 1.0).clamp(0.5, 1.0);
        for font in [
            &mut a.ui_font,
            &mut a.mono_font,
            &mut a.grid_font,
            &mut self.editor.font,
        ] {
            if font.as_ref().is_some_and(|f| f.trim().is_empty()) {
                *font = None;
            }
        }
        if a.custom_theme.as_ref().is_some_and(|t| t.trim().is_empty()) {
            a.custom_theme = None;
        }
        let e = &mut self.editor;
        e.font_size = finite_or(e.font_size, 13.0).clamp(10.0, 22.0);
        e.tab_size = e.tab_size.clamp(1, 8);
        let data = &mut self.data;
        data.page_size = data.page_size.clamp(50, 5_000);
        data.cache_memory_mb = data.cache_memory_mb.clamp(32, 4_096);
        data.spill_limit_mb = data.spill_limit_mb.min(1_048_576);
        data.query_timeout_secs = data.query_timeout_secs.min(86_400);
        self.keybinds.sanitize();
        // Connections need an id that is unique, since the keychain is keyed by it.
        let mut seen: Vec<String> = Vec::new();
        for (n, c) in self.connections.iter_mut().enumerate() {
            if c.id.trim().is_empty() || seen.contains(&c.id) {
                c.id = format!("conn-{n}-{}", seen.len());
            }
            seen.push(c.id.clone());
            if c.name.trim().is_empty() {
                c.name = c.kind.label().to_owned();
            }
        }
        self.layout.sanitize();
    }

    pub fn from_toml(text: &str) -> Result<Self, Error> {
        let mut s: Self = toml::from_str(text)?;
        // Files from before Obsidian became the default move off the old default; a
        // palette picked on purpose stays.
        if s.revision < 2 && s.appearance.dark_palette == Palette::Graphite {
            s.appearance.dark_palette = Palette::Obsidian;
        }
        s.revision = s.revision.max(REVISION);
        s.sanitize();
        Ok(s)
    }

    pub fn to_toml(&self) -> Result<String, Error> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Loads `path`, or the defaults when it does not exist yet.
    pub fn load(path: &Path) -> Result<Self, Error> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(Error::Io {
                path: path.to_owned(),
                source,
            }),
        }
    }

    /// Writes to a temporary file beside `path`, then renames it into place, so a crash
    /// mid-write never leaves a truncated settings file.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        write_atomic(path, self.to_toml()?.as_bytes())
    }

    pub fn connection(&self, id: &str) -> Option<&SavedConnection> {
        self.connections.iter().find(|c| c.id == id)
    }

    /// A connection id not used yet, from the name the person gave it.
    pub fn new_connection_id(&self, name: &str) -> String {
        let mut base: String = name
            .chars()
            .filter_map(|c| match c {
                'a'..='z' | '0'..='9' => Some(c),
                'A'..='Z' => Some(c.to_ascii_lowercase()),
                ' ' | '-' | '_' | '.' => Some('-'),
                _ => None,
            })
            .collect();
        base.truncate(40);
        let base = base.trim_matches('-');
        let base = if base.is_empty() { "connection" } else { base };
        let mut id = base.to_owned();
        let mut n = 2;
        while self.connection(&id).is_some() {
            id = format!("{base}-{n}");
            n += 1;
        }
        id
    }
}

/// Writes `bytes` to `path` through a temporary file and a rename, creating the folder.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let io = |source| Error::Io {
        path: path.to_owned(),
        source,
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut file = std::fs::File::create(&tmp).map_err(io)?;
    file.write_all(bytes).map_err(io)?;
    file.sync_all().map_err(io)?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(io)
}

/// Where Deltos keeps its files: settings and themes under the config folder, spilled
/// query results and downloads under the cache folder, the query history under the data
/// folder.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Paths {
    pub fn detect() -> Option<Self> {
        let dirs = directories::ProjectDirs::from("dev", "khaodius", "deltos")?;
        Some(Self {
            config_dir: dirs.config_dir().to_owned(),
            cache_dir: dirs.cache_dir().to_owned(),
            data_dir: dirs.data_dir().to_owned(),
        })
    }

    pub fn settings_file(&self) -> PathBuf {
        self.config_dir.join("settings.toml")
    }

    pub fn themes(&self) -> PathBuf {
        self.config_dir.join("themes")
    }

    pub fn crash_log(&self) -> PathBuf {
        self.config_dir.join("crash.log")
    }

    /// Where query results larger than memory are spilled.
    pub fn spill_dir(&self) -> PathBuf {
        self.cache_dir.join("spill")
    }

    /// The statements that ran, one JSON object per line.
    pub fn history_file(&self) -> PathBuf {
        self.data_dir.join("history.jsonl")
    }

    /// Saved queries.
    pub fn snippets_file(&self) -> PathBuf {
        self.config_dir.join("snippets.json")
    }

    /// Every theme file in the themes folder, by file stem. Unreadable files are skipped
    /// with a log line.
    pub fn scan_themes(&self) -> Vec<(String, ThemeFile)> {
        let Ok(entries) = std::fs::read_dir(self.themes()) else {
            return Vec::new();
        };
        let mut out: Vec<(String, ThemeFile)> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .filter_map(|p| {
                let stem = p.file_stem()?.to_string_lossy().into_owned();
                match ThemeFile::load(&p) {
                    Ok(t) => Some((stem, t)),
                    Err(e) => {
                        tracing::warn!("skipping theme {}: {e}", p.display());
                        None
                    }
                }
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deltos_data::{DriverKind, ObjectKind, ObjectRef};

    #[test]
    fn empty_file_loads_defaults() {
        assert_eq!(Settings::from_toml("").unwrap(), Settings::default());
    }

    #[test]
    fn moves_old_files_to_the_new_dark_default() {
        let old = "revision = 1\n[appearance]\ndark_palette = \"graphite\"\n";
        let s = Settings::from_toml(old).unwrap();
        assert_eq!(s.appearance.dark_palette, Palette::Obsidian);
        assert_eq!(s.revision, REVISION);
        let kept = "revision = 2\n[appearance]\ndark_palette = \"graphite\"\n";
        let s = Settings::from_toml(kept).unwrap();
        assert_eq!(s.appearance.dark_palette, Palette::Graphite);
        let plum = "revision = 1\n[appearance]\ndark_palette = \"plum\"\n";
        assert_eq!(
            Settings::from_toml(plum).unwrap().appearance.dark_palette,
            Palette::Plum
        );
    }

    #[test]
    fn keeps_the_ssh_tunnel_without_its_secret() {
        let mut s = Settings::default();
        s.connections.push(SavedConnection {
            id: "remote".into(),
            kind: DriverKind::Mysql,
            config: deltos_data::ConnectionConfig {
                ssh: deltos_data::SshTunnel {
                    enabled: true,
                    host: "prod-db".into(),
                    auth: deltos_data::SshAuth::Agent,
                    secret: Some("hunter2".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        });
        let text = s.to_toml().unwrap();
        assert!(!text.contains("hunter2"));
        let back = Settings::from_toml(&text).unwrap();
        let ssh = &back.connections[0].config.ssh;
        assert!(ssh.enabled);
        assert_eq!(ssh.host, "prod-db");
        assert_eq!(ssh.auth, deltos_data::SshAuth::Agent);
        assert_eq!(ssh.secret, None);
    }

    #[test]
    fn round_trips() {
        let mut s = Settings::default();
        s.connections.push(SavedConnection {
            id: "local".into(),
            name: "Local".into(),
            kind: DriverKind::Mysql,
            config: deltos_data::ConnectionConfig {
                host: "127.0.0.1".into(),
                port: Some(3306),
                user: "root".into(),
                password: Some("secret".into()),
                options: vec![("charset".into(), "utf8mb4".into())],
                ..Default::default()
            },
            tag: ConnectionTag::Production,
            save_password: true,
            read_only: true,
        });
        s.layout = SavedLayout::default_for(Some("local"));
        s.layout.panels.push(SavedPanel {
            id: deltos_dock::PanelId(77),
            kind: PanelKind::Table {
                connection: "local".into(),
                object: ObjectRef {
                    namespace: vec!["shop".into()],
                    name: "orders".into(),
                    kind: ObjectKind::Table,
                },
            },
        });
        s.layout.sanitize();
        let text = s.to_toml().unwrap();
        assert!(!text.contains("secret"), "passwords stay out of the file");
        let back = Settings::from_toml(&text).unwrap();
        let mut expected = s.clone();
        expected.connections[0].config.password = None;
        assert_eq!(back, expected);
    }

    #[test]
    fn old_and_partial_files_load() {
        let s = Settings::from_toml(
            "[appearance]\ntheme = \"dark\"\nui_scale = 9.0\nfuture_key = 1\n[brand_new]\nx = 1\n",
        )
        .unwrap();
        assert_eq!(s.appearance.theme, ThemeMode::Dark);
        assert_eq!(s.appearance.ui_scale, 1.5);
        assert_eq!(s.updates, Updates::default());
        assert!(s.updates.auto_check && s.updates.auto_download);
        assert_eq!(s.data.cache_memory_mb, 256);
        assert_eq!(s.keybinds.get("run-query"), Some("ctrl-enter"));
        // Files from before staged edits keep them staged.
        assert!(!s.data.commit_immediately);
        assert_eq!(s.keybinds.get("commit-changes"), Some("ctrl-s"));
        // Connections and safety settings from before read-only existed.
        let s = Settings::from_toml(
            "[data]\nconfirm_destructive = false\n[[connections]]\nid = \"a\"\nkind = \"postgres\"\ntag = \"production\"\n",
        )
        .unwrap();
        assert!(!s.connections[0].read_only);
        assert!(!s.data.confirm_destructive);
        assert!(s.data.confirm_production_writes);
        assert_eq!(s.keybinds.get("clear-cells"), Some("delete backspace"));
    }

    #[test]
    fn repairs_connections() {
        let s = Settings::from_toml(
            "[[connections]]\nid = \"a\"\nkind = \"sqlite\"\n[[connections]]\nid = \"a\"\nname = \"B\"\n",
        )
        .unwrap();
        assert_eq!(s.connections.len(), 2);
        assert_ne!(s.connections[0].id, s.connections[1].id);
        assert_eq!(s.connections[0].name, "SQLite");
        assert_eq!(s.new_connection_id("a"), "a-2");
        assert_eq!(s.new_connection_id("Prod DB!"), "prod-db");
        assert_eq!(s.new_connection_id("***"), "connection");
    }

    #[test]
    fn saves_atomically() {
        let dir = std::env::temp_dir().join(format!("deltos-settings-{}", std::process::id()));
        let path = dir.join("settings.toml");
        let mut s = Settings::default();
        s.appearance.zebra_stripes = false;
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap(), s);
        assert!(!dir.join("settings.toml.tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_file_is_defaults() {
        let path = std::env::temp_dir().join("deltos-no-such-dir/settings.toml");
        assert_eq!(Settings::load(&path).unwrap(), Settings::default());
    }
}
