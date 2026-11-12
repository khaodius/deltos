# Changelog

Packages for every version are attached to its
[release](https://github.com/khaodius/deltos/releases), with SHA-256 checksums and a signed
update manifest.

## 0.1.9 - 2026-11-11

- **History that lasts.** The last 5,000 statements you ran are kept between sessions, with
  their connection, time, duration, rows and errors. The History panel searches them, shows
  only the active connection's or all of them, and opens one in a new query tab with a click.
  Entries can be copied, removed or cleared.
- **Saved queries.** Ctrl+Shift+S in a query tab saves the query, or the selection, by name,
  for that connection or for all of them. Saved queries are listed under Saved in the History
  panel, where they open in a new tab, go in at the cursor, and can be renamed or deleted.
  The editor suggests them by name, and the command palette finds them.

## 0.1.8 - 2026-11-11

- **More ways to filter.** The filter bar joins its conditions with All (AND) or Any (OR) and
  adds not contains, ends with, in list, not in list, between, LIKE pattern, is empty and is
  not empty, in every database. Values are always sent as parameters.
- **Write the WHERE yourself.** Switch the bar to SQL (JSON on MongoDB) to type a raw
  condition; it starts out holding the WHERE of your conditions. A muted line always shows the
  WHERE that runs, and Copy as SQL copies the whole query.
- **Filters stay.** Ctrl+F focuses the filter bar, and each table keeps its sort and filters
  when you close and reopen it during a session.
- **Pages and counts.** The status bar shows the rows in view and the total, counts exactly on
  demand in the background, and pages with previous and next, a page size, and a jump to any
  page or row, without reading the rows in between.

## 0.1.7 - 2026-11-11

- **Suggestions while you type.** The query editor suggests tables, columns, keywords and
  functions as you type, and Ctrl+Space asks for them. It knows which tables a statement
  names, aliases included. In MongoDB it suggests collections after `db.`, then their methods.
  Settings → Editor → Suggest while typing turns the automatic popup off.
- **Run the statement or everything.** Ctrl+Enter runs the selection or the statement at the
  cursor, which is shaded when the editor holds several. Ctrl+Shift+Enter runs them all in
  order and stops at the first error.
- **A tab per result.** Each statement that returns rows gets its own result tab, with a
  summary of every statement's rows, affected count or error. A pinned tab stays through the
  next run.
- **See the plan.** Ctrl+E shows how the database will run a statement as a tree, with each
  step's share of the cost and time and the most expensive step marked: PostgreSQL, MySQL,
  SQLite and MongoDB. On PostgreSQL and MongoDB, Analyze measures it by running it; on
  PostgreSQL its changes are rolled back.
- **Format SQL.** Ctrl+Shift+F formats the selection or the whole query, keeping comments,
  strings and quoted names exactly as they are.

## 0.1.6 - 2026-11-11

- **Keyboard-first grid.** Type on a cell to start editing it with that character. Enter saves
  and moves down, Tab and Shift+Tab save and move across, wrapping at the ends of a row, and
  Escape cancels. Ctrl+arrows jump to the first or last row or column, and Ctrl+Home and
  Ctrl+End go to the first and last cell.
- **Clear cells.** Delete or Backspace sets the selected cells to NULL, or to empty text where
  the column can't be NULL, and the status bar says which. The change is staged like any
  other edit.
- **Copy and paste like a spreadsheet.** Ctrl+C copies tab-separated text quoted the way
  spreadsheets expect, and Ctrl+Shift+C adds the headers. Ctrl+V pastes it into the grid from
  the focused cell as staged edits and tells you which cells didn't fit. One value pasted on a
  selection fills all of it.
- **Duplicate rows.** Ctrl+D stages copies of the selected rows.

## 0.1.5 - 2026-11-11

- **Production looks like production.** A connection's tag colors the strip under the title
  bar, its tabs, the status bars and the connection list: a quiet dot for Local and
  Development, an orange mark for Staging and a red badge for Production.
- **Read-only connections.** Turn on Read-only in a connection and Deltos runs only reads on
  it. Edits and every statement that can change data are refused, and the server's own
  read-only setting is turned on for every session too (PostgreSQL and MySQL; SQLite files
  open read-only), so a function that writes from inside a SELECT fails as well. A lock marks
  the connection everywhere.
- **Asks before risky statements.** `UPDATE` and `DELETE` without `WHERE`, `DROP` and
  `TRUNCATE` ask first on every connection; a `WHERE` inside a string, a comment or a
  sub-select no longer counts. On Staging and Production, `ALTER` and every write ask too;
  Settings → Data → Confirm every write on production turns that part off. The question names
  the connection, in its environment's color.

## 0.1.4 - 2026-11-11

- **Command palette.** Ctrl+K, or Ctrl+Shift+P, finds actions, open tabs, connections,
  tables, views and collections, and Settings pages from a few typed letters, with the
  matched letters marked and each action's shortcut shown. Start with `>` for commands, `@`
  for tables or `#` for connections. Items you used recently come first.
- **Ctrl+P searches tables in the palette.** Search objects now opens the palette on tables,
  views and collections of every open connection.

## 0.1.3 - 2026-11-11

- **Edits wait for Commit.** Changed cells, deleted rows and new rows are staged and drawn in
  the grid until you commit them: amber for a changed cell, red and struck through for a
  deleted row, green at the top for a new one. The status bar counts them, with Review,
  Discard and Commit. Ctrl+S commits, Ctrl+Shift+S reviews and Ctrl+Shift+Z discards. Review
  shows the exact SQL that will run, or the mongosh calls for MongoDB.
- **One transaction per commit.** A commit runs in one transaction where the database has
  them. If it fails, nothing is lost: the changes stay staged and the error shows. Refreshing,
  sorting or filtering keeps them, and closing a table or quitting with staged changes asks
  first.
- **New rows.** Ctrl+Shift+N, or the + button in the status bar, adds a row. Duplicate stages
  copies of the selected rows.
- **Edit in the inspector.** Every field of the selected row can be edited in the inspector,
  with multi-line fields for long text and JSON. Changes are staged with the grid's, tinted,
  with the old value and a revert button. Key columns warn before you change them. Rows from
  query results stay read-only.
- **Write at once if you prefer.** Settings → Data → Commit edits immediately brings back
  saving each edit as soon as you make it.

## 0.1.2 - 2026-11-11

- **SSH tunnels built in.** Turn on SSH tunnel in a MySQL, MariaDB, PostgreSQL or MongoDB
  connection and Deltos reaches a database that only listens on its own server, with no
  terminal and no `ssh -L`. The SSH host can be a `Host` alias from `~/.ssh/config`, whose
  HostName, User, Port and IdentityFile are used. Sign in with a key file (ed25519, ECDSA or
  RSA, OpenSSH format, with an optional passphrase), the ssh-agent (or Pageant on Windows) or
  a password; the password and passphrase go to the keychain with the database password. No
  OpenSSH client is needed.
- **Host and Port mean the server's side.** With a tunnel on, Host and Port are the
  database's address as seen from the SSH server, 127.0.0.1 and the default port when left
  empty. The tunnel listens on a free local port the system picks, so it never lands on a
  local database.
- **Tunnels look after themselves.** Each open connection has its own tunnel, closed on
  disconnect and on quit. Keepalives notice a dead link; the tunnel then reconnects on the
  same port and opens the database connection again, and says so in the connection list.
- **Host keys are checked.** Keys are checked against `~/.ssh/known_hosts`. A server seen for
  the first time shows its fingerprint and asks before it is trusted and added; a changed key
  is refused with the line to remove.
- **Test says which step failed**: `SSH: host unreachable`, `SSH: auth failed`, a server that
  cannot reach the database address, or `Tunnel OK, database: ...` with the database's own
  message. Pasting a connection URL still fills only the database fields.
- **Clear message for GSSAPI servers.** A MySQL or MariaDB server that asks for Windows/GSSAPI
  login (`auth_gssapi_client`, common on Windows MariaDB installs) now says it is not supported
  and to check that you are not reaching a local MariaDB on 3306, instead of showing the raw
  driver error.
- **Color picker.** The accent color has a saturation and brightness square, a hue slider and
  an eyedropper that takes the color of any pixel on the screen (Windows and X11).
- **Obsidian is the default dark palette.** Settings that still had the old default,
  Graphite, move to Obsidian; a palette picked on purpose stays.
- **Title bar switch applies at once on Windows.** Turning Deltos' own title bar on or off
  reopens the windows in place instead of waiting for a restart.
- **Panels dock back quickly.** A panel window dropped over the main window docks the moment
  the button comes up, holding it there docks after a fifth of a second instead of almost half
  a second, a window torn off can be docked again right away, and the landing animation is
  shorter.

## 0.1.1 - 2026-11-11

- **Edit cells in place.** Double-click a cell, or press Enter or F2, to edit it. Enter or a
  click anywhere else saves; Escape cancels. What you type is checked against the column's
  type first, the grid shows the new value at once, and it goes back if the database refuses
  it, with the reason in the status bar.
- **Right-click menus** on cells, column headers and the object list: set to NULL, copy as
  CSV, JSON, Markdown or `INSERT` statements, copy a row's `WHERE` clause, show or hide rows
  with the same value, sort, fit columns, duplicate rows, delete rows after a confirmation,
  and open a table or a query on it from the sidebar.
- **Grid font.** The grid has its own font setting, Malgun Gothic by default where it is
  installed (Windows) and the interface font elsewhere. Every font menu shows each name in its
  own font.
- **Torn-off panels land under the pointer.** The new window is placed under the pointer
  before it starts following it, on Windows and on X11, so it no longer trails off to one
  side.
- **Faster CI and releases.** Workflows reuse compiled dependencies and system packages
  between runs, skip documentation-only changes, and run the database tests on pull requests
  only when the data layer changed. Releases reuse the dependencies CI built on `main`.

## 0.1.0 - 2026-11-09

- **First release.** Deltos is a native database client for Windows and Linux, written in Rust
  with GPUI: one binary drawn on the GPU, no browser engine.
- **PostgreSQL, MySQL and MariaDB, SQLite and MongoDB** from one connection list. Every
  database gets the same table view, the same editor and the same shortcuts; the driver for
  each lives in its own module behind one contract.
- **Tables of any size.** Rows are fetched as you scroll and kept in a bounded cache, so a table
  with hundreds of millions of rows opens as fast and uses as little memory as one with a
  hundred.
- **Panels you arrange.** Connections, schema, tables, query and results are panels. Drag one to
  a new place, resize it, or drop it outside the window to give it a window of its own, and
  dock it back when you are done.
- **Own title bar** on Windows and Linux, slim, with the window controls, for desktops that
  draw none or when you prefer it.
- **Appearance settings**: light and dark themes, accent color, interface and code fonts, zoom
  and spacing, each applied as you change it and saved as plain TOML.
- **Signed updates.** Deltos checks for a new release at start, downloads it in the background
  and installs it when you ask or when you quit. An update is installed only when the release's
  manifest carries a valid signature from the project's key and the download matches the hash
  in it. The check can be turned off in Settings.
- **Packages**: a `.deb`, an `.rpm`, an AppImage and a `.tar.gz` for Linux; a per-user setup
  program and a portable `.zip` for Windows; one-line install scripts for both.
- **Connections in a few keystrokes.** Paste a connection URL into the form and the fields
  fill themselves in; a URL on the clipboard is offered when you start a new connection. Pick
  the database from a row of tiles, type `host:port` in one go, move between fields with Tab,
  and show the password to check it. A test reports the server version and how long it took,
  and a failed one says what to check.
- **Ten palettes** for light and dark mode, any accent color with an optional tint across the
  window, title bar, tab and column header styles, high contrast text and a live preview of
  every change. Adwaita Sans and Go Mono are the new default fonts, and any installed font can
  be picked from a list.
- **Panels move like objects.** A dragged panel stretches with the pointer, a torn-off window
  stays under the pointer as it leaves on Windows and Linux, and a panel settles into place
  with a short squash and stretch when it docks or gets its own window.
