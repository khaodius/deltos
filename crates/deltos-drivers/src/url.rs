//! Connection URLs: which database a URL is for, and its parts as a
//! [`ConnectionConfig`] for the connection form.
//!
//! The drivers themselves hand a URL to their client library's own parser, which knows
//! every option. This parser is for showing a pasted URL in the form's fields.

use deltos_data::{ConnectionConfig, DriverKind, Error, Result, TlsMode};
use percent_encoding::percent_decode_str;

/// The database kind a URL or path is for: `postgres://`, `postgresql://`, `mysql://`,
/// `mariadb://`, `mongodb://`, `mongodb+srv://`, `sqlite://`, `file:` or a path to a
/// `.db`, `.sqlite` or `.sqlite3` file.
pub fn kind_from_url(url: &str) -> Option<DriverKind> {
    let url = url.trim();
    let scheme = url
        .split_once("://")
        .map(|(scheme, _)| scheme)
        .or_else(|| url.split_once(':').map(|(scheme, _)| scheme))
        .unwrap_or("")
        .to_ascii_lowercase();
    match scheme.as_str() {
        "postgres" | "postgresql" => Some(DriverKind::Postgres),
        "mysql" | "mariadb" => Some(DriverKind::Mysql),
        "mongodb" | "mongodb+srv" => Some(DriverKind::Mongodb),
        "sqlite" | "sqlite3" | "file" => Some(DriverKind::Sqlite),
        _ => {
            let lower = url.to_ascii_lowercase();
            [".db", ".sqlite", ".sqlite3", ".db3"]
                .iter()
                .any(|ext| lower.ends_with(ext))
                .then_some(DriverKind::Sqlite)
        }
    }
}

/// Splits a URL into the form's fields. The URL itself is kept in `url`, so options
/// this parser does not know still reach the driver. Unknown query parameters go to
/// `options`; `sslmode`, `ssl-mode`, `ssl` and `tls` set [`TlsMode`].
pub fn parse_url(url: &str) -> Result<(DriverKind, ConnectionConfig)> {
    let url = url.trim();
    let kind = kind_from_url(url)
        .ok_or_else(|| Error::Connect(format!("“{url}” is not a database URL Deltos knows.")))?;
    let mut config = ConnectionConfig {
        url: Some(url.to_string()),
        ..Default::default()
    };
    if kind == DriverKind::Sqlite {
        config.database = sqlite_path(url).to_string();
        return Ok((kind, config));
    }
    let Some((_, rest)) = url.split_once("://") else {
        return Err(Error::Connect(format!("“{url}” has no host.")));
    };
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let (userinfo, hosts) = match authority.rsplit_once('@') {
        Some((userinfo, hosts)) => (Some(userinfo), hosts),
        None => (None, authority),
    };
    if let Some(userinfo) = userinfo {
        let (user, password) = match userinfo.split_once(':') {
            Some((user, password)) => (user, Some(password)),
            None => (userinfo, None),
        };
        config.user = decode(user);
        config.password = password.map(decode);
    }
    // Only the first host shows in the form; the URL keeps the others.
    let first = hosts.split(',').next().unwrap_or("");
    let (host, port) = split_host_port(first);
    config.host = decode(host);
    config.port = port;
    config.database = decode(path.trim_end_matches('/'));
    if kind == DriverKind::Mongodb && url.to_ascii_lowercase().starts_with("mongodb+srv") {
        config.tls = TlsMode::VerifyFull;
    }
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (key, value) = (decode(key), decode(value));
        match key.to_ascii_lowercase().as_str() {
            "sslmode" | "ssl-mode" | "ssl_mode" => {
                config.tls = match value.to_ascii_lowercase().replace('_', "-").as_str() {
                    "disable" | "disabled" => TlsMode::Disable,
                    "allow" | "prefer" | "preferred" => TlsMode::Prefer,
                    "require" | "required" => TlsMode::Require,
                    _ => TlsMode::VerifyFull,
                }
            }
            "ssl" | "tls" => {
                config.tls = if matches!(value.as_str(), "true" | "1") {
                    TlsMode::Require
                } else {
                    TlsMode::Disable
                }
            }
            "user" if config.user.is_empty() => config.user = value,
            "password" if config.password.is_none() => config.password = Some(value),
            "dbname" if config.database.is_empty() => config.database = value,
            _ => config.options.push((key, value)),
        }
    }
    Ok((kind, config))
}

/// The file path in a SQLite URL, or the text itself when it is a plain path.
pub(crate) fn sqlite_path(url: &str) -> &str {
    let url = url.trim();
    for prefix in ["sqlite://", "sqlite3://", "sqlite:", "sqlite3:"] {
        if let Some(head) = url.get(..prefix.len())
            && head.eq_ignore_ascii_case(prefix)
        {
            return &url[prefix.len()..];
        }
    }
    url
}

fn split_host_port(text: &str) -> (&str, Option<u16>) {
    if let Some(rest) = text.strip_prefix('[') {
        // [::1]:5432
        return match rest.split_once(']') {
            Some((host, tail)) => (host, tail.strip_prefix(':').and_then(|p| p.parse().ok())),
            None => (rest, None),
        };
    }
    match text.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => (host, port.parse().ok()),
        _ => (text, None),
    }
}

fn decode(text: &str) -> String {
    percent_decode_str(text).decode_utf8_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_the_schemes() {
        assert_eq!(kind_from_url("postgresql://x"), Some(DriverKind::Postgres));
        assert_eq!(kind_from_url("MariaDB://x"), Some(DriverKind::Mysql));
        assert_eq!(kind_from_url("mongodb+srv://x"), Some(DriverKind::Mongodb));
        assert_eq!(kind_from_url("sqlite::memory:"), Some(DriverKind::Sqlite));
        assert_eq!(
            kind_from_url("/home/me/app.sqlite3"),
            Some(DriverKind::Sqlite)
        );
        assert_eq!(kind_from_url("C:\\data\\app.db"), Some(DriverKind::Sqlite));
        assert_eq!(kind_from_url("http://example.com"), None);
    }

    #[test]
    fn parses_postgres() {
        let (kind, config) = parse_url(
            "postgres://me:p%40ss@db.example.com:6543/shop?sslmode=require&application_name=x",
        )
        .expect("parses");
        assert_eq!(kind, DriverKind::Postgres);
        assert_eq!(config.user, "me");
        assert_eq!(config.password.as_deref(), Some("p@ss"));
        assert_eq!(config.host, "db.example.com");
        assert_eq!(config.port, Some(6543));
        assert_eq!(config.database, "shop");
        assert_eq!(config.tls, TlsMode::Require);
        assert_eq!(
            config.options,
            vec![("application_name".into(), "x".into())]
        );
    }

    #[test]
    fn parses_mongo_and_ipv6() {
        let (_, config) =
            parse_url("mongodb+srv://u@cluster0.example.net/app?retryWrites=true").expect("parses");
        assert_eq!(config.host, "cluster0.example.net");
        assert_eq!(config.tls, TlsMode::VerifyFull);
        assert_eq!(config.database, "app");
        let (_, config) = parse_url("mongodb://a:1,b:2/?tls=false").expect("parses");
        assert_eq!((config.host.as_str(), config.port), ("a", Some(1)));
        assert_eq!(config.tls, TlsMode::Disable);
        let (_, config) = parse_url("mysql://[::1]:3307/db").expect("parses");
        assert_eq!((config.host.as_str(), config.port), ("::1", Some(3307)));
    }

    #[test]
    fn parses_sqlite_paths() {
        assert_eq!(
            parse_url("sqlite:///tmp/a.db").expect("parses").1.database,
            "/tmp/a.db"
        );
        assert_eq!(
            parse_url("sqlite::memory:").expect("parses").1.database,
            ":memory:"
        );
        assert_eq!(
            parse_url("./x.sqlite").expect("parses").1.database,
            "./x.sqlite"
        );
    }

    #[test]
    fn rejects_unknown() {
        assert!(parse_url("ftp://x").is_err());
        assert!(parse_url("").is_err());
        for junk in [
            "postgres://",
            "mysql://@:/?&=",
            "mongodb://%zz@[",
            "postgres://a:b:c@d:e",
        ] {
            let _ = parse_url(junk);
        }
    }
}
