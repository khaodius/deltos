# Deltos

A native database client for PostgreSQL, MySQL, MariaDB, SQLite and MongoDB, on Windows and Linux.

[![CI](https://github.com/khaodius/deltos/actions/workflows/ci.yml/badge.svg?branch=main&event=push)](https://github.com/khaodius/deltos/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/khaodius/deltos?display_name=tag&sort=semver)](https://github.com/khaodius/deltos/releases/latest)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

![Deltos showing a 50 million row table, the object sidebar and the row inspector](docs/images/screenshot.png)

Deltos is written in Rust and drawn on the GPU with [GPUI](https://www.gpui.rs). It ships as
one small binary, starts in well under a second, and keeps its memory flat however big the
table you open. There is no browser engine inside it.

[Download](https://github.com/khaodius/deltos/releases/latest) ·
[Changelog](CHANGELOG.md) ·
[Build from source](#build-from-source) ·
[Themes](docs/THEMES.md)

## Databases

| Database           | Browse and filter | Edit rows | Query editor | Cancel a running query |
| ------------------ | :---------------: | :-------: | :----------: | :--------------------: |
| PostgreSQL         | yes               | yes       | SQL          | yes                    |
| MySQL and MariaDB  | yes               | yes       | SQL          | yes                    |
| SQLite             | yes               | yes       | SQL          | yes                    |
| MongoDB            | yes               | yes       | Shell syntax | yes                    |

Cassandra, Redis, ClickHouse and SQL Server are next. Every driver implements the same
contract in [`deltos-data`](crates/deltos-data), so the table view, editor and shortcuts work
the same everywhere.

## Panels

![Dragging a panel out of the main window and docking it back](docs/images/panels.gif)

Every part of the window is a panel: connections, the object sidebar, tables, queries,
results, the inspector, history and settings. Drag a tab to split a pane or join another
stack. Pull it past the window's edge and it becomes a window of its own, which stays under
the pointer until you let go. Hold that window over the main one and it docks again. The
layout is saved and comes back on the next start.

## Large tables

Rows are fetched page by page as you scroll and kept in a cache with a fixed memory budget.
A table with 300 million rows opens as fast as one with 300. Paging uses keyset seeks where
the table has a key, so the end of a big table is as quick to reach as the start. Sorting and
filtering run in the database. Query results larger than memory spill to a temporary file
instead of filling RAM.

## Connecting

Paste a connection URL such as `mysql://user:pass@db.example.com:3306/shop` anywhere in the
connection form and the fields fill themselves in. Deltos also offers a URL it finds on the
clipboard. Passwords go to the system keychain, never to the settings file. When a test
fails, the form says why it most likely failed: wrong credentials, a server that only
listens on localhost, or a firewall.

## Appearance

Ten palettes, four light and six dark, chosen separately so following the system switches
between your two. Any accent color, with an optional tint across the window. Title bar,
tab and column header styles. Density, corner radius, separators and zoom. A live preview
shows each change as you make it. The default fonts are Adwaita Sans and Go Mono, both
bundled. You can also pick any font installed on your system, or load your own colors from
a [theme file](docs/THEMES.md).

## Install

**Windows** (PowerShell):

```powershell
irm https://raw.githubusercontent.com/khaodius/deltos/main/scripts/install.ps1 | iex
```

**Linux**:

```sh
curl -fsSL https://raw.githubusercontent.com/khaodius/deltos/main/scripts/install.sh | bash
```

Both check the download against the release's `SHA256SUMS.txt`.

- **Windows**: the script installs for your user, with no admin prompt.
- **Linux**: the script uses apt or dnf where it can. Otherwise it puts the binary in
  `~/.local/bin`.

Deltos then updates itself. Each update is signed, and Deltos verifies the signature before
installing it. You can turn automatic updates off in Settings.

<details>
<summary>Packages, older versions and verifying downloads</summary>

Every [release](https://github.com/khaodius/deltos/releases/latest) has:

| Platform | Files                                         |
| -------- | --------------------------------------------- |
| Windows  | `-setup.exe` installer, portable `.zip`       |
| Linux    | `.deb`, `.rpm`, `.AppImage`, `.tar.gz`        |

To install a specific version, pass it to the script:

```sh
curl -fsSL https://raw.githubusercontent.com/khaodius/deltos/main/scripts/install.sh | bash -s -- 0.1.0
```

```powershell
& ([scriptblock]::Create((irm https://raw.githubusercontent.com/khaodius/deltos/main/scripts/install.ps1))) 0.1.0
```

From Command Prompt on Windows:

```bat
curl -fsSL https://raw.githubusercontent.com/khaodius/deltos/main/scripts/install.cmd -o install.cmd && install.cmd && del install.cmd
```

To verify a download:

```sh
sha256sum --check --ignore-missing SHA256SUMS.txt     # Linux
gh attestation verify <file> --repo khaodius/deltos  # build provenance
```

The Windows builds are not code-signed yet, so SmartScreen may warn about an unknown
publisher the first time.

</details>

## Keyboard

| Action                      | Keys               |
| --------------------------- | ------------------ |
| Run the statement           | `Ctrl+Enter`       |
| New query tab               | `Ctrl+T`           |
| Close tab                   | `Ctrl+W`           |
| Next / previous tab         | `Ctrl+Tab` / `Ctrl+Shift+Tab` |
| Search tables and views     | `Ctrl+P`           |
| Reload                      | `Ctrl+R`           |
| Sidebar / inspector         | `Ctrl+B` / `Ctrl+I` |
| Settings                    | `Ctrl+,`           |
| Zoom in / out / reset       | `Ctrl+=` / `Ctrl+-` / `Ctrl+0` |
| Reset the panel layout      | `Ctrl+Shift+0`     |

Every binding can be changed in Settings → Keyboard.

## Build from source

You need [Rust](https://rustup.rs) 1.88 or newer.

On Windows, you also need the Visual Studio Build Tools with the C++ workload, which brings
the Windows SDK.

On Linux, you also need the development packages for xkbcommon, Wayland, X11, fontconfig and
Vulkan:

```sh
sudo apt install pkg-config libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libx11-xcb-dev \
  libxcb1-dev libfontconfig-dev libfreetype-dev libvulkan-dev libgbm-dev libclang-dev
```

Then:

```sh
git clone https://github.com/khaodius/deltos.git
cd deltos
cargo run --release
```

`DELTOS_DEMO=1 cargo run` opens a built-in demo database with a 50 million row table, so you
can try Deltos without a server.

## Contributing

Read [AGENTS.md](AGENTS.md) first. It covers the crate layout, the checks CI runs and how
releases are made. [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) explains how drivers, the row
cache and the dock fit together.

## License

[Apache License 2.0](LICENSE). The bundled fonts keep their own licenses, listed in
[assets/fonts](assets/fonts/README.md).
