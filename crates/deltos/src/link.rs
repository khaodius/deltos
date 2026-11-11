//! Connection links on the command line: `deltos postgres://user@host:5432/db`, the other
//! URL schemes [`deltos_drivers::kind_from_url`] knows (`postgresql`, `mysql`, `mariadb`,
//! `mongodb`, `mongodb+srv`, `sqlite`), a `file://` URL or a path to a SQLite file. A
//! desktop opens these when Deltos is registered for the schemes (see
//! `packaging/linux/deltos.desktop`).
//!
//! A link that reaches a saved connection opens that one. Otherwise it opens as an
//! unsaved connection, which lives only until Deltos quits: a password in the URL is
//! used to connect and is never written to the settings file. It reaches the keychain
//! only if the person saves the connection.

use std::path::{Path, PathBuf};

use deltos_data::{ConnectionConfig, DriverKind};
use deltos_settings::SavedConnection;
use percent_encoding::percent_decode_str;

/// A connection asked for on the command line.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub kind: DriverKind,
    /// Where to connect, with the password from the URL, if it had one.
    pub config: ConnectionConfig,
    /// A name for it, from the database and host or the file.
    pub name: String,
}

/// The connection link among the program's arguments: the first that is not an option.
pub fn from_args(args: impl IntoIterator<Item = std::ffi::OsString>) -> Option<String> {
    args.into_iter()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .find(|a| !a.starts_with('-') && !a.trim().is_empty())
}

/// Reads `arg` as a connection link. Relative SQLite paths are taken from `cwd`.
pub fn parse(arg: &str, cwd: &Path) -> Result<Link, String> {
    let arg = arg.trim();
    if let Some(path) = file_url_path(arg) {
        return Ok(sqlite(path, cwd));
    }
    let Some(known) = deltos_drivers::kind_from_url(arg) else {
        return Err(format!(
            "“{}” is not a connection URL or a database file Deltos knows.",
            without_password(arg)
        ));
    };
    // The parser's message quotes the URL, password and all.
    let (kind, mut config) = deltos_drivers::parse_url(arg)
        .map_err(|_| format!("The {} link is not a valid connection URL.", known.label()))?;
    if kind == DriverKind::Sqlite {
        return Ok(sqlite(PathBuf::from(windows_drive(&config.database)), cwd));
    }
    if config.host.is_empty() {
        return Err(format!("“{}” has no host.", without_password(arg)));
    }
    // The fields hold the password from here on; a URL that is kept for its options
    // loses it, so it can be shown and saved safely.
    let keeps_url = !config.options.is_empty()
        || config_hosts(arg) > 1
        || arg.to_ascii_lowercase().starts_with("mongodb+srv");
    config.url = keeps_url.then(|| without_password(arg));
    if !keeps_url {
        config.options.clear();
    }
    let name = match (config.database.as_str(), config.host.as_str()) {
        ("", host) => host.to_owned(),
        (db, host) => format!("{db} on {host}"),
    };
    Ok(Link { kind, config, name })
}

fn sqlite(path: PathBuf, cwd: &Path) -> Link {
    let memory = path.as_os_str() == ":memory:";
    let path = if path.is_relative() && !memory {
        cwd.join(path)
    } else {
        path
    };
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "SQLite".into());
    Link {
        kind: DriverKind::Sqlite,
        config: ConnectionConfig {
            database: path.to_string_lossy().into_owned(),
            ..ConnectionConfig::default()
        },
        name,
    }
}

/// The path in a `file://` URL, as a file manager passes it.
fn file_url_path(arg: &str) -> Option<PathBuf> {
    let head = arg.get(..7)?;
    if !head.eq_ignore_ascii_case("file://") {
        return None;
    }
    let rest = &arg[7..];
    // `file:///path` or `file://localhost/path`.
    let rest = rest
        .strip_prefix("localhost")
        .filter(|r| r.starts_with('/'))
        .unwrap_or(rest);
    let decoded = percent_decode_str(rest).decode_utf8_lossy().into_owned();
    let path = if decoded.starts_with('/') {
        windows_drive(&decoded).to_owned()
    } else {
        // `file://server/share/app.db`: a network path.
        format!("//{decoded}")
    };
    Some(PathBuf::from(path))
}

/// `C:/data/app.db` for `/C:/data/app.db`, as URLs write Windows paths; other paths as
/// they are.
fn windows_drive(path: &str) -> &str {
    let bytes = path.as_bytes();
    if bytes.len() > 2 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        &path[1..]
    } else {
        path
    }
}

/// How many hosts the URL lists (`mongodb://a:1,b:2/...`).
fn config_hosts(url: &str) -> usize {
    let authority = url
        .split_once("://")
        .map(|(_, rest)| rest.split(['/', '?']).next().unwrap_or(""))
        .unwrap_or("");
    let hosts = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    hosts.split(',').count()
}

/// `url` with the password left out: of its user part, and a `password` parameter.
pub fn without_password(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let tail = match tail.split_once('?') {
        Some((path, query)) => {
            let kept: Vec<&str> = query
                .split('&')
                .filter(|pair| {
                    let key = pair.split_once('=').map_or(*pair, |(k, _)| k);
                    !percent_decode_str(key)
                        .decode_utf8_lossy()
                        .eq_ignore_ascii_case("password")
                })
                .collect();
            if kept.is_empty() {
                path.to_owned()
            } else {
                format!("{path}?{}", kept.join("&"))
            }
        }
        None => tail.to_owned(),
    };
    match authority.rsplit_once('@') {
        Some((userinfo, hosts)) => {
            let user = userinfo.split_once(':').map_or(userinfo, |(u, _)| u);
            format!("{scheme}://{user}@{hosts}{tail}")
        }
        None => format!("{scheme}://{authority}{tail}"),
    }
}

/// What a server connection reaches, with its URL read into the fields.
fn reach(kind: DriverKind, config: &ConnectionConfig) -> (String, Option<u16>, String, String) {
    let mut c = config.clone();
    if let Some(url) = &config.url
        && let Ok((_, from_url)) = deltos_drivers::parse_url(url)
    {
        c = ConnectionConfig {
            host: if from_url.host.is_empty() {
                c.host
            } else {
                from_url.host
            },
            port: from_url.port.or(c.port),
            user: if from_url.user.is_empty() {
                c.user
            } else {
                from_url.user
            },
            database: if from_url.database.is_empty() {
                c.database
            } else {
                from_url.database
            },
            ..c
        };
    }
    let host = match c.host.trim().to_ascii_lowercase() {
        h if h.is_empty() || h == "127.0.0.1" || h == "::1" => "localhost".to_owned(),
        h => h,
    };
    (host, c.port.or(kind.default_port()), c.database, c.user)
}

/// The saved connection `link` reaches, if there is one. A connection through an SSH
/// tunnel never matches: its host is an address on the SSH server, not on this computer.
/// A link without a user or a database matches any.
pub fn find_saved<'a>(link: &Link, saved: &'a [SavedConnection]) -> Option<&'a SavedConnection> {
    let candidates = saved
        .iter()
        .filter(|s| s.kind == link.kind && !s.config.ssh.enabled);
    if link.kind == DriverKind::Sqlite {
        let want = canonical(&link.config.database);
        return candidates
            .into_iter()
            .find(|s| canonical(&s.config.database) == want);
    }
    let (host, port, database, user) = reach(link.kind, &link.config);
    candidates.into_iter().find(|s| {
        let (h, p, d, u) = reach(s.kind, &s.config);
        h == host
            && p == port
            && (database.is_empty() || d == database)
            && (user.is_empty() || u == user)
    })
}

fn canonical(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> PathBuf {
        PathBuf::from("/work")
    }

    #[test]
    fn reads_server_urls() {
        let link = parse("postgres://me:s%40cret@db.example.com:5433/shop", &cwd()).expect("ok");
        assert_eq!(link.kind, DriverKind::Postgres);
        assert_eq!(link.config.host, "db.example.com");
        assert_eq!(link.config.port, Some(5433));
        assert_eq!(link.config.user, "me");
        assert_eq!(link.config.password.as_deref(), Some("s@cret"));
        assert_eq!(link.config.url, None, "the fields hold it all");
        assert_eq!(link.name, "shop on db.example.com");

        let link = parse("mariadb://root@localhost/app", &cwd()).expect("ok");
        assert_eq!(link.kind, DriverKind::Mysql);
        let link = parse("postgresql://h", &cwd()).expect("ok");
        assert_eq!(link.name, "h");

        // A URL kept for its options never keeps the password.
        let link = parse(
            "mongodb+srv://u:pw@cluster0.example.net/app?retryWrites=true",
            &cwd(),
        )
        .expect("ok");
        assert_eq!(link.config.password.as_deref(), Some("pw"));
        let url = link.config.url.expect("kept");
        assert!(!url.contains("pw"));
        assert_eq!(
            url,
            "mongodb+srv://u@cluster0.example.net/app?retryWrites=true"
        );
        assert!(
            parse("mongodb://a:1,b:2/app", &cwd())
                .expect("ok")
                .config
                .url
                .is_some()
        );
    }

    #[test]
    fn reads_files() {
        let link = parse("sqlite:///tmp/a.db", &cwd()).expect("ok");
        assert_eq!(link.kind, DriverKind::Sqlite);
        assert_eq!(
            PathBuf::from(&link.config.database),
            PathBuf::from("/tmp/a.db")
        );
        assert_eq!(link.name, "a.db");
        let link = parse("data/app.sqlite", &cwd()).expect("ok");
        assert_eq!(
            PathBuf::from(&link.config.database),
            cwd().join("data").join("app.sqlite")
        );
        let link = parse("file:///home/me/My%20Notes.db", &cwd()).expect("ok");
        assert_eq!(link.config.database, "/home/me/My Notes.db");
        let link = parse("file://localhost/srv/x.db", &cwd()).expect("ok");
        assert_eq!(link.config.database, "/srv/x.db");
        assert_eq!(
            file_url_path("file:///C:/data/app.db"),
            Some(PathBuf::from("C:/data/app.db"))
        );
        assert_eq!(
            file_url_path("file://server/share/app.db"),
            Some(PathBuf::from("//server/share/app.db"))
        );
        assert_eq!(windows_drive("/C:/data/app.db"), "C:/data/app.db");
        assert_eq!(windows_drive("/srv/app.db"), "/srv/app.db");
        assert_eq!(windows_drive("/1:/x"), "/1:/x");
    }

    #[test]
    fn refuses_other_things() {
        assert!(parse("https://example.com", &cwd()).is_err());
        assert!(parse("postgres://", &cwd()).is_err());
        assert!(parse("notes.txt", &cwd()).is_err());
        assert_eq!(
            from_args(["deltos", "--verbose", "mysql://h/db"].map(Into::into)),
            Some("mysql://h/db".into())
        );
        assert_eq!(from_args(["deltos"].map(Into::into)), None);
    }

    #[test]
    fn hides_passwords() {
        assert_eq!(
            without_password("mysql://a:b@h:1/d?x=1"),
            "mysql://a@h:1/d?x=1"
        );
        assert_eq!(without_password("mysql://h/d"), "mysql://h/d");
        assert_eq!(without_password("mysql://a@h/d"), "mysql://a@h/d");
        assert_eq!(without_password("garbage"), "garbage");
        assert_eq!(
            without_password("postgres://h/d?sslmode=require&password=s3cret&x=1"),
            "postgres://h/d?sslmode=require&x=1"
        );
        assert_eq!(
            without_password("postgres://u:pw@h/d?PassWord=s3cret"),
            "postgres://u@h/d"
        );
        assert_eq!(
            without_password("mysql://h/d?passwordless=1"),
            "mysql://h/d?passwordless=1"
        );
    }

    #[test]
    fn a_kept_url_never_holds_the_password() {
        let link = parse(
            "postgres://me@h/shop?application_name=x&password=s3cret",
            &cwd(),
        )
        .expect("ok");
        assert_eq!(link.config.password.as_deref(), Some("s3cret"));
        let url = link.config.url.expect("kept for its options");
        assert!(!url.contains("s3cret"), "{url}");
        let e = parse("postgres:me:s3cret@h", &cwd()).expect_err("no host");
        assert!(!e.contains("s3cret"), "{e}");
    }

    fn saved(id: &str, kind: DriverKind, config: ConnectionConfig) -> SavedConnection {
        SavedConnection {
            id: id.into(),
            kind,
            config,
            ..SavedConnection::default()
        }
    }

    #[test]
    fn finds_the_saved_connection() {
        let list = vec![
            saved(
                "tunneled",
                DriverKind::Postgres,
                ConnectionConfig {
                    host: "localhost".into(),
                    database: "shop".into(),
                    ssh: deltos_data::SshTunnel {
                        enabled: true,
                        host: "bastion".into(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ),
            saved(
                "local",
                DriverKind::Postgres,
                ConnectionConfig {
                    host: "127.0.0.1".into(),
                    port: Some(5432),
                    user: "me".into(),
                    database: "shop".into(),
                    ..Default::default()
                },
            ),
            saved(
                "by-url",
                DriverKind::Mysql,
                ConnectionConfig {
                    url: Some("mysql://root@db.example.com:3307/app".into()),
                    ..Default::default()
                },
            ),
        ];
        let find = |url: &str| {
            let link = parse(url, &cwd()).expect("parses");
            find_saved(&link, &list).map(|s| s.id.clone())
        };
        assert_eq!(find("postgres://localhost/shop").as_deref(), Some("local"));
        assert_eq!(
            find("postgres://me@localhost:5432/shop").as_deref(),
            Some("local")
        );
        assert_eq!(find("postgres://other@localhost/shop"), None);
        assert_eq!(find("postgres://localhost/other"), None);
        assert_eq!(find("postgres://localhost:5433/shop"), None);
        assert_eq!(
            find("mysql://db.example.com:3307/app").as_deref(),
            Some("by-url")
        );
        assert_eq!(find("mysql://db.example.com/app"), None);
    }
}
