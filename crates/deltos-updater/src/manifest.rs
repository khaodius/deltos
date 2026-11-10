//! `latest.json`, the signed description of a release.
//!
//! ```json
//! {
//!   "version": "0.1.0",
//!   "pub_date": "2026-11-09T12:00:00Z",
//!   "notes_url": "https://github.com/khaodius/deltos/releases/tag/v0.1.0",
//!   "assets": [
//!     {
//!       "name": "deltos_0.1.0_amd64.deb",
//!       "url": "https://github.com/khaodius/deltos/releases/download/v0.1.0/deltos_0.1.0_amd64.deb",
//!       "sha256": "…",
//!       "size": 12345678
//!     }
//!   ]
//! }
//! ```
//!
//! The signature in `latest.json.sig` covers the file byte for byte, so the manifest is parsed
//! only after it verifies. Parsing still checks every field: the signer is trusted, but a
//! mistake in a release must fail here with a clear message rather than install something odd.

use serde::{Deserialize, Serialize};

use crate::{Error, Result, version};

/// One package of a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestAsset {
    /// The file name, which is also the name under which it is saved while updating.
    pub name: String,
    pub url: String,
    /// Lowercase hex SHA-256 of the file.
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The released version, without a `v`; it is the version in the release tag.
    pub version: String,
    /// When the release was published, as RFC 3339 in UTC.
    pub pub_date: String,
    /// Where to read what changed: the release page.
    pub notes_url: String,
    pub assets: Vec<ManifestAsset>,
}

impl Manifest {
    /// Parses and validates the manifest. Hashes are normalized to lowercase.
    pub fn parse(bytes: &[u8]) -> Result<Manifest> {
        let mut manifest: Manifest = serde_json::from_slice(bytes)
            .map_err(|e| Error::Manifest(format!("latest.json is not valid: {e}")))?;
        for asset in &mut manifest.assets {
            asset.sha256.make_ascii_lowercase();
        }
        manifest.validate()?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<()> {
        if version::parse(&self.version).is_empty()
            || !self.version.starts_with(|c: char| c.is_ascii_digit())
        {
            return Err(Error::Manifest(format!(
                "\"{}\" is not a version number",
                self.version
            )));
        }
        if self.pub_date.trim().is_empty() {
            return Err(Error::Manifest("pub_date is empty".into()));
        }
        if !self.notes_url.starts_with("https://") {
            return Err(Error::Manifest("notes_url is not an https URL".into()));
        }
        if self.assets.is_empty() {
            return Err(Error::Manifest("the release lists no packages".into()));
        }
        for asset in &self.assets {
            asset.validate(&self.version)?;
        }
        Ok(())
    }

    pub fn asset(&self, name: &str) -> Option<&ManifestAsset> {
        self.assets.iter().find(|a| a.name == name)
    }

    /// The manifest as the bytes that get signed and published: pretty JSON, one trailing
    /// newline.
    pub fn to_json(&self) -> Result<Vec<u8>> {
        let mut json = serde_json::to_vec_pretty(self)
            .map_err(|e| Error::Manifest(format!("could not write latest.json: {e}")))?;
        json.push(b'\n');
        Ok(json)
    }
}

impl ManifestAsset {
    fn validate(&self, version: &str) -> Result<()> {
        let name = &self.name;
        if name.is_empty()
            || name.contains(['/', '\\'])
            || name == "."
            || name == ".."
            || name.starts_with('.')
        {
            return Err(Error::Manifest(format!(
                "\"{name}\" is not a plain file name"
            )));
        }
        // Every package name carries the version, so a manifest that mixes releases, or whose
        // version does not match its tag, is caught here.
        if !name.contains(version) {
            return Err(Error::Manifest(format!(
                "{name} does not belong to version {version}"
            )));
        }
        if !self.url.starts_with("https://") || !self.url.ends_with(&format!("/{name}")) {
            return Err(Error::Manifest(format!(
                "the URL of {name} is not an https URL to that file"
            )));
        }
        if self.sha256.len() != 64 || !self.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Manifest(format!(
                "the SHA-256 of {name} is not 64 hex digits"
            )));
        }
        if self.size == 0 {
            return Err(Error::Manifest(format!("{name} is listed as empty")));
        }
        Ok(())
    }
}

/// Checks a finished download against what the manifest promised.
pub(crate) fn check_download(asset: &ManifestAsset, size: u64, sha256: &str) -> Result<()> {
    if size != asset.size {
        return Err(Error::SizeMismatch {
            name: asset.name.clone(),
            expected: asset.size,
            actual: size,
        });
    }
    if !sha256.eq_ignore_ascii_case(&asset.sha256) {
        return Err(Error::HashMismatch(asset.name.clone()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const SAMPLE: &str = r#"{
  "version": "0.2.0",
  "pub_date": "2026-11-09T12:00:00Z",
  "notes_url": "https://github.com/khaodius/deltos/releases/tag/v0.2.0",
  "assets": [
    {
      "name": "deltos-0.2.0-x64-setup.exe",
      "url": "https://github.com/khaodius/deltos/releases/download/v0.2.0/deltos-0.2.0-x64-setup.exe",
      "sha256": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
      "size": 1
    },
    {
      "name": "deltos_0.2.0_amd64.deb",
      "url": "https://github.com/khaodius/deltos/releases/download/v0.2.0/deltos_0.2.0_amd64.deb",
      "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      "size": 2
    }
  ]
}
"#;

    fn sample() -> Manifest {
        Manifest::parse(SAMPLE.as_bytes()).unwrap()
    }

    #[test]
    fn parses_and_normalizes() {
        let manifest = sample();
        assert_eq!(manifest.version, "0.2.0");
        assert_eq!(manifest.assets.len(), 2);
        let setup = manifest.asset("deltos-0.2.0-x64-setup.exe").unwrap();
        assert_eq!(setup.sha256, "a".repeat(64));
        assert_eq!(setup.size, 1);
        assert!(manifest.asset("deltos-0.2.0-x86_64.AppImage").is_none());
    }

    #[test]
    fn round_trips_through_json() {
        let manifest = sample();
        let json = manifest.to_json().unwrap();
        assert!(json.ends_with(b"}\n"));
        assert_eq!(Manifest::parse(&json).unwrap(), manifest);
    }

    fn with(edit: impl FnOnce(&mut Manifest)) -> Result<Manifest> {
        let mut manifest = sample();
        edit(&mut manifest);
        Manifest::parse(&serde_json::to_vec(&manifest).unwrap())
    }

    #[test]
    fn rejects_broken_manifests() {
        assert!(matches!(
            Manifest::parse(b"not json"),
            Err(Error::Manifest(_))
        ));
        assert!(matches!(Manifest::parse(b"{}"), Err(Error::Manifest(_))));
        assert!(with(|m| m.version = "latest".into()).is_err());
        assert!(with(|m| m.version = "v0.2.0".into()).is_err());
        assert!(with(|m| m.pub_date = String::new()).is_err());
        assert!(with(|m| m.notes_url = "http://example.com".into()).is_err());
        assert!(with(|m| m.assets.clear()).is_err());
    }

    #[test]
    fn rejects_broken_assets() {
        assert!(with(|m| m.assets[0].name = "../deltos-0.2.0.exe".into()).is_err());
        assert!(with(|m| m.assets[0].name = "sub/deltos-0.2.0.exe".into()).is_err());
        assert!(with(|m| m.assets[0].name = ".deltos-0.2.0".into()).is_err());
        // A package from another version, which also means the manifest and tag disagree.
        assert!(with(|m| m.assets[0].name = "deltos-0.1.9-x64-setup.exe".into()).is_err());
        assert!(
            with(|m| m.assets[0].url = "http://example.com/deltos-0.2.0-x64-setup.exe".into())
                .is_err()
        );
        assert!(with(|m| m.assets[0].url = "https://example.com/other.exe".into()).is_err());
        assert!(with(|m| m.assets[0].sha256 = "abc".into()).is_err());
        assert!(with(|m| m.assets[0].sha256 = "g".repeat(64)).is_err());
        assert!(with(|m| m.assets[0].size = 0).is_err());
    }

    #[test]
    fn checks_downloads_against_the_manifest() {
        let asset = sample().assets[1].clone();
        assert!(check_download(&asset, 2, &"b".repeat(64)).is_ok());
        assert!(check_download(&asset, 2, &"B".repeat(64)).is_ok());
        assert!(matches!(
            check_download(&asset, 3, &"b".repeat(64)),
            Err(Error::SizeMismatch {
                expected: 2,
                actual: 3,
                ..
            })
        ));
        let mut wrong = "b".repeat(63);
        wrong.push('c');
        assert!(matches!(
            check_download(&asset, 2, &wrong),
            Err(Error::HashMismatch(name)) if name == "deltos_0.2.0_amd64.deb"
        ));
    }
}
