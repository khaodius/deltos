# Themes

Deltos ships a light and a dark theme. Settings > Appearance picks between them, or follows
the system. A theme file changes the colors on top of either one.

## Where theme files go

Put `.toml` files in the `themes` folder next to `settings.toml`:

- Linux: `~/.config/deltos/themes/`
- Windows: `%APPDATA%\khaodius\deltos\config\themes\`

Settings > About shows the exact path. Then pick the file under Settings > Appearance >
Theme file. The list shows files by name without `.toml`. After you add or edit a file,
choose "Rescan the themes folder" from the same menu.

## Format

```toml
name = "Graphite"
author = "you"
# The built-in theme the file starts from: "light" or "dark".
base = "dark"
# Optional. Corner radius in pixels, 0 to 10. Replaces the corner radius setting.
radius = 4

[colors]
base = "#16171a"
sidebar = "#1b1c20"
surface = "#1e1f23"
accent = "#7c5cf6"
text = "#e6e6e9"
```

Every key is optional. A key you leave out keeps the base theme's color. Colors are `#rgb`,
`#rrggbb` or `#rrggbbaa`. Deltos skips unknown keys and colors it cannot read, and logs a
warning for a file that is not valid TOML.

## Color keys

| Key | What it paints |
| --- | --- |
| `base` | Window background and tab strip |
| `title_bar` | Title bar |
| `sidebar` | Sidebars and panels |
| `surface` | Grid and editor background |
| `surface_alt` | Striped grid rows |
| `grid_header` | Grid column headers |
| `floating` | Menus, popovers and tooltips |
| `input` | Text fields |
| `hover` | Items under the pointer |
| `selected` | Selected list items and rows |
| `selection` | Selected grid cells |
| `border` | Separators between panels |
| `grid_line` | Lines between grid cells |
| `text` | Text |
| `text_muted` | Secondary text |
| `text_faint` | Hints, placeholders and NULL |
| `accent` | Accent; replaces the accent setting |
| `accent_text` | Text on the accent |
| `danger` | Errors and destructive actions |
| `warning` | Warnings |
| `success` | Success and connected state |
| `syntax_keyword` | SQL keywords |
| `syntax_string` | Strings in the editor |
| `syntax_number` | Numbers in the editor |
| `syntax_comment` | Comments in the editor |

Without an `accent_text` color, Deltos picks black or white text for the accent, whichever
reads better. The appearance settings still apply on top of a theme file: separator strength,
striped rows, cell lines, density, fonts and zoom.
