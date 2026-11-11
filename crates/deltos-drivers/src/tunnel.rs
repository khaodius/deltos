//! Pointing a connection at the local end of an SSH tunnel.
//!
//! The tunnel itself lives in `deltos-ssh`. The drivers only need the config rewritten so
//! they connect to `127.0.0.1:<port>`, where the tunnel listens, instead of the address
//! the form names, which is the database as seen from the SSH server.

use deltos_data::{ConnectionConfig, DriverKind, Error, Result};

/// Where the tunnel should forward to, as seen from the SSH server: the form's host
/// (127.0.0.1 when empty) and port (the driver's default when empty). A URL, when there
/// is one, names the address instead.
pub fn tunnel_target(kind: DriverKind, config: &ConnectionConfig) -> Result<(String, u16)> {
    if kind == DriverKind::Sqlite {
        return Err(Error::Connect(
            "an SSH tunnel does not apply to SQLite, which opens a local file".into(),
        ));
    }
    let (host, port) = match url_authority(config)? {
        Some((host, port)) => (host, port.or(config.port)),
        None => (config.host.trim().to_string(), config.port),
    };
    let host = match host.trim() {
        "" => "127.0.0.1".to_string(),
        h => h.trim_start_matches('[').trim_end_matches(']').to_string(),
    };
    let port = port
        .or(kind.default_port())
        .ok_or_else(|| Error::Connect(format!("give the port {} listens on", kind.label())))?;
    Ok((host, port))
}

/// The config the driver should use once the tunnel listens on `127.0.0.1:local_port`.
/// MongoDB is told to talk to that one server, since the replica set members it would
/// otherwise discover are addresses only the SSH server can reach.
pub fn through_tunnel(
    kind: DriverKind,
    config: &ConnectionConfig,
    local_port: u16,
) -> Result<ConnectionConfig> {
    // Fails the same way for SQLite and for URLs a tunnel cannot carry.
    tunnel_target(kind, config)?;
    let mut out = config.clone();
    out.ssh.enabled = false;
    out.host = "127.0.0.1".into();
    out.port = Some(local_port);
    if let Some(url) = config
        .url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    {
        let (before, _, after) = split_authority(url)?;
        // MongoDB wants a slash between the hosts and the options.
        let slash = if kind == DriverKind::Mongodb && !after.starts_with('/') {
            "/"
        } else {
            ""
        };
        let mut url = format!("{before}127.0.0.1:{local_port}{slash}{after}");
        if kind == DriverKind::Mongodb && !has_query_key(&url, "directconnection") {
            url.push(if url.contains('?') { '&' } else { '?' });
            url.push_str("directConnection=true");
        }
        out.url = Some(url);
    } else if kind == DriverKind::Mongodb
        && !out
            .options
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("directConnection"))
    {
        out.options.push(("directConnection".into(), "true".into()));
    }
    Ok(out)
}

/// The host and port in the config's URL, or `None` without a URL.
fn url_authority(config: &ConnectionConfig) -> Result<Option<(String, Option<u16>)>> {
    let Some(url) = config
        .url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
    else {
        return Ok(None);
    };
    let (_, hosts, _) = split_authority(url)?;
    let (host, port) = split_host_port(hosts);
    Ok(Some((
        percent_encoding::percent_decode_str(host)
            .decode_utf8_lossy()
            .into_owned(),
        port,
    )))
}

/// Splits a URL into what comes before its hosts (scheme and user), the hosts, and the
/// rest (path and query). Refuses what one tunnel cannot carry.
fn split_authority(url: &str) -> Result<(&str, &str, &str)> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(Error::Connect("the connection URL has no host".into()));
    };
    if scheme.eq_ignore_ascii_case("mongodb+srv") {
        return Err(Error::Connect(
            "an SSH tunnel cannot carry a mongodb+srv:// address, which looks its servers up in DNS. Use a mongodb:// URL with one host".into(),
        ));
    }
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let hosts_start = authority.rfind('@').map_or(0, |at| at + 1);
    let hosts = &authority[hosts_start..];
    if hosts.contains(',') {
        return Err(Error::Connect(
            "an SSH tunnel reaches one server, and the connection URL names several. Keep one host in the URL".into(),
        ));
    }
    let before = &url[..scheme.len() + 3 + hosts_start];
    Ok((before, hosts, &rest[end..]))
}

fn has_query_key(url: &str, key: &str) -> bool {
    url.split_once('?').is_some_and(|(_, q)| {
        q.split('&').any(|pair| {
            pair.split('=')
                .next()
                .unwrap_or("")
                .eq_ignore_ascii_case(key)
        })
    })
}

fn split_host_port(text: &str) -> (&str, Option<u16>) {
    if let Some(rest) = text.strip_prefix('[') {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(host: &str, port: Option<u16>) -> ConnectionConfig {
        ConnectionConfig {
            host: host.into(),
            port,
            ..Default::default()
        }
    }

    fn with_url(url: &str) -> ConnectionConfig {
        ConnectionConfig {
            url: Some(url.into()),
            ..Default::default()
        }
    }

    #[test]
    fn target_defaults_to_the_servers_own_loopback() {
        let t = |kind, c: &ConnectionConfig| tunnel_target(kind, c).expect("target");
        assert_eq!(
            t(DriverKind::Mysql, &fields("", None)),
            ("127.0.0.1".into(), 3306)
        );
        assert_eq!(
            t(DriverKind::Postgres, &fields("db.internal", Some(6432))),
            ("db.internal".into(), 6432)
        );
        assert_eq!(
            t(DriverKind::Mongodb, &with_url("mongodb://u:p@10.0.0.5/app")),
            ("10.0.0.5".into(), 27017)
        );
        assert_eq!(
            t(DriverKind::Mysql, &with_url("mysql://root@[::1]:3307/x")),
            ("::1".into(), 3307)
        );
        assert!(tunnel_target(DriverKind::Sqlite, &fields("", None)).is_err());
    }

    #[test]
    fn points_the_driver_at_the_local_end() {
        let c = through_tunnel(DriverKind::Mysql, &fields("", None), 41234).expect("config");
        assert_eq!((c.host.as_str(), c.port), ("127.0.0.1", Some(41234)));
        assert!(c.options.is_empty());

        let c = through_tunnel(
            DriverKind::Postgres,
            &with_url("postgres://me:p%40ss@db:5432/shop?sslmode=disable"),
            5000,
        )
        .expect("config");
        assert_eq!(
            c.url.as_deref(),
            Some("postgres://me:p%40ss@127.0.0.1:5000/shop?sslmode=disable")
        );

        let c = through_tunnel(DriverKind::Mongodb, &with_url("mongodb://h"), 7).expect("config");
        assert_eq!(
            c.url.as_deref(),
            Some("mongodb://127.0.0.1:7/?directConnection=true")
        );
        let c = through_tunnel(DriverKind::Mongodb, &fields("h", None), 7).expect("config");
        assert_eq!(
            c.options,
            vec![("directConnection".to_string(), "true".to_string())]
        );
    }

    #[test]
    fn refuses_what_one_tunnel_cannot_carry() {
        assert!(through_tunnel(DriverKind::Mongodb, &with_url("mongodb://a:1,b:2/x"), 1).is_err());
        assert!(
            through_tunnel(
                DriverKind::Mongodb,
                &with_url("mongodb+srv://c.example/x"),
                1
            )
            .is_err()
        );
    }
}
