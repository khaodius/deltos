# Working on Deltos

Notes for anyone, person or coding agent, changing this repository. They cover how the
code is laid out, what CI checks, how a release goes out, and the rules the owner set.
Read this before your first commit.

## Ground rules from the owner

- **Authorship.** Commits are authored as the owner and carry nothing else:
  `git -c user.name=khaodius -c user.email=72529959+khaodius@users.noreply.github.com commit ...`.
  - No `Co-Authored-By`, `Signed-off-by` or session trailers.
  - No "generated with" lines, tool names or model names. This applies to commit messages,
    PR titles and bodies, tags, release notes, code comments and logs.
  - If a tool adds a footer to a PR body, remove it.
- **Releases come from `main`.** Pull before you push: other work lands on `main` often.
- **One version per change set.** Before you release, check that nobody has already taken
  the next version number (see [Versions](#versions)).
- **CI stays green on `main`.** Run the same checks locally before pushing (see
  [Local checks](#local-checks)). Never skip, ignore or delete a test to get green.
- **Any table size.** Deltos must open a table of hundreds of millions of rows with bounded
  memory and a responsive window. Never read a result set to its end, never keep more rows
  than the cache allows, and keep per-cell allocations off the render path.
- **No panics on input.** Nothing read from the network, a file or a database is unwrapped.
  Drivers and parsers return errors; the UI shows them.

## Layout

A Cargo workspace. Everything is Rust, and the UI is [GPUI](https://www.gpui.rs) 0.2.2.

| Crate | What it holds |
| --- | --- |
| `crates/deltos` | The app: windows, the title bar, the panel workspace, the settings screen, and the glue between drivers, grid and UI |
| `crates/deltos-data` | The driver contract every database driver implements: connections, schema, queries, row streams and `Value`. Change it with care; every driver follows it |
| `crates/deltos-drivers` | The drivers: PostgreSQL, MySQL and MariaDB, SQLite and MongoDB. Integration tests run against live servers in CI when the `DELTOS_TEST_*_URL` variables are set, and skip otherwise |
| `crates/deltos-grid` | The bounded row cache and on-disk spill store behind the table view, which is what keeps memory flat on huge tables |
| `crates/deltos-dock` | The dock layout model: splits, tabs and floating panel windows, serialized so a layout survives restarts |
| `crates/deltos-settings` | `Settings` and its TOML file. Serde defaults must keep old files loading |
| `crates/deltos-updater` | Signed self-updates: the manifest, Ed25519 verification, download and install per package type, and the `deltos-sign` binary the release workflow uses |
| `third_party/` | Patched xattr (see its README) |

## Building

Rust 1.88 or newer (`rust-version` in `Cargo.toml`; CI checks it). On Ubuntu 22.04 or 24.04:

```sh
sudo apt-get install -y pkg-config libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
  libx11-xcb-dev libxcb1-dev libfontconfig-dev libfreetype-dev libvulkan-dev libgbm-dev libclang-dev
cargo build -p deltos
```

If you are short on disk, set `CARGO_INCREMENTAL=0`; the debug target directory grows large
with incremental builds.

The Windows build needs the MSVC toolchain. Windows-only code (`#[cfg(windows)]`) is verified
by CI only. Keep it small and simple to review.

## Local checks

These are exactly what CI runs. Run all four before every push:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo +1.88 check --workspace --locked    # the minimum Rust version job
```

- Clippy runs on the newest stable and treats warnings as errors. New lints appear with each
  Rust release.
- If the 1.88 check fails because a dependency raised its own minimum Rust version, pin that
  dependency to its last compatible version and run `cargo update -p <crate> --precise <version>`.
- Run `cargo fmt --all` after every edit, including small ones. A formatting failure is the
  most common way `main` turns red.
- The driver tests against real servers need the databases running. The MySQL tests create and drop
  their own databases, so they connect as root. With Docker:

  ```sh
  docker run -d -p 5432:5432 -e POSTGRES_USER=deltos -e POSTGRES_PASSWORD=deltos -e POSTGRES_DB=deltos postgres:16
  docker run -d -p 3306:3306 -e MYSQL_ROOT_PASSWORD=root -e MYSQL_USER=deltos -e MYSQL_PASSWORD=deltos -e MYSQL_DATABASE=deltos mysql:8.4
  docker run -d -p 27017:27017 mongo:7
  DELTOS_TEST_POSTGRES_URL=postgres://deltos:deltos@localhost:5432/deltos \
  DELTOS_TEST_MYSQL_URL=mysql://root:root@localhost:3306/deltos \
  DELTOS_TEST_MONGODB_URL=mongodb://localhost:27017 \
  cargo test -p deltos-drivers
  ```

## CI workflows

All are in `.github/workflows/`.

| Workflow | Runs on | Does |
| --- | --- | --- |
| `ci.yml` | push to `main`, every PR, manual | Linux and Windows: fmt, clippy `-D warnings`, tests. A separate job checks Rust 1.88. A `drivers` job runs `cargo test -p deltos-drivers` against PostgreSQL 16, MySQL 8.4, MariaDB 11 and MongoDB 7 service containers. Then it builds release binaries and uploads them as artifacts. Pull requests run only the Linux check, plus the driver tests when `deltos-data`, `deltos-drivers` or `Cargo.lock` changed; Windows, Rust 1.88 and the builds run on `main`. Changes to Markdown and `docs/` alone run nothing. Compiled dependencies and apt packages are cached, and only `main` saves caches |
| `release.yml` | push to `main` that touches `Cargo.toml` or the workflow, a `v*` tag, manual | Publishes a release when the workspace version has no tag yet (details below). Restores the dependency cache CI's release build saved on `main`, so it compiles only Deltos' own crates |
| `install-scripts.yml` | changes to `scripts/` or the AppImage script | ShellCheck and PSScriptAnalyzer, then installs the latest release on Linux (user install, `.deb`, `.rpm` on Fedora) and Windows |

CI cancels an older run on the same branch when a newer push arrives. A cancelled run shows
as failing on the badge until the next run finishes. The README badge only tracks `main`.

To watch a run: `gh run list --limit 5`, then `gh run watch <id> --exit-status`. To read the
failure: `gh run view <id> --log-failed`.

## Releasing

Releases are automatic from `main`:

1. Bump `version` under `[workspace.package]` in the root `Cargo.toml`.
2. Run `cargo check` so `Cargo.lock` picks up the new version. The release job fails if the
   two disagree.
3. Add a `## X.Y.Z - YYYY-MM-DD` section at the top of `CHANGELOG.md`. The release job fails
   without one, and the release notes are copied from it.
4. Commit and push to `main`, or merge a PR into it.

`release.yml` then does the following:

- checks the version, lock file and changelog, and that the update signing key is set up;
- builds the packages on clean runners:
  - Linux: `.deb`, `.rpm`, an AppImage and a `.tar.gz` for user installs;
  - Windows: an Inno Setup installer and a portable `.zip`;
- writes `SHA256SUMS.txt`;
- writes the update manifest `latest.json`, signs it with the `DELTOS_UPDATE_KEY` secret and
  verifies the signature against `packaging/update-key.pub`;
- attests build provenance;
- tags the commit `vX.Y.Z` and publishes the GitHub release.

A push whose version is already released does nothing, so pushing fixes after a release is
safe. The in-app updater (`crates/deltos-updater`) reads `latest.json` from the latest release
and picks the package by name, so asset names must stay as they are.
[docs/RELEASING.md](docs/RELEASING.md) has the full procedure and the signing key setup.

To check that a release went out:
`gh api repos/khaodius/deltos/releases/tags/vX.Y.Z --jq '.tag_name, (.assets | length)'`.
There should be 9 assets. The GraphQL API is not available from every environment, so use
REST.

### Versions

- Several pieces of work often go on at once. Before bumping, run
  `git fetch origin main && git show origin/main:Cargo.toml | grep -m1 ^version` and check
  open PRs for a bump of their own: `gh pr list`, or
  `gh api repos/khaodius/deltos/pulls --jq '.[].head.ref'` where GraphQL is blocked.
- Take the next free number. Don't reuse one that has been released or that an open PR has
  claimed.
- If your branch conflicts on `Cargo.toml`, `Cargo.lock` or `CHANGELOG.md` after a rebase:
  - take `main`'s files;
  - set your version again;
  - run `cargo check`;
  - put your changelog section on top.
- Write each changelog entry as one bold lead phrase and plain sentences that say what
  changed for the user. Follow the style of the existing entries.

### The update signing key

The updater trusts one Ed25519 key. `packaging/update-key.pub` holds the public half and is
compiled into every build; the private half is only the `DELTOS_UPDATE_KEY` repository secret.
A build with an empty public key file has updates off and says so; it never installs anything
unverified. Make the pair once with `cargo run -p deltos-updater --bin deltos-sign -- keygen`,
store the printed private key as the secret, and commit the public key. Replacing the key
strands every installed copy, so do it only after a leak. Details in
[docs/RELEASING.md](docs/RELEASING.md).

## GPUI pitfalls seen in this code

- `on_mouse_move` and `capture_any_mouse_up` fire only while the element is hovered. To
  follow a drag past an element or out of the window, register `window.on_mouse_event`
  from a `canvas` paint callback.
- A bare `div` is block layout. A `flex_1` child inside it with a fixed-width sibling can be
  measured at its narrowest, and its text then wraps one letter per line. Give list rows
  `w_full()`, and make wrappers `flex().flex_col()`.
- `ListState` and uniform lists keep measured heights. Reset when something that changes row
  height changes, such as a setting, and not on every event.
- Children are not clipped to rounded corners, so round the element itself.
- Windows cannot add or remove the system title bar on an open window; reopen the window
  when that setting changes there.
- `remove_window` does not call `on_window_should_close`.
- Redraw only what changed. A grid that notifies on every fetched page redraws the whole
  window; notify once per visible range instead.

## Tests

- Unit tests sit next to the code (`#[cfg(test)] mod tests`).
- Driver tests that need a server read their connection string from `DELTOS_TEST_*_URL` and
  skip when it is unset, so `cargo test --workspace` passes offline.
- When you fix a crash, add the input that caused it as a test.
- `deltos-updater` tests verification end to end with keys made in the test; nothing in its
  tests touches the network.
