//! What `~/.ssh/config` says about a host: `HostName`, `User`, `Port` and `IdentityFile`,
//! read the way OpenSSH reads them. The first value found for a keyword wins, `Host`
//! patterns take `*`, `?` and `!`, and `Include` pulls in other files. `Match` blocks
//! are skipped, except `Match all`, since their conditions need things only OpenSSH
//! knows.

use std::path::{Path, PathBuf};

/// The settings `~/.ssh/config` gives a host. Anything not set is `None` or empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostConfig {
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    /// In the order given, with `~` and `%` tokens left as written.
    pub identity_files: Vec<String>,
    pub proxy_jump: Option<String>,
}

/// Looks `host` up in the person's `~/.ssh/config`. A missing or unreadable file gives
/// an empty config.
pub fn lookup(host: &str) -> HostConfig {
    let Some(home) = home_dir() else {
        return HostConfig::default();
    };
    let dir = home.join(".ssh");
    let mut out = HostConfig::default();
    if let Ok(text) = std::fs::read_to_string(dir.join("config")) {
        apply(&text, host, &dir, &mut out, 0);
    }
    out
}

/// Reads `text` for `host`, filling in what `out` does not have yet. `dir` is where
/// relative `Include` paths start.
pub fn parse(text: &str, host: &str, dir: &Path) -> HostConfig {
    let mut out = HostConfig::default();
    apply(text, host, dir, &mut out, 0);
    out
}

fn apply(text: &str, host: &str, dir: &Path, out: &mut HostConfig, depth: u32) {
    // Lines before the first Host or Match apply to every host.
    let mut active = true;
    for line in text.lines() {
        let Some((key, args)) = split_line(line) else {
            continue;
        };
        match key.as_str() {
            "host" => active = host_matches(host, &args),
            "match" => active = args.len() == 1 && args[0].eq_ignore_ascii_case("all"),
            _ if !active => {}
            "include" if depth < 8 => {
                for pattern in &args {
                    for file in expand_include(pattern, dir) {
                        if let Ok(text) = std::fs::read_to_string(&file) {
                            apply(&text, host, dir, out, depth + 1);
                        }
                    }
                }
            }
            "hostname" if out.hostname.is_none() => out.hostname = args.first().cloned(),
            "user" if out.user.is_none() => out.user = args.first().cloned(),
            "port" if out.port.is_none() => {
                out.port = args.first().and_then(|p| p.parse().ok());
            }
            "identityfile" => {
                if let Some(file) = args.first()
                    && !file.eq_ignore_ascii_case("none")
                    && !out.identity_files.contains(file)
                {
                    out.identity_files.push(file.clone());
                }
            }
            "proxyjump" if out.proxy_jump.is_none() => {
                out.proxy_jump = args
                    .first()
                    .filter(|j| !j.eq_ignore_ascii_case("none"))
                    .cloned();
            }
            _ => {}
        }
    }
}

/// A line as its lowercased keyword and its arguments. Keywords and arguments are
/// separated by spaces or one `=`; arguments may be quoted.
fn split_line(line: &str) -> Option<(String, Vec<String>)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let key = line[..end].to_ascii_lowercase();
    let rest = line[end..].trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest).trim_start();
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in rest.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    args.push(std::mem::take(&mut current));
                    any = false;
                }
            }
            c => {
                current.push(c);
                any = true;
            }
        }
    }
    if any {
        args.push(current);
    }
    Some((key, args))
}

/// Whether `host` matches a `Host` line: any pattern matches and no negated one does.
fn host_matches(host: &str, patterns: &[String]) -> bool {
    let mut matched = false;
    for pattern in patterns {
        if let Some(negated) = pattern.strip_prefix('!') {
            if glob(negated, host) {
                return false;
            }
        } else if glob(pattern, host) {
            matched = true;
        }
    }
    matched
}

/// `*` and `?` wildcards, ignoring case, as OpenSSH matches host names.
pub(crate) fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// The files an `Include` names. Relative paths start in `~/.ssh`; a `*` or `?` in the
/// file name matches the files of that folder, in name order.
fn expand_include(pattern: &str, dir: &Path) -> Vec<PathBuf> {
    let path = expand_tilde(pattern);
    let path = if path.is_absolute() {
        path
    } else {
        dir.join(path)
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !name.contains(['*', '?']) {
        return vec![path];
    }
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|e| glob(&name, &e.file_name().to_string_lossy()))
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    files
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf())
}

pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    match (
        path.strip_prefix("~/").or(path.strip_prefix("~\\")),
        home_dir(),
    ) {
        (Some(rest), Some(home)) => home.join(rest),
        _ if path == "~" => home_dir().unwrap_or_else(|| PathBuf::from(path)),
        _ => PathBuf::from(path),
    }
}

/// Expands the tokens OpenSSH allows in `IdentityFile`: `~`, `%d` (home), `%u` (local
/// user), `%r` (remote user), `%h` (host name), `%n` (the name as given) and `%%`.
pub(crate) fn expand_identity(path: &str, alias: &str, host: &str, user: &str) -> PathBuf {
    let home = home_dir()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut out = String::new();
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('d') => out.push_str(&home),
            Some('u') => out.push_str(&local_user()),
            Some('r') => out.push_str(user),
            Some('h') => out.push_str(host),
            Some('n') => out.push_str(alias),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    expand_tilde(&out)
}

/// The name signed in to this computer, which SSH uses when no user is given.
pub fn local_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
# Defaults for everything come last in most files, but first values win.
Host prod-db
    HostName db1.example.com
    User deploy
    Port 2222
    IdentityFile ~/.ssh/prod_ed25519

Host *.internal !secret.internal
  User = ops
  IdentityFile "~/.ssh/internal key"

Match host something exec "true"
  User nobody

Host *
    User fallback
    IdentityFile ~/.ssh/id_ed25519
    ServerAliveInterval 30
"#;

    #[test]
    fn reads_an_alias() {
        let c = parse(CONFIG, "prod-db", Path::new("/nowhere"));
        assert_eq!(c.hostname.as_deref(), Some("db1.example.com"));
        assert_eq!(c.user.as_deref(), Some("deploy"));
        assert_eq!(c.port, Some(2222));
        assert_eq!(
            c.identity_files,
            vec!["~/.ssh/prod_ed25519", "~/.ssh/id_ed25519"]
        );
    }

    #[test]
    fn follows_patterns_and_negation() {
        let c = parse(CONFIG, "a.internal", Path::new("/nowhere"));
        assert_eq!(c.user.as_deref(), Some("ops"));
        assert_eq!(c.identity_files[0], "~/.ssh/internal key");
        assert_eq!(c.hostname, None);
        let c = parse(CONFIG, "secret.internal", Path::new("/nowhere"));
        assert_eq!(c.user.as_deref(), Some("fallback"));
        let c = parse(CONFIG, "unknown", Path::new("/nowhere"));
        assert_eq!(c.user.as_deref(), Some("fallback"));
        assert_eq!(c.port, None);
    }

    #[test]
    fn includes_other_files() {
        let dir = std::env::temp_dir().join(format!("deltos-ssh-config-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("conf.d")).expect("dir");
        std::fs::write(dir.join("conf.d/a.conf"), "Host box\n  HostName 10.0.0.9\n").expect("a");
        std::fs::write(dir.join("conf.d/b.conf"), "Host box\n  Port 2200\n").expect("b");
        let c = parse(
            "Include conf.d/*.conf\nHost box\n  HostName ignored\n",
            "box",
            &dir,
        );
        assert_eq!(c.hostname.as_deref(), Some("10.0.0.9"));
        assert_eq!(c.port, Some(2200));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn globs() {
        assert!(glob("*.example.com", "DB.example.com"));
        assert!(glob("db?", "db1"));
        assert!(!glob("db?", "db12"));
        assert!(glob("*", ""));
        assert!(!glob("a*b", "acd"));
    }

    #[test]
    fn expands_tokens() {
        let p = expand_identity("/keys/%r@%h-%n%%", "alias", "h.example", "bob");
        assert_eq!(p, PathBuf::from("/keys/bob@h.example-alias%"));
    }
}
