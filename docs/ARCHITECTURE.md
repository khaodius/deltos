# Architecture

Deltos is a native database client in Rust, drawn with GPUI. It runs on Linux (X11 and
Wayland) and Windows. This page describes how the pieces fit together, and why.

## Crates

| Crate | What it holds |
| --- | --- |
| `deltos-data` | The contract between the UI and the drivers: `Driver`, `Connection`, `Value`, `Column`, browse and execute requests. |
| `deltos-drivers` | PostgreSQL, MySQL/MariaDB, SQLite and MongoDB behind that contract. `available()` lists the ones a build includes. |
| `deltos-grid` | The bounded row cache behind every grid: `GridCache`, `BrowseLoader` for tables, `StreamLoader` for query results, CSV export. |
| `deltos-dock` | The panel layout as plain data: split trees for the main window and each torn-off window, with every move as a method. No UI code. |
| `deltos-settings` | `settings.toml` (with defaults, sanitizing and atomic saves), the saved layout, theme files and the keychain. |
| `deltos-updater` | Checking, verifying and installing signed releases. |
| `deltos` | The app: state, theme, views and the GPUI glue. |

Only `deltos` depends on GPUI. Everything below it can be tested without a window.

## Threads

GPUI owns the main thread, and every view runs there. Database work runs on a Tokio
runtime (`net.rs`, at most four worker threads). `Net::run` starts a future on the runtime
and returns one that GPUI's executor can await, so a view writes:

```rust
let job = cx.global::<Net>().run(async move { conn.describe(&object).await });
cx.spawn(async move |this, cx| {
    let info = job.await;
    this.update(cx, |this, cx| { /* store it, cx.notify() */ }).ok();
}).detach();
```

The grid cache calls back from runtime threads when pages land. `ui::grid::hooks` turns
those calls into one redraw per burst: the callback sets an atomic flag and wakes a GPUI
task, which waits 8 ms for more pages and then calls `notify` once.

## State

`AppState` (`state.rs`) is one GPUI entity holding the settings, open connections
(`Session`), connection attempts, the inspected row and the statement history. Views read it
and subscribe to its `StateEvent`s. Settings changes go through `update_settings`, which
sanitizes, rebuilds the `Theme` global, redraws every window and saves 400 ms later.

### Connection safety

A saved connection marked read-only opens behind `safety::ReadOnly` (`safety.rs`), which
wraps the session's `Arc<dyn Connection>`. Every panel goes through the session, so no path
can get around it: it reports `edit_rows: false`, refuses `apply_edits`, and refuses every
statement that `deltos_drivers::classify` does not know to be a read (`SELECT`, `WITH` without
data-modifying parts, `SHOW`, `EXPLAIN` without `ANALYZE` of a write, `DESCRIBE`, reading
`PRAGMA`s, `VALUES`, and MongoDB reads without `$out` or `$merge`; a `SELECT` that calls
`set_config`, `dblink` or another function that acts counts as a write). The connection is
also opened with the driver option `read_only`: PostgreSQL and MySQL make every session
read-only as it opens (the drivers open several, and open them again after a lost
connection), and SQLite opens the file read-only, so a function that writes from inside a
`SELECT` fails too.

The same classifier decides which statements the query tab asks about first
(`safety::confirmation`): `UPDATE` and `DELETE` without `WHERE`, `DROP` and `TRUNCATE` on
every connection, and on connections tagged Staging or Production also `ALTER` and every
write. The tag's color marks the connection in the title bar, its tabs, the status bars and
the connection list (`ui/environment.rs`).

The `Theme` global (`theme.rs`) is computed once per change from the appearance settings,
the system appearance and an optional theme file (see [THEMES.md](THEMES.md)). Views copy the
colors they need from it; nothing parses colors while drawing.

## Panels and the dock

Everything in the window is a panel: objects, connections, inspector, history, tables,
queries and settings. `PanelKind` (in `deltos-settings`) says what a panel shows and is what
the layout saves. `deltos-dock::Layout` says where each panel sits: a tree of row and column
splits whose leaves are tab stacks, one tree for the main window and one per torn-off
window.

`ui::workspace::Workspace` owns the layout and one view per panel. It handles opening
(including the preview tab that a single click in the sidebar replaces), closing, dragging,
splitting and tearing off. `ui::dock::DockView` draws one window's tree. While it draws, it
records the bounds of every stack, tab and divider. A drag then compares the pointer
against those records, so hit testing never walks the element tree.

Two GPUI details shape this code:

- A view only gets `on_mouse_move` while the pointer is over it. Drags and divider resizes
  therefore register window-level handlers from a `canvas` for as long as they last, so they
  keep tracking past the panel's edge and outside the window.
- GPUI draws a new window while it opens, and that draw reads the workspace. Panel windows
  therefore open from `cx.defer`, just after the update that asked for them.

### Tearing off and docking back

- **Tear off.** Drag a tab more than 10 px outside the window, or press its pop-out button.
  The panel moves into a new `FloatingWindow` with its own tree, and a `PanelWindow` opens
  for it. On X11 the same press keeps moving the new window (`start_window_move`), so
  tearing off and placing it is one motion.
- **Dock back.** Drag the panel window's header over the main window. On X11 and Windows
  the app sees window positions: it shows the drop target under the pointer, and when the
  window rests there for 450 ms, it merges the window's tree into the main window at that
  target. Wayland does not tell apps where their windows are. There the panel window's Dock
  button docks it, and so does closing it. Docked panels go back where they came from when
  that place still exists. A tool panel docked beside other panels takes a tool's width,
  not half of its neighbor.
- **Between windows.** On X11 and Windows, a tab dragged out of a panel window over the main
  window drops into the main window's layout, at the target under the pointer.

The layout, including each panel window's bounds, is saved in `settings.toml` and restored
at start.

## Showing large tables

A table panel builds a `BrowseLoader` and a `GridCache` from the data settings: rows per
page, and memory per table (256 MB by default). The cache holds whole pages and evicts the
least recently used. The loader seeks by key where the driver can, so page 100,000 costs the
same as page 1. The grid view (`ui::grid`):

- keeps its scroll position as a row number in `f64`, because `f32` pixels lose whole rows
  past about 16 million;
- paints only the visible rows and columns, straight from the cache's row slices, with no
  per-cell element and no copy of a `Value`;
- caches formatted cell text, up to 16,384 entries, so scrolling does not reformat;
- draws its own scrollbars, because a thumb for 50 million rows must still be grabbable.

A query result goes through a `StreamLoader` instead. It reads the result forward as the
viewport needs rows and keeps them in a temporary file, up to the spill limit (8 GB by
default). The grid counts "N+ rows" until the stream ends. Fetch all reads the rest now,
and Stop ends reading.

## The demo connection

`DELTOS_DEMO=1` opens a built-in connection (`demo.rs`). Its values are computed from a hash
of row and column, so nothing is stored. It has a 50,000,000 row `orders` table, a 300 column
`events_wide` table, JSON documents, and views. Other screens:

- `DELTOS_DEMO=settings`: Settings.
- `DELTOS_DEMO=connections`: the connection form.
- `DELTOS_DEMO=query`: a query tab that runs at once.
- `DELTOS_DEMO=wide`: the 300 column table.
- `DELTOS_DEMO=popout`: the inspector in its own window.

The demo starts from the default layout and never writes settings.

## Files and logs

- Settings, saved connections and the layout are in `settings.toml`, under
  `~/.config/deltos/` on Linux and `%APPDATA%\khaodius\deltos\config\` on Windows.
- Passwords are in the system keychain under the service `dev.khaodius.deltos`, never in the
  file.
- Theme files are in `themes/` next to the settings.
- A crash writes `crash.log` there too.
- Query spill files go in the cache directory under `spill/`.
- `DELTOS_LOG` sets the log filter, for example `DELTOS_LOG=deltos=debug`.

## Updates

`update.rs` wraps `deltos-updater`. With the default settings, Deltos checks at start and
every six hours, and downloads a new version in the background. The title bar then shows
"Restart to update". If you don't restart, the update installs when Deltos quits. Each step
has a switch in Settings > Updates. Builds without a signing key, and the demo, do not check.
