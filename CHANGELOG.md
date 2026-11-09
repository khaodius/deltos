# Changelog

Packages for every version are attached to its
[release](https://github.com/khaodius/deltos/releases), with SHA-256 checksums and a signed
update manifest.

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
