//! Signed self-updates from GitHub Releases.
//!
//! Every release publishes `latest.json`, which names the version and each package with its
//! SHA-256 and size, and `latest.json.sig`, an Ed25519 signature over the exact bytes of that
//! file. The app embeds the public half of the signing key ([`EMBEDDED_PUBLIC_KEY`], read from
//! `packaging/update-key.pub` at compile time). An update is accepted only when the manifest's
//! signature verifies with that key, the manifest is newer than the running build, and the
//! downloaded package matches the size and hash the manifest lists. A build with no key
//! embedded cannot verify anything, so it does not update at all and says why; it never falls
//! back to unverified downloads.
//!
//! The app drives the updater in two ways, and both start the same:
//!
//! ```ignore
//! let updater = Updater::new(UpdaterConfig { .. });
//! updater.clean_up();                               // at start: remove the last update's leftovers
//! if let Some(update) = updater.check().await? {    // at start and every few hours
//!     let downloaded = updater.download(&update, |done, total| { .. }).await?;
//!     // Either now, when the person clicks "Restart to update":
//!     match updater.install(downloaded).await? {
//!         Applied::Restart(path) => { /* start `path`, then quit */ }
//!         Applied::Quit => { /* the installer restarts the app; quit */ }
//!     }
//!     // Or later, as the app quits:
//!     updater.install_on_exit(downloaded)?;
//! }
//! ```
//!
//! `check` and `download` only touch the network and a temporary folder; nothing changes on
//! the machine until `install` or `install_on_exit`.

use std::path::PathBuf;
use std::time::Duration;

use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;

mod install;
mod manifest;
mod signing;
mod version;

pub use install::{Applied, InstallKind, asset_name, detect_install};
pub use manifest::{Manifest, ManifestAsset};
pub use signing::{KeyError, PublicKey, SecretKey};
pub use version::is_newer;

/// The public update key compiled into this build, as the base64 text of
/// `packaging/update-key.pub`. Empty in a checkout without a key, which disables updates.
pub const EMBEDDED_PUBLIC_KEY: &str = include_str!("../../../packaging/update-key.pub");

/// The manifest's file name in a release.
pub const MANIFEST_NAME: &str = "latest.json";
/// The signature's file name in a release.
pub const SIGNATURE_NAME: &str = "latest.json.sig";

/// Manifests and signatures are small; anything larger is not one.
const MANIFEST_LIMIT: u64 = 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(900);

/// Where the manifest is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Channel {
    /// The latest GitHub release of the configured repository.
    Stable,
    /// A folder URL that holds `latest.json` and `latest.json.sig`, for mirrors and tests.
    Manifest(String),
}

#[derive(Debug, Clone)]
pub struct UpdaterConfig {
    /// `owner/name` on GitHub.
    pub repo: String,
    /// The running version, normally `env!("CARGO_PKG_VERSION")`.
    pub current_version: String,
    /// The base64 public key, normally [`EMBEDDED_PUBLIC_KEY`].
    pub public_key: String,
    /// The binary's name, which is also the prefix of every package name.
    pub app_name: String,
    pub channel: Channel,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// This build cannot update, and the message says why; show it where updates are offered.
    #[error("{0}")]
    Disabled(String),
    #[error("could not reach the release server: {0}")]
    Network(#[from] reqwest::Error),
    #[error("the update is not signed by this app's update key: {0}")]
    Signature(String),
    #[error("the update manifest is not usable: {0}")]
    Manifest(String),
    #[error("the release has no package {0} for this install")]
    NoAsset(String),
    #[error("{name} is {actual} bytes, but the release lists {expected}")]
    SizeMismatch {
        name: String,
        expected: u64,
        actual: u64,
    },
    #[error("{0} does not match the hash the release lists")]
    HashMismatch(String),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("could not install the update: {0}")]
    Install(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A newer release, with the one package that fits this install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub version: String,
    pub pub_date: String,
    pub notes_url: String,
    pub asset: ManifestAsset,
    pub install: InstallKind,
}

/// A package that was downloaded and matched its hash, waiting to be installed.
#[derive(Debug)]
pub struct Downloaded {
    pub version: String,
    pub install: InstallKind,
    /// The package file, inside `dir`.
    pub path: PathBuf,
    dir: PathBuf,
}

pub struct Updater {
    config: UpdaterConfig,
    key: std::result::Result<PublicKey, String>,
}

impl std::fmt::Debug for Updater {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Updater")
            .field("config", &self.config)
            .field("enabled", &self.key.is_ok())
            .finish()
    }
}

impl Updater {
    /// Never fails: a build without a usable key gets an updater whose
    /// [`disabled_reason`](Self::disabled_reason) explains why it does nothing.
    pub fn new(config: UpdaterConfig) -> Updater {
        let key = if config.public_key.trim().is_empty() {
            Err(
                "This build has no update signing key, so it cannot check that an update is \
                 genuine. Updates are off."
                    .to_owned(),
            )
        } else {
            PublicKey::parse(&config.public_key).map_err(|e| {
                format!("The update signing key built into this app is not valid ({e}), so updates are off.")
            })
        };
        Updater { config, key }
    }

    pub fn config(&self) -> &UpdaterConfig {
        &self.config
    }

    pub fn is_enabled(&self) -> bool {
        self.key.is_ok()
    }

    pub fn disabled_reason(&self) -> Option<&str> {
        self.key.as_ref().err().map(String::as_str)
    }

    fn key(&self) -> Result<&PublicKey> {
        self.key
            .as_ref()
            .map_err(|reason| Error::Disabled(reason.clone()))
    }

    fn client(&self) -> Result<reqwest::Client> {
        Ok(reqwest::Client::builder()
            .user_agent(format!(
                "{}/{}",
                self.config.app_name, self.config.current_version
            ))
            .connect_timeout(CONNECT_TIMEOUT)
            .build()?)
    }

    /// The folder URL that holds the manifest and its signature.
    pub fn manifest_base(&self) -> String {
        match &self.config.channel {
            Channel::Stable => format!(
                "https://github.com/{}/releases/latest/download",
                self.config.repo
            ),
            Channel::Manifest(base) => base.trim_end_matches('/').to_owned(),
        }
    }

    /// The newest release if it is newer than this build and has a package for this install.
    /// `Ok(None)` means there is nothing to do: no release yet, or none newer.
    pub async fn check(&self) -> Result<Option<Update>> {
        let key = self.key()?;
        let client = self.client()?;
        let base = self.manifest_base();
        let Some(manifest_bytes) = fetch_small(&client, &format!("{base}/{MANIFEST_NAME}")).await?
        else {
            tracing::debug!("no release manifest yet");
            return Ok(None);
        };
        let signature = fetch_small(&client, &format!("{base}/{SIGNATURE_NAME}"))
            .await?
            .ok_or_else(|| Error::Signature("the release has no latest.json.sig".into()))?;
        let signature = std::str::from_utf8(&signature)
            .map_err(|_| Error::Signature("latest.json.sig is not text".into()))?;
        // Verify before parsing: nothing in an unverified manifest is read.
        key.verify(&manifest_bytes, signature)?;
        let manifest = Manifest::parse(&manifest_bytes)?;
        if !is_newer(&manifest.version, &self.config.current_version) {
            tracing::debug!(
                latest = %manifest.version,
                current = %self.config.current_version,
                "no newer release"
            );
            return Ok(None);
        }
        let app_name = self.config.app_name.clone();
        let install = tokio::task::spawn_blocking(move || detect_install(&app_name))
            .await
            .map_err(|e| Error::Install(format!("could not detect the install: {e}")))?;
        let name = asset_name(install, &self.config.app_name, &manifest.version);
        let asset = manifest
            .asset(&name)
            .cloned()
            .ok_or_else(|| Error::NoAsset(name))?;
        tracing::info!(version = %manifest.version, ?install, package = %asset.name, "update available");
        Ok(Some(Update {
            version: manifest.version,
            pub_date: manifest.pub_date,
            notes_url: manifest.notes_url,
            asset,
            install,
        }))
    }

    /// Downloads the package into a temporary folder, calling `progress(received, total)` as
    /// bytes arrive, and checks its size and hash against the manifest. Nothing is kept when
    /// a check fails.
    pub async fn download(
        &self,
        update: &Update,
        progress: impl Fn(u64, u64) + Send,
    ) -> Result<Downloaded> {
        // A disabled build never downloads, even if handed an Update from elsewhere.
        self.key()?;
        let dir = std::env::temp_dir().join(format!(
            "{}-update-{}",
            self.config.app_name, update.version
        ));
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(&update.asset.name);
        match self.download_to(&update.asset, &path, progress).await {
            Ok(()) => Ok(Downloaded {
                version: update.version.clone(),
                install: update.install,
                path,
                dir,
            }),
            Err(e) => {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                Err(e)
            }
        }
    }

    async fn download_to(
        &self,
        asset: &ManifestAsset,
        path: &std::path::Path,
        progress: impl Fn(u64, u64) + Send,
    ) -> Result<()> {
        let client = self.client()?;
        let mut response = client
            .get(&asset.url)
            .timeout(DOWNLOAD_TIMEOUT)
            .send()
            .await?
            .error_for_status()?;
        let mut file = tokio::fs::File::create(path).await?;
        let mut hasher = Sha256::new();
        let mut received = 0u64;
        progress(0, asset.size);
        while let Some(chunk) = response.chunk().await? {
            received += chunk.len() as u64;
            if received > asset.size {
                // Stop as soon as the file is longer than promised rather than filling the disk.
                return Err(Error::SizeMismatch {
                    name: asset.name.clone(),
                    expected: asset.size,
                    actual: received,
                });
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
            progress(received, asset.size);
        }
        file.flush().await?;
        drop(file);
        let digest = hasher.finalize();
        manifest::check_download(asset, received, &hex(&digest))
    }

    /// Installs the package and says how the app should finish. Blocking work runs on Tokio's
    /// blocking pool; on Linux packages this includes the pkexec prompt.
    pub async fn install(&self, downloaded: Downloaded) -> Result<Applied> {
        let app_name = self.config.app_name.clone();
        tokio::task::spawn_blocking(move || {
            install::apply(&downloaded, &app_name, install::Relaunch::Now)
        })
        .await
        .map_err(|e| Error::Install(format!("the install task stopped: {e}")))?
    }

    /// Installs the package as the app quits, without starting it again. Call it after the
    /// windows are closed; it blocks until the files are in place (or, on Windows, until the
    /// setup program has been started, which then finishes on its own).
    pub fn install_on_exit(&self, downloaded: Downloaded) -> Result<()> {
        install::apply(
            &downloaded,
            &self.config.app_name,
            install::Relaunch::OnExit,
        )
        .map(|_| ())
    }

    /// Removes what the previous update left behind: the replaced binary and downloads. Call
    /// it once at start.
    pub fn clean_up(&self) {
        install::clean_up(&self.config.app_name);
    }
}

/// Fetches a small file in full, or `None` when the server has no such file.
async fn fetch_small(client: &reqwest::Client, url: &str) -> Result<Option<Vec<u8>>> {
    let mut response = client.get(url).timeout(MANIFEST_TIMEOUT).send().await?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    response = response.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|len| len > MANIFEST_LIMIT)
    {
        return Err(Error::Manifest(format!("{url} is too large")));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() as u64 + chunk.len() as u64 > MANIFEST_LIMIT {
            return Err(Error::Manifest(format!("{url} is too large")));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Some(bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(public_key: &str) -> UpdaterConfig {
        UpdaterConfig {
            repo: "khaodius/deltos".into(),
            current_version: "0.1.0".into(),
            public_key: public_key.into(),
            app_name: "deltos".into(),
            channel: Channel::Stable,
        }
    }

    #[test]
    fn an_empty_key_disables_updates_with_a_reason() {
        let updater = Updater::new(config(""));
        assert!(!updater.is_enabled());
        assert!(
            updater
                .disabled_reason()
                .unwrap()
                .contains("no update signing key")
        );
        assert!(matches!(updater.key(), Err(Error::Disabled(_))));
        let updater = Updater::new(config("\n"));
        assert!(!updater.is_enabled());
    }

    #[test]
    fn a_broken_key_disables_updates_with_a_reason() {
        let updater = Updater::new(config("not a key"));
        assert!(!updater.is_enabled());
        assert!(updater.disabled_reason().unwrap().contains("not valid"));
    }

    #[test]
    fn a_valid_key_enables_updates() {
        let key = SecretKey::from_seed([3; 32]).public_key().to_base64();
        let updater = Updater::new(config(&format!("{key}\n")));
        assert!(updater.is_enabled());
        assert_eq!(updater.disabled_reason(), None);
        assert_eq!(
            updater.manifest_base(),
            "https://github.com/khaodius/deltos/releases/latest/download"
        );
    }

    #[test]
    fn custom_channels_name_a_folder() {
        let mut config = config("");
        config.channel = Channel::Manifest("https://example.com/deltos/".into());
        assert_eq!(
            Updater::new(config).manifest_base(),
            "https://example.com/deltos"
        );
    }

    #[tokio::test]
    async fn a_disabled_updater_never_checks_or_downloads() {
        let updater = Updater::new(config(""));
        assert!(matches!(updater.check().await, Err(Error::Disabled(_))));
        let update = Update {
            version: "0.2.0".into(),
            pub_date: "2026-11-09T12:00:00Z".into(),
            notes_url: "https://github.com/khaodius/deltos/releases/tag/v0.2.0".into(),
            asset: ManifestAsset {
                name: "deltos_0.2.0_amd64.deb".into(),
                url: "https://github.com/khaodius/deltos/releases/download/v0.2.0/deltos_0.2.0_amd64.deb".into(),
                sha256: "a".repeat(64),
                size: 1,
            },
            install: InstallKind::Deb,
        };
        assert!(matches!(
            updater.download(&update, |_, _| {}).await,
            Err(Error::Disabled(_))
        ));
    }

    #[test]
    fn embedded_key_is_text() {
        // Either empty (no key committed yet) or a key that parses.
        if !EMBEDDED_PUBLIC_KEY.trim().is_empty() {
            PublicKey::parse(EMBEDDED_PUBLIC_KEY).unwrap();
        }
    }

    #[test]
    fn hex_is_lowercase() {
        assert_eq!(hex(&[0, 15, 255]), "000fff");
    }
}
