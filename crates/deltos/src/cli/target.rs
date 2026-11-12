//! Which database a command works on, and opening it.
//!
//! A target is a saved connection by name or id, the built-in demo, or a connection link
//! as the app takes them (`postgres://...`, `mysql://...`, `mongodb://...`, a SQLite file).
//! A saved connection brings its keychain password, SSH tunnel, tag and read-only setting.

use std::sync::Arc;

use deltos_data::{Connection, ConnectionConfig, DriverKind};
use deltos_settings::{ConnectionTag, SavedConnection, Settings};

use crate::link;
use crate::net::Net;
use crate::safety::{self, Environment};

/// Overrides the database password, for scripts and machines without a keychain.
pub const PASSWORD_ENV: &str = "DELTOS_PASSWORD";
/// Overrides the SSH password or key passphrase.
pub const SSH_SECRET_ENV: &str = "DELTOS_SSH_SECRET";

/// A connection to open.
#[derive(Debug, Clone)]
pub struct Target {
    pub name: String,
    pub kind: DriverKind,
    pub config: ConnectionConfig,
    pub tag: ConnectionTag,
    pub read_only: bool,
    /// The keychain entry, for a saved connection that keeps its password.
    keychain: Option<String>,
    demo: bool,
}

impl Target {
    pub fn environment(&self) -> Environment {
        Environment {
            name: self.name.clone(),
            kind: self.kind,
            tag: self.tag,
            read_only: self.read_only,
        }
    }

    fn saved(c: &SavedConnection) -> Self {
        Target {
            name: c.name.clone(),
            kind: c.kind,
            config: c.config.clone(),
            tag: c.tag,
            read_only: c.read_only,
            keychain: c.save_password.then(|| c.id.clone()),
            demo: false,
        }
    }
}

/// Finds `spec` among the saved connections (by id, then by name ignoring case), then as
/// the demo, then as a connection link.
pub fn resolve(spec: &str, settings: &Settings) -> Result<Target, String> {
    let spec = spec.trim();
    let saved = &settings.connections;
    if let Some(c) = saved
        .iter()
        .find(|c| c.id == spec)
        .or_else(|| saved.iter().find(|c| c.name.eq_ignore_ascii_case(spec)))
    {
        return Ok(Target::saved(c));
    }
    if spec.eq_ignore_ascii_case(crate::state::DEMO_ID) {
        return Ok(Target {
            name: "Demo".into(),
            kind: DriverKind::Postgres,
            config: ConnectionConfig::default(),
            tag: ConnectionTag::Local,
            read_only: false,
            keychain: None,
            demo: true,
        });
    }
    let cwd = std::env::current_dir().unwrap_or_default();
    let parsed = link::parse(spec, &cwd).map_err(|e| {
        format!(
            "{e} It is not the name of a saved connection either; `deltos connections` lists them."
        )
    })?;
    if let Some(c) = link::find_saved(&parsed, saved) {
        // A link to a saved connection uses it, with the link's password if it has one.
        let mut t = Target::saved(c);
        if parsed.config.password.is_some() {
            t.config.password = parsed.config.password;
        }
        return Ok(t);
    }
    Ok(Target {
        name: parsed.name,
        kind: parsed.kind,
        config: parsed.config,
        tag: ConnectionTag::None,
        read_only: false,
        keychain: None,
        demo: false,
    })
}

/// An open connection and the tunnel it goes through, which closes when this is dropped.
pub struct Open {
    pub conn: Arc<dyn Connection>,
    _tunnel: Option<Arc<deltos_ssh::Tunnel>>,
}

/// Fills in the passwords and opens `target`, through its SSH tunnel when it has one.
/// `accept_host_key` trusts an SSH server whose key is not in `known_hosts` yet; without
/// it such a server is refused with its fingerprint, as there is nobody to ask.
pub fn open(target: &Target, net: &Net, accept_host_key: bool) -> Result<Open, String> {
    if target.demo {
        return Ok(Open {
            conn: Arc::new(crate::demo::DemoConnection::new()),
            _tunnel: None,
        });
    }
    let mut config = target.config.clone();
    fill_secrets(&mut config, target.keychain.as_deref());
    let config = safety::read_only_config(config, target.read_only);
    let ask: deltos_ssh::AskHostKey = Arc::new(move |q: deltos_ssh::UnknownHost| {
        Box::pin(async move {
            if accept_host_key {
                eprintln!(
                    "deltos: trusting the {} key of {}:{} ({}) and adding it to known_hosts",
                    q.algorithm, q.host, q.port, q.fingerprint
                );
            } else {
                eprintln!(
                    "deltos: {}:{} is not in known_hosts. Its {} key is {}. Check it, then run again with --accept-host-key to trust it.",
                    q.host, q.port, q.algorithm, q.fingerprint
                );
            }
            accept_host_key
        })
    });
    let opened = net
        .handle()
        .block_on(crate::state::open_on_runtime(target.kind, config, ask))?;
    Ok(Open {
        conn: safety::guard(opened.conn, target.read_only),
        _tunnel: opened.tunnel,
    })
}

/// The passwords from the environment, then the keychain.
fn fill_secrets(config: &mut ConnectionConfig, keychain: Option<&str>) {
    use deltos_settings::secrets;
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    if let Some(p) = env(PASSWORD_ENV) {
        config.password = Some(p);
    } else if config.password.is_none()
        && let Some(id) = keychain
    {
        match secrets::password(id) {
            Ok(p) => config.password = p,
            Err(e) => eprintln!("deltos: could not read the keychain: {e}"),
        }
    }
    let ssh = &mut config.ssh;
    if !ssh.enabled || ssh.auth == deltos_data::SshAuth::Agent {
        return;
    }
    if let Some(s) = env(SSH_SECRET_ENV) {
        ssh.secret = Some(s);
    } else if ssh.secret.is_none()
        && let Some(id) = keychain
    {
        match secrets::ssh_secret(id) {
            Ok(s) => ssh.secret = s,
            Err(e) => eprintln!("deltos: could not read the keychain: {e}"),
        }
    }
}

/// Where a saved connection points, for the listing: the host and database, the file,
/// or the URL without its password.
pub fn location(c: &SavedConnection) -> String {
    let cfg = &c.config;
    if let Some(url) = cfg.url.as_deref().filter(|u| !u.trim().is_empty()) {
        return link::without_password(url);
    }
    if c.kind == DriverKind::Sqlite {
        return cfg.database.clone();
    }
    let host = if cfg.host.is_empty() {
        "localhost"
    } else {
        &cfg.host
    };
    let mut out = match cfg.port {
        Some(p) => format!("{host}:{p}"),
        None => host.to_owned(),
    };
    if !cfg.database.is_empty() {
        out.push('/');
        out.push_str(&cfg.database);
    }
    if cfg.ssh.enabled {
        out.push_str(&format!(" via ssh {}", cfg.ssh.host));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        let mut s = Settings::default();
        s.connections.push(SavedConnection {
            id: "c1".into(),
            name: "Prod".into(),
            kind: DriverKind::Postgres,
            config: ConnectionConfig {
                host: "db.internal".into(),
                port: Some(5432),
                database: "app".into(),
                ..ConnectionConfig::default()
            },
            tag: ConnectionTag::Production,
            read_only: true,
            ..SavedConnection::default()
        });
        s
    }

    #[test]
    fn finds_saved_connections_by_name_or_id() {
        let s = settings();
        let t = resolve("prod", &s).unwrap();
        assert_eq!(t.name, "Prod");
        assert!(t.read_only);
        assert_eq!(t.tag, ConnectionTag::Production);
        assert_eq!(t.keychain.as_deref(), Some("c1"));
        assert_eq!(resolve("c1", &s).unwrap().name, "Prod");
    }

    #[test]
    fn takes_links_and_the_demo() {
        let s = settings();
        let t = resolve("postgres://me:pw@elsewhere:6543/shop", &s).unwrap();
        assert_eq!(t.kind, DriverKind::Postgres);
        assert_eq!(t.config.password.as_deref(), Some("pw"));
        assert!(!t.read_only);
        assert!(resolve("demo", &s).unwrap().demo);
        assert!(resolve("nothing-like-this", &s).is_err());
    }

    #[test]
    fn a_link_to_a_saved_connection_keeps_its_safety() {
        let s = settings();
        let t = resolve("postgres://db.internal:5432/app", &s).unwrap();
        assert!(t.read_only, "{t:?}");
    }

    #[test]
    fn shows_where_without_the_password() {
        let s = settings();
        assert_eq!(location(&s.connections[0]), "db.internal:5432/app");
    }
}
