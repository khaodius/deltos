# Releasing Deltos

Releases are built and published by `.github/workflows/release.yml` from `main`. This page
covers what you do, what the workflow checks, and the one-time setup of the update signing key.

## Publishing a version

1. Bump `version` under `[workspace.package]` in the root `Cargo.toml`. Check first that nobody
   has taken the number: `git fetch origin main && git show origin/main:Cargo.toml | grep -m1
   ^version`, and look at open PRs.
2. Run `cargo check` so `Cargo.lock` records the new version. The workflow fails if the two
   disagree.
3. Add a `## X.Y.Z - YYYY-MM-DD` section at the top of `CHANGELOG.md`. The release notes are
   copied from it, so write it for the people who use Deltos: one bold lead phrase per entry,
   then plain sentences that say what changed for them.
4. Commit and push to `main`, or merge a PR into it.

The workflow runs on every push to `main` that touches `Cargo.toml`. When the version has no
`vX.Y.Z` tag yet, it:

- checks the version, the lock file and the changelog, and that the signing key is set up;
- builds the packages on clean runners, with no build cache:
  - Linux (Ubuntu 22.04): `deltos_X.Y.Z_amd64.deb`, `deltos-X.Y.Z-1.x86_64.rpm`,
    `deltos-X.Y.Z-x86_64.AppImage` and `deltos-X.Y.Z-x86_64-linux.tar.gz`;
  - Windows: `deltos-X.Y.Z-x64-setup.exe` (Inno Setup, per user) and
    `deltos-X.Y.Z-x64-windows.zip` (portable);
- writes `SHA256SUMS.txt`;
- writes `latest.json`, the update manifest, signs it into `latest.json.sig` and verifies the
  signature against `packaging/update-key.pub`;
- attests build provenance when the repository is public;
- tags the commit `vX.Y.Z` and publishes the GitHub release, titled "Deltos X.Y.Z", with the
  changelog section and a download table as its notes.

A push whose version is already released does nothing, so pushing fixes after a release is
safe. Pushing a `vX.Y.Z` tag by hand, or running the workflow manually, publishes that version
as well, as long as the tag matches the version in `Cargo.toml`.

The asset names are part of the contract with the in-app updater and the install scripts
(`deltos_updater::asset_name`, `scripts/install.*`), so they must not change.

To check that a release went out:

```sh
gh api repos/khaodius/deltos/releases/tags/vX.Y.Z --jq '.tag_name, (.assets | length)'
```

There should be 9 assets: six packages, `SHA256SUMS.txt`, `latest.json` and `latest.json.sig`.

## The update signing key

The in-app updater trusts exactly one Ed25519 key. Its public half is committed as
`packaging/update-key.pub` and compiled into every build (`deltos_updater::EMBEDDED_PUBLIC_KEY`);
its private half is the `DELTOS_UPDATE_KEY` repository secret and exists nowhere else. The
release workflow signs `latest.json` with the secret, then verifies the signature with the
committed public key, so a mismatch stops the release instead of publishing an update that
every installed copy would reject.

A build whose `packaging/update-key.pub` is empty has updates turned off, and says so where
updates are offered. It never falls back to unverified downloads.

### One-time setup

1. Make the key pair. The private key is printed once, to stdout, and the public key is written
   to `packaging/update-key.pub`:

   ```sh
   cargo run -p deltos-updater --bin deltos-sign -- keygen
   ```

   `keygen` refuses to overwrite a file that already holds a key; see [Rotating the
   key](#rotating-the-key) before passing `--force`.

2. Store the private key as the repository secret, straight from the command so it never sits
   in a shell history or a file:

   ```sh
   cargo run -q -p deltos-updater --bin deltos-sign -- keygen --force | gh secret set DELTOS_UPDATE_KEY --repo khaodius/deltos
   ```

   (Run this instead of step 1, or paste the key from step 1 into **Settings → Secrets and
   variables → Actions → New repository secret**. Line breaks in a pasted key are harmless;
   `deltos-sign` ignores whitespace.)

3. Commit `packaging/update-key.pub`. From then on every build verifies updates with it.

4. Run the Release workflow by hand (**Actions → Release → Run workflow**) to publish the
   current version. Until both halves exist, pushes to `main` publish nothing and the Version
   job says so with a warning; with only one half in place it fails.

To check that the secret and the file belong together without starting a release, run the
workflow's own test locally:

```sh
DELTOS_UPDATE_KEY=... cargo run -q -p deltos-updater --bin deltos-sign -- public-key
cat packaging/update-key.pub
```

The two lines must be identical.

### Rotating the key

Installed copies trust only the key they were built with. If the private key is replaced,
every copy built with the old public key stops accepting updates and has to be reinstalled from
a download. So:

- rotate only if the private key may have leaked;
- then regenerate with `deltos-sign keygen --force`, set the new secret, commit the new public
  key, and tell users of the old versions to reinstall once;
- never publish a release signed with a key other than the committed one (the workflow refuses).

### What the tool does

`deltos-sign` lives in `crates/deltos-updater/src/bin/deltos-sign.rs` and is built by the
publish job with `cargo build --release -p deltos-updater --bin deltos-sign`:

| Command | Does |
| --- | --- |
| `keygen [--public-key-file PATH] [--force]` | Prints a new private key, writes the public key |
| `public-key` | Prints the public key that belongs to `DELTOS_UPDATE_KEY` |
| `manifest --version V --notes-url URL --base-url URL --output latest.json FILE...` | Writes the manifest: version, publish date, notes URL, and each file's name, URL, SHA-256 and size |
| `sign FILE [--output PATH]` | Writes the base64 Ed25519 signature over the exact bytes of `FILE` |
| `verify FILE SIGNATURE PUBLIC_KEY_FILE` | Exits 0 only if the signature verifies; an empty key file is an error |

## How the updater decides

At start, and every few hours while the setting is on, Deltos downloads `latest.json` and
`latest.json.sig` from the latest release, verifies the signature, and parses the manifest only
afterwards. The manifest's version must be strictly newer than the running one; an equal or
older version is never installed, so a stale or replayed manifest does nothing. It then picks
the one package for how this copy was installed, downloads it to a temporary folder, and
accepts it only when its size and SHA-256 match the manifest:

| Installed as | Detected by | Package | Installed by |
| --- | --- | --- | --- |
| Windows setup | `unins000.exe` next to `deltos.exe` | `-x64-setup.exe` | the setup program with `/VERYSILENT`, which restarts Deltos (`/RESTARTDELTOS=1`) |
| Windows portable | no uninstaller | `-x64-windows.zip` | unpack, replace `deltos.exe` by rename, restart |
| AppImage | the `APPIMAGE` variable | `-x86_64.AppImage` | write beside the image, rename over it, restart |
| `.deb` | binary under `/usr`, owned by dpkg | `_amd64.deb` | `pkexec apt-get install`, which asks for the password |
| `.rpm` | binary under `/usr`, owned by rpm | `-1.x86_64.rpm` | `pkexec dnf install` (or zypper, rpm) |
| user install | anything else | `-x86_64-linux.tar.gz` | unpack, replace `~/.local/bin/deltos` by rename, restart |

The app installs either when the person clicks (`Updater::install`, then restarts as
`Applied` says) or as it quits (`Updater::install_on_exit`). `Updater::clean_up` at the next
start removes the replaced binary and the download.

## Trying a release flow locally

The Linux packages can be built on Ubuntu 22.04 or newer the way the workflow builds them:

```sh
cargo install cargo-deb cargo-generate-rpm
cargo build --release --locked -p deltos
cargo deb -p deltos --no-build --output dist/deltos_0.1.0_amd64.deb
cargo generate-rpm -p crates/deltos --output dist/deltos-0.1.0-1.x86_64.rpm
packaging/linux/appimage.sh 0.1.0            # needs desktop-file-utils
```

The Windows installer needs Inno Setup 6:

```powershell
iscc /DVersion=0.1.0 /DSource=$PWD\target\release packaging\windows\deltos.iss
```

A manifest can be written and signed against a throwaway key to see the files the release
will carry:

```sh
key=$(cargo run -q -p deltos-updater --bin deltos-sign -- keygen --public-key-file /tmp/test-key.pub)
cargo run -q -p deltos-updater --bin deltos-sign -- manifest --version 0.1.0 \
  --notes-url https://github.com/khaodius/deltos/releases/tag/v0.1.0 \
  --base-url https://github.com/khaodius/deltos/releases/download/v0.1.0 \
  --output /tmp/latest.json dist/*
DELTOS_UPDATE_KEY=$key cargo run -q -p deltos-updater --bin deltos-sign -- sign /tmp/latest.json
cargo run -q -p deltos-updater --bin deltos-sign -- verify /tmp/latest.json /tmp/latest.json.sig /tmp/test-key.pub
```
