//! `deltos-sign`: the release workflow's tool for the update manifest.
//!
//! ```text
//! deltos-sign keygen [--public-key-file PATH] [--force]
//!     Makes a new key pair. Prints the private key to stdout, for the DELTOS_UPDATE_KEY
//!     secret, and writes the public key to PATH (default packaging/update-key.pub).
//! deltos-sign public-key
//!     Prints the public key that belongs to DELTOS_UPDATE_KEY.
//! deltos-sign manifest --version V --notes-url URL --base-url URL [--pub-date T] --output PATH FILE...
//!     Writes latest.json for the given package files: each with its name, URL under
//!     base-url, SHA-256 and size.
//! deltos-sign sign FILE [--output PATH]
//!     Signs FILE with DELTOS_UPDATE_KEY and writes the signature to PATH (default FILE.sig).
//! deltos-sign verify FILE SIGNATURE PUBLIC_KEY_FILE
//!     Checks that SIGNATURE was made over FILE by the key in PUBLIC_KEY_FILE.
//! ```
//!
//! Every failure exits with status 1 and one line on stderr, so a workflow step stops there.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use deltos_updater::{Manifest, ManifestAsset, PublicKey, SecretKey};
use sha2::{Digest as _, Sha256};

const KEY_VAR: &str = "DELTOS_UPDATE_KEY";
const USAGE: &str = "usage:
  deltos-sign keygen [--public-key-file PATH] [--force]
  deltos-sign public-key
  deltos-sign manifest --version V --notes-url URL --base-url URL [--pub-date T] --output PATH FILE...
  deltos-sign sign FILE [--output PATH]
  deltos-sign verify FILE SIGNATURE PUBLIC_KEY_FILE";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("deltos-sign: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let (command, rest) = args.split_first().ok_or_else(|| USAGE.to_owned())?;
    match command.as_str() {
        "keygen" => keygen(rest),
        "public-key" => {
            println!("{}", secret_from_env()?.public_key().to_base64());
            Ok(())
        }
        "manifest" => manifest(rest),
        "sign" => sign(rest),
        "verify" => verify(rest),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown command {other}\n{USAGE}")),
    }
}

/// Splits `--name value` options from positional arguments. Only the names in `known` are
/// accepted as options, and `flags` take no value.
fn parse_options(
    args: &[String],
    known: &[&str],
    flags: &[&str],
) -> Result<(Vec<(String, String)>, Vec<String>), String> {
    let mut options = Vec::new();
    let mut positional = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(name) = arg.strip_prefix("--") {
            if flags.contains(&name) {
                options.push((name.to_owned(), String::new()));
            } else if known.contains(&name) {
                let value = iter
                    .next()
                    .ok_or_else(|| format!("--{name} needs a value"))?;
                options.push((name.to_owned(), value.clone()));
            } else {
                return Err(format!("unknown option --{name}\n{USAGE}"));
            }
        } else {
            positional.push(arg.clone());
        }
    }
    Ok((options, positional))
}

fn option<'a>(options: &'a [(String, String)], name: &str) -> Option<&'a str> {
    options
        .iter()
        .rev()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

fn required<'a>(options: &'a [(String, String)], name: &str) -> Result<&'a str, String> {
    option(options, name).ok_or_else(|| format!("--{name} is required\n{USAGE}"))
}

fn secret_from_env() -> Result<SecretKey, String> {
    let text = std::env::var(KEY_VAR)
        .map_err(|_| format!("{KEY_VAR} is not set; it must hold the private update key"))?;
    // A key pasted into a secret with line breaks still decodes, since base64 has no spaces.
    SecretKey::parse(&text).map_err(|e| format!("{KEY_VAR} is not a usable key: {e}"))
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("could not read {}: {e}", path.display()))
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("could not write {}: {e}", path.display()))
}

fn keygen(args: &[String]) -> Result<(), String> {
    let (options, positional) = parse_options(args, &["public-key-file"], &["force"])?;
    if !positional.is_empty() {
        return Err(format!("keygen takes no file arguments\n{USAGE}"));
    }
    let public_path =
        PathBuf::from(option(&options, "public-key-file").unwrap_or("packaging/update-key.pub"));
    if option(&options, "force").is_none()
        && std::fs::read_to_string(&public_path).is_ok_and(|text| !text.trim().is_empty())
    {
        return Err(format!(
            "{} already holds a key; every installed copy trusts only that key, so replacing it \
             would stop them updating. Pass --force if that is what you want.",
            public_path.display()
        ));
    }
    let secret = SecretKey::generate().map_err(|e| e.to_string())?;
    write(
        &public_path,
        format!("{}\n", secret.public_key().to_base64()).as_bytes(),
    )?;
    eprintln!(
        "Public key written to {}. Commit that file.\n\
         The private key follows on stdout. Store it as the {KEY_VAR} repository secret and \
         nowhere else; it cannot be recovered from the public key.",
        public_path.display()
    );
    println!("{}", secret.to_base64());
    Ok(())
}

fn manifest(args: &[String]) -> Result<(), String> {
    let (options, files) = parse_options(
        args,
        &["version", "notes-url", "base-url", "pub-date", "output"],
        &[],
    )?;
    let version = required(&options, "version")?.trim_start_matches('v');
    let notes_url = required(&options, "notes-url")?;
    let base_url = required(&options, "base-url")?.trim_end_matches('/');
    let output = PathBuf::from(required(&options, "output")?);
    if files.is_empty() {
        return Err(format!("manifest needs at least one package file\n{USAGE}"));
    }
    let pub_date = match option(&options, "pub-date") {
        Some(date) => date.to_owned(),
        None => now_rfc3339(),
    };
    let mut assets = Vec::with_capacity(files.len());
    for file in &files {
        let path = Path::new(file);
        let name = path
            .file_name()
            .ok_or_else(|| format!("{file} has no file name"))?
            .to_string_lossy()
            .into_owned();
        let (size, sha256) = hash_file(path)?;
        assets.push(ManifestAsset {
            url: format!("{base_url}/{name}"),
            name,
            sha256,
            size,
        });
    }
    assets.sort_by(|a, b| a.name.cmp(&b.name));
    let manifest = Manifest {
        version: version.to_owned(),
        pub_date,
        notes_url: notes_url.to_owned(),
        assets,
    };
    let json = manifest.to_json().map_err(|e| e.to_string())?;
    // Parse what was written with the app's own checks, so a bad release fails here.
    Manifest::parse(&json).map_err(|e| e.to_string())?;
    write(&output, &json)?;
    eprintln!(
        "Wrote {} for {} with {} packages",
        output.display(),
        manifest.version,
        manifest.assets.len()
    );
    Ok(())
}

fn hash_file(path: &Path) -> Result<(u64, String), String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    let mut size = 0u64;
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|e| format!("could not read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        size += n as u64;
    }
    let sha256 = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok((size, sha256))
}

fn sign(args: &[String]) -> Result<(), String> {
    let (options, files) = parse_options(args, &["output"], &[])?;
    let [file] = files.as_slice() else {
        return Err(format!("sign takes exactly one file\n{USAGE}"));
    };
    let path = Path::new(file);
    let output = option(&options, "output")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("{file}.sig")));
    let secret = secret_from_env()?;
    let signature = secret.sign(&read(path)?);
    write(&output, format!("{signature}\n").as_bytes())?;
    eprintln!("Signed {} into {}", path.display(), output.display());
    Ok(())
}

fn verify(args: &[String]) -> Result<(), String> {
    let [file, signature, key_file] = args else {
        return Err(format!(
            "verify takes FILE SIGNATURE PUBLIC_KEY_FILE\n{USAGE}"
        ));
    };
    let key_text = String::from_utf8(read(Path::new(key_file))?)
        .map_err(|_| format!("{key_file} is not text"))?;
    if key_text.trim().is_empty() {
        return Err(format!("{key_file} is empty; run deltos-sign keygen first"));
    }
    let key = PublicKey::parse(&key_text).map_err(|e| format!("{key_file}: {e}"))?;
    let message = read(Path::new(file))?;
    let signature_text = String::from_utf8(read(Path::new(signature))?)
        .map_err(|_| format!("{signature} is not text"))?;
    key.verify(&message, &signature_text)
        .map_err(|e| format!("{file}: {e}"))?;
    eprintln!("{file} is signed by the key in {key_file}");
    Ok(())
}

/// The current time as `YYYY-MM-DDTHH:MM:SSZ`, without a date library.
fn now_rfc3339() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_rfc3339(seconds)
}

fn format_rfc3339(unix_seconds: u64) -> String {
    let days = unix_seconds / 86_400;
    let rem = unix_seconds % 86_400;
    let (hour, minute, second) = (rem / 3600, rem % 3600 / 60, rem % 60);
    // Civil date from days since 1970-01-01, after Howard Hinnant's algorithm.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_dates() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_rfc3339(1_793_880_000), "2026-11-05T12:00:00Z");
    }

    #[test]
    fn parses_options_and_files() {
        let args: Vec<String> = ["--version", "0.1.0", "a.deb", "--output", "out", "b.rpm"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (options, files) = parse_options(&args, &["version", "output"], &[]).unwrap();
        assert_eq!(option(&options, "version"), Some("0.1.0"));
        assert_eq!(option(&options, "output"), Some("out"));
        assert_eq!(files, ["a.deb", "b.rpm"]);
        assert!(parse_options(&args, &["version"], &[]).is_err());
        assert!(parse_options(&["--version".to_string()], &["version"], &[]).is_err());
    }

    #[test]
    fn signs_and_verifies_a_file_end_to_end() {
        let dir = std::env::temp_dir().join(format!("deltos-sign-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let secret = SecretKey::from_seed([9; 32]);
        let key_file = dir.join("update-key.pub");
        std::fs::write(&key_file, format!("{}\n", secret.public_key().to_base64())).unwrap();
        let package = dir.join("deltos_0.3.0_amd64.deb");
        std::fs::write(&package, b"package bytes").unwrap();
        let manifest_path = dir.join("latest.json");

        let s = |p: &Path| p.to_string_lossy().into_owned();
        manifest(&[
            "--version".into(),
            "v0.3.0".into(),
            "--notes-url".into(),
            "https://github.com/khaodius/deltos/releases/tag/v0.3.0".into(),
            "--base-url".into(),
            "https://github.com/khaodius/deltos/releases/download/v0.3.0/".into(),
            "--pub-date".into(),
            "2026-11-09T12:00:00Z".into(),
            "--output".into(),
            s(&manifest_path),
            s(&package),
        ])
        .unwrap();
        let parsed = Manifest::parse(&std::fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(parsed.version, "0.3.0");
        assert_eq!(parsed.assets[0].size, 13);
        assert_eq!(
            parsed.assets[0].url,
            "https://github.com/khaodius/deltos/releases/download/v0.3.0/deltos_0.3.0_amd64.deb"
        );

        let signature_path = dir.join("latest.json.sig");
        let signature = secret.sign(&std::fs::read(&manifest_path).unwrap());
        std::fs::write(&signature_path, format!("{signature}\n")).unwrap();
        verify(&[s(&manifest_path), s(&signature_path), s(&key_file)]).unwrap();

        // A manifest changed after signing fails.
        let mut bytes = std::fs::read(&manifest_path).unwrap();
        bytes[2] ^= 1;
        std::fs::write(&manifest_path, bytes).unwrap();
        assert!(verify(&[s(&manifest_path), s(&signature_path), s(&key_file)]).is_err());

        // An empty key file is a clear error, not a pass.
        std::fs::write(&key_file, "").unwrap();
        let err = verify(&[s(&manifest_path), s(&signature_path), s(&key_file)]).unwrap_err();
        assert!(err.contains("empty"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_refuses_a_package_from_another_version() {
        let dir = std::env::temp_dir().join(format!("deltos-sign-test-v-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let package = dir.join("deltos_0.2.0_amd64.deb");
        std::fs::write(&package, b"x").unwrap();
        let err = manifest(&[
            "--version".into(),
            "0.3.0".into(),
            "--notes-url".into(),
            "https://example.com/notes".into(),
            "--base-url".into(),
            "https://example.com/dl".into(),
            "--output".into(),
            dir.join("latest.json").to_string_lossy().into_owned(),
            package.to_string_lossy().into_owned(),
        ])
        .unwrap_err();
        assert!(err.contains("0.3.0"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
