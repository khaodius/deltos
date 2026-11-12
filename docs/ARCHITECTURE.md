# Architecture

Deltos is a native database client in Rust, drawn with GPUI. It runs on Linux (X11 and
Wayland) and Windows. This page describes how the pieces fit together, and why.

## Crates

| Crate | What it holds |
| --- | --- |
| `deltos-data` | The contract between the UI and the drivers: `Driver`, `Connection`, `Value`, `Column`, browse and execute requests. |
| `deltos-drivers` | PostgreSQL, MySQL/MariaDB, SQLite and MongoDB behind that contract. `available()` lists the ones a build includes. `ddl` turns structure edits into each dialect's statements. |
| `deltos-grid` | The bounded row cache behind every grid: `GridCache`, `BrowseLoader` for tables, `StreamLoader` for query results, and the streaming export with its CSV, JSON and Markdown writers. |
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

### Connection links and imports

`deltos postgres://user@host:5432/db` (and `postgresql://`, `mysql://`, `mariadb://`,
`mongodb://`, `mongodb+srv://`, `sqlite:///path`, `file:///path/app.db` or a plain path to a
`.db`, `.sqlite`, `.sqlite3` or `.db3` file) opens that connection (`link.rs`). A link that
reaches a saved connection (same kind, host, port and database, and the user when the link
names one; never one behind an SSH tunnel) opens it. Anything else opens as an unsaved
connection in `AppState::unsaved`: it shows in the connection list as "Not saved", its
password stays in memory, and saving it from the form is the only way it reaches the
settings file and, with Keychain on, the keychain. A connection turned away for a missing
password (imported ones, links, connections that keep no password) raises the password
prompt over the main window (`ui/password.rs`). Each link starts its own Deltos; there is
no single-instance hand-off yet.

On Linux the desktop entry registers the URL schemes (`MimeType=x-scheme-handler/...` in
`packaging/linux/deltos.desktop`) and the SQLite file types, and `scripts/install.sh` and
the packages refresh the desktop database. On Windows the installer does not register them
yet. To do it by hand, or in the Inno Setup script later, each scheme needs these keys
(shown for `postgres`; repeat for `postgresql`, `mysql`, `mariadb`, `mongodb` and
`mongodb+srv`):

```text
HKEY_CURRENT_USER\Software\Classes\postgres
    (Default)      = "URL:PostgreSQL connection"
    URL Protocol   = ""
HKEY_CURRENT_USER\Software\Classes\postgres\shell\open\command
    (Default)      = "C:\Users\<you>\AppData\Local\Programs\Deltos\deltos.exe" "%1"
```

In Inno Setup that is one `[Registry]` group per scheme under `HKA\Software\Classes`, with
`Flags: uninsdeletekey` on the scheme key.

The connection list's menu imports connections from other tools (`import/`): DBeaver's
`data-sources*.json`, the `dataSources.xml` (with `dataSources.local.xml` and
`sshConfigs.xml` beside it) of DataGrip and the other JetBrains IDEs, and TablePlus
connection lists. It looks in each tool's usual folders and takes a picked file too, then
lists what it found with the duplicates (same kind, host, port, database, user and SSH
host) and unsupported databases left out. Passwords are never imported or decrypted.

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

## Changing structure

The Structure tab (`ui::structure`) stages edits to columns, indexes and foreign keys
against the table as last described. `deltos_drivers::ddl::plan` compares the two and
returns the statements, or a reason it will not. It is pure, so every dialect is tested
without a server. Review shows exactly those statements, and `Connection::apply_schema` runs
them: in one transaction on PostgreSQL and SQLite, one by one on MySQL, which commits each
DDL statement. SQLite cannot alter most column details in place, so the plan rebuilds the
table: create a copy with the new shape, copy the rows, drop the old table, rename the copy,
and recreate its indexes and triggers. Foreign key checks are off during the rebuild and
checked before the commit. Tables the rebuild could damage (generated columns, CHECK
constraints, WITHOUT ROWID and the like) are refused with a note.

## The demo connection

`DELTOS_DEMO=1` opens a built-in connection (`demo.rs`). Its values are computed from a hash
of row and column, so nothing is stored. It has a 50,000,000 row `orders` table, a 300 column
`events_wide` table, JSON documents, and views. Tables over 20,000 rows sort only by their
key and filter only on columns that grow with it (the key and the timestamps that follow
it), which narrows them to a range of ids. Row edits stage, review and show their SQL, but
`apply_edits` refuses the commit and the changes stay staged; changing a table's structure
and imports are off. Other screens:

- `DELTOS_DEMO=settings`: Settings.
- `DELTOS_DEMO=connections`: the connection form.
- `DELTOS_DEMO=query`: a query tab that runs at once.
- `DELTOS_DEMO=wide`: the 300 column table.
- `DELTOS_DEMO=popout`: the inspector in its own window.

The demo starts from the default layout and never writes settings.

## The command line

`main` hands the arguments to `cli::run_from_args` before GPUI starts. When the first
argument is a command (`query`, `export`, `import`, `objects`, `test`, `connections`, `help`,
`version`) it runs it and exits; anything else, a link or nothing, starts the app. The CLI
opens no window and reuses the layers under the views:

- `cli/target.rs` resolves a saved connection by id or name, `demo`, or a link
  (`link.rs`), fills passwords from `DELTOS_PASSWORD` / `DELTOS_SSH_SECRET` or the
  keychain, and opens it with `state::open_on_runtime`, the same tunnel and connect path
  the app uses. Saved connections keep their tag and read-only guard (`safety.rs`).
- `query` splits statements with `ui/transfer/read.rs`'s `ScriptReader`, asks
  `safety::confirmation` about each and refuses without `--yes`, and streams each result's
  batches through a `deltos_grid` row writer, or `cli/output.rs`'s `TableWriter`, which
  fixes its widths from the first 200 rows.
- `export` is `BrowseLoader` plus `ui/transfer/export.rs`'s `run`; `import` is
  `ui/transfer/import.rs`'s `run_import` and `run_script`.

Logs go to standard error so they never mix into results.

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
