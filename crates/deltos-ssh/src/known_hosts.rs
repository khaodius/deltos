//! Checking a server's host key against OpenSSH `known_hosts` files, and adding one.
//!
//! Lines are read the way OpenSSH reads them: comma-separated host patterns with `*`,
//! `?` and `!`, `[host]:port` for ports other than 22, hashed `|1|salt|hash` names, and
//! the `@revoked` marker. `@cert-authority` lines are skipped, since certificates are
//! not checked here. A key type this code does not know never stops the check; keys are
//! compared by their encoded form.

use std::io::Write;
use std::path::{Path, PathBuf};

use data_encoding::BASE64;
use hmac::{Hmac, KeyInit, Mac};
use russh::keys::PublicKey;
use sha1::Sha1;

use crate::config::{glob, home_dir};

/// What the known hosts files say about a server's key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyStatus {
    /// The key is listed for this host.
    Known,
    /// Nothing is listed for this host with this key type.
    Unknown,
    /// Another key of the same type is listed: the server changed its key, or someone
    /// is in between.
    Changed { path: PathBuf, line: usize },
    /// The key is marked `@revoked`.
    Revoked,
}

/// The person's own file, which new keys are added to.
pub fn user_file() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".ssh").join("known_hosts"))
}

/// The files checked, in order: the person's own, then the system-wide one.
pub fn default_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = user_file().into_iter().collect();
    if cfg!(windows) {
        if let Ok(data) = std::env::var("ProgramData") {
            files.push(Path::new(&data).join("ssh").join("ssh_known_hosts"));
        }
    } else {
        files.push(PathBuf::from("/etc/ssh/ssh_known_hosts"));
    }
    files
}

/// The name a host is listed under: `host`, or `[host]:port` when the port is not 22.
pub fn host_label(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

/// Checks `key` for `host:port` in `files`. Missing files are skipped.
pub fn check(files: &[PathBuf], host: &str, port: u16, key: &PublicKey) -> HostKeyStatus {
    let (algorithm, encoded) = encode(key);
    let label = host_label(host, port);
    let mut changed = None;
    for path in files {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            let Some(entry) = Entry::parse(line) else {
                continue;
            };
            if entry.marker == Marker::CertAuthority || !entry.matches(&label) {
                continue;
            }
            let same_key = entry.key == encoded;
            match entry.marker {
                Marker::Revoked if same_key => return HostKeyStatus::Revoked,
                Marker::Revoked => {}
                _ if same_key => return HostKeyStatus::Known,
                _ if entry.algorithm == algorithm && changed.is_none() => {
                    changed = Some(HostKeyStatus::Changed {
                        path: path.clone(),
                        line: n + 1,
                    });
                }
                _ => {}
            }
        }
    }
    changed.unwrap_or(HostKeyStatus::Unknown)
}

/// Adds `key` for `host:port` to the end of `path`, creating the file and its folder
/// as OpenSSH would (readable only by the person on Unix).
pub fn learn(path: &Path, host: &str, port: u16, key: &PublicKey) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true).read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let needs_newline = std::fs::read(path)
        .map(|bytes| bytes.last().is_some_and(|b| *b != b'\n'))
        .unwrap_or(false);
    let (algorithm, encoded) = encode(key);
    let mut line = String::new();
    if needs_newline {
        line.push('\n');
    }
    line.push_str(&format!(
        "{} {algorithm} {encoded}\n",
        host_label(host, port)
    ));
    file.write_all(line.as_bytes())
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// The key's type name and base64 body, as they appear in a known hosts line.
fn encode(key: &PublicKey) -> (String, String) {
    let text = key.to_openssh().unwrap_or_default();
    let mut parts = text.split_whitespace();
    let algorithm = parts.next().unwrap_or_default().to_string();
    let body = parts.next().unwrap_or_default().to_string();
    (algorithm, body)
}

#[derive(Debug, PartialEq, Eq)]
enum Marker {
    None,
    Revoked,
    CertAuthority,
}

struct Entry<'a> {
    marker: Marker,
    hosts: &'a str,
    algorithm: &'a str,
    key: &'a str,
}

impl<'a> Entry<'a> {
    fn parse(line: &'a str) -> Option<Self> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let mut parts = line.split_whitespace();
        let mut first = parts.next()?;
        let marker = match first {
            "@revoked" => Marker::Revoked,
            "@cert-authority" => Marker::CertAuthority,
            m if m.starts_with('@') => return None,
            _ => Marker::None,
        };
        if marker != Marker::None {
            first = parts.next()?;
        }
        Some(Self {
            marker,
            hosts: first,
            algorithm: parts.next()?,
            key: parts.next()?,
        })
    }

    /// Whether the host field names `label`: a hashed name that hashes to it, or a
    /// pattern list where something matches and nothing negated does.
    fn matches(&self, label: &str) -> bool {
        if let Some(hashed) = self.hosts.strip_prefix("|1|") {
            return hashed_matches(hashed, label);
        }
        let mut matched = false;
        for pattern in self.hosts.split(',') {
            if let Some(negated) = pattern.strip_prefix('!') {
                if glob(negated, label) {
                    return false;
                }
            } else if glob(pattern, label) {
                matched = true;
            }
        }
        matched
    }
}

fn hashed_matches(hashed: &str, label: &str) -> bool {
    let Some((salt, hash)) = hashed.split_once('|') else {
        return false;
    };
    let (Ok(salt), Ok(hash)) = (
        BASE64.decode(salt.as_bytes()),
        BASE64.decode(hash.as_bytes()),
    ) else {
        return false;
    };
    let Ok(mac) = Hmac::<Sha1>::new_from_slice(&salt) else {
        return false;
    };
    mac.chain_update(label.as_bytes())
        .verify_slice(&hash)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    use russh::keys::ssh_key::public::Ed25519PublicKey;

    fn key(byte: u8) -> PublicKey {
        PublicKey::from(Ed25519PublicKey([byte; 32]))
    }

    /// The key as a known hosts line ends: type and body.
    fn line(byte: u8) -> String {
        let (algorithm, body) = encode(&key(byte));
        format!("{algorithm} {body}")
    }

    fn file(name: &str, text: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deltos-kh-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("known_hosts");
        std::fs::write(&path, text).expect("write");
        path
    }

    #[test]
    fn finds_known_changed_and_unknown_keys() {
        let (ed, other) = (line(1), line(2));
        let path = file(
            "basic",
            &format!(
                "# comment\ndb.example.com,10.0.0.1 {ed}\n[db.example.com]:2222 {other}\n*.lan {ed}\nweird.host sk-made-up AAAA\n"
            ),
        );
        let files = vec![path.clone()];
        let k = key(1);
        assert_eq!(
            check(&files, "db.example.com", 22, &k),
            HostKeyStatus::Known
        );
        assert_eq!(check(&files, "10.0.0.1", 22, &k), HostKeyStatus::Known);
        assert_eq!(check(&files, "box.lan", 22, &k), HostKeyStatus::Known);
        assert_eq!(
            check(&files, "db.example.com", 2222, &k),
            HostKeyStatus::Changed { path, line: 3 }
        );
        assert_eq!(check(&files, "other", 22, &k), HostKeyStatus::Unknown);
        assert_eq!(check(&files, "weird.host", 22, &k), HostKeyStatus::Unknown);
        assert_eq!(check(&[], "db.example.com", 22, &k), HostKeyStatus::Unknown);
    }

    #[test]
    fn reads_hashed_names_and_revocations() {
        // `ssh-keygen -H` output for "hashed.example".
        let salt = [7u8; 20];
        let mac = Hmac::<Sha1>::new_from_slice(&salt)
            .expect("mac")
            .chain_update(b"hashed.example")
            .finalize()
            .into_bytes();
        let hashed = format!(
            "|1|{}|{} {}\n",
            BASE64.encode(&salt),
            BASE64.encode(&mac),
            line(1)
        );
        let path = file("hashed", &format!("{hashed}@revoked * {}\n", line(2)));
        let files = vec![path];
        assert_eq!(
            check(&files, "hashed.example", 22, &key(1)),
            HostKeyStatus::Known
        );
        assert_eq!(
            check(&files, "nothashed", 22, &key(1)),
            HostKeyStatus::Unknown
        );
        assert_eq!(
            check(&files, "anything", 22, &key(2)),
            HostKeyStatus::Revoked
        );
    }

    #[test]
    fn learns_a_key() {
        let path = file("learn", "existing.host ssh-rsa AAAA");
        let k = key(1);
        learn(&path, "new.host", 2200, &k).expect("learn");
        assert_eq!(
            check(std::slice::from_ref(&path), "new.host", 2200, &k),
            HostKeyStatus::Known
        );
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.starts_with("existing.host ssh-rsa AAAA\n[new.host]:2200 ssh-ed25519 "));
    }
}
