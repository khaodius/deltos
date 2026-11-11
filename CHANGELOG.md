# Changelog

Packages for every version are attached to its
[release](https://github.com/khaodius/deltos/releases), with SHA-256 checksums and a signed
update manifest.

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
