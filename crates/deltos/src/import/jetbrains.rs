//! DataGrip and the other JetBrains IDEs: `dataSources.xml` in the IDE's `options` folder
//! (global data sources) or a project's `.idea` folder.
//!
//! `dataSources.xml` has one `data-source` per connection with its `driver-ref`,
//! `jdbc-url` and `group`; `dataSources.local.xml` beside it adds the `user-name` and the
//! SSH tunnel by the same `uuid`. A tunnel either names an entry of `sshConfigs.xml` by
//! `ssh-config-id` or, in older files, carries its own `proxy-host`, `port` and `user`.
//! Passwords are in the system keychain or the IDE's encrypted store; they are not read.

use std::path::Path;

use deltos_data::{ConnectionConfig, DriverKind, SshAuth, SshTunnel};

use super::xml::{self, Element};
use super::{Imported, from_jdbc, kind_named, read_text};

/// Reads `path`, either of the two data source files, with the files beside it.
pub fn read(path: &Path) -> Result<Vec<Imported>, String> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let picked_local = path.file_name().is_some_and(|n| {
        n.to_string_lossy()
            .eq_ignore_ascii_case("dataSources.local.xml")
    });
    let main = if picked_local {
        read_text(&dir.join("dataSources.xml"))?
    } else {
        read_text(path)?
    };
    let local = read_text(&dir.join("dataSources.local.xml")).ok();
    let ssh = read_text(&dir.join("sshConfigs.xml")).ok();
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok();
    parse(&main, local.as_deref(), ssh.as_deref(), home.as_deref())
}

/// `$USER_HOME$` and friends, as the IDE writes paths.
fn expand(text: &str, home: Option<&str>) -> String {
    match home {
        Some(home) => text.replace("$USER_HOME$", home),
        None => text.to_owned(),
    }
}

pub fn parse(
    main: &str,
    local: Option<&str>,
    ssh_configs: Option<&str>,
    home: Option<&str>,
) -> Result<Vec<Imported>, String> {
    let root = xml::parse(main)?;
    let sources = root.descendants("data-source");
    if root.name != "project" && root.name != "application" {
        return Err("not a JetBrains dataSources.xml file".into());
    }
    let local = local.and_then(|t| xml::parse(t).ok());
    let local_sources: Vec<&Element> = local
        .as_ref()
        .map(|l| l.descendants("data-source"))
        .unwrap_or_default();
    let ssh_root = ssh_configs.and_then(|t| xml::parse(t).ok());
    let ssh_entries: Vec<&Element> = ssh_root
        .as_ref()
        .map(|s| s.descendants("sshConfig"))
        .unwrap_or_default();
    let mut out = Vec::new();
    for source in sources {
        let name = source
            .attr("name")
            .map(str::to_owned)
            .unwrap_or_else(|| "Data source".into());
        let uuid = source.attr("uuid").unwrap_or_default();
        let extra = local_sources
            .iter()
            .find(|l| !uuid.is_empty() && l.attr("uuid") == Some(uuid))
            .copied();
        let driver = source.child_text("driver-ref").unwrap_or_default();
        let url = source
            .child_text("jdbc-url")
            .map(|u| expand(&u, home))
            .unwrap_or_default();
        let parsed = from_jdbc(&url);
        let Some(kind) = kind_named(&driver).or(parsed.as_ref().map(|(k, _)| *k)) else {
            let what = if driver.is_empty() {
                "This database".to_owned()
            } else {
                driver
            };
            out.push(Imported::unsupported(name, &what));
            continue;
        };
        let mut config: ConnectionConfig = parsed
            .filter(|(k, _)| *k == kind)
            .map(|(_, c)| c)
            .unwrap_or_default();
        let mut notes = Vec::new();
        if let Some(extra) = extra {
            if let Some(user) = extra.child_text("user-name") {
                config.user = user;
            }
            if kind != DriverKind::Sqlite
                && let Some(ssh) = extra.child("ssh-properties")
                && ssh.child_text("enabled").as_deref() == Some("true")
            {
                match tunnel(ssh, &ssh_entries, home) {
                    Some(t) => config.ssh = t,
                    None => notes.push("Its SSH tunnel could not be read; set it up again".into()),
                }
            }
        }
        if kind != DriverKind::Sqlite {
            notes.push("Its password stays in the IDE; Deltos asks for it".into());
        }
        let mut imported = Imported::new(name, kind, config);
        imported.folder = source
            .attr("group")
            .filter(|g| !g.trim().is_empty())
            .map(str::to_owned);
        imported.saved.read_only = source.attr("read-only") == Some("true")
            || source.child_text("read-only").as_deref() == Some("true");
        imported.notes = notes;
        out.push(imported);
    }
    Ok(out)
}

/// The SSH tunnel of one data source, from `sshConfigs.xml` or from the old inline form.
fn tunnel(ssh: &Element, configs: &[&Element], home: Option<&str>) -> Option<SshTunnel> {
    let auth = |name: &str| match name.to_ascii_uppercase().as_str() {
        "PASSWORD" => SshAuth::Password,
        "OPEN_SSH" | "AGENT" => SshAuth::Agent,
        _ => SshAuth::Key,
    };
    if let Some(id) = ssh.child_text("ssh-config-id") {
        let c = configs.iter().find(|c| c.attr("id") == Some(id.as_str()))?;
        return Some(SshTunnel {
            enabled: true,
            host: c.attr("host")?.to_owned(),
            port: c.attr("port").and_then(|p| p.parse().ok()),
            user: c.attr("username").unwrap_or_default().to_owned(),
            auth: auth(c.attr("authType").unwrap_or_default()),
            key_path: expand(c.attr("keyPath").unwrap_or_default(), home),
            secret: None,
        });
    }
    Some(SshTunnel {
        enabled: true,
        host: ssh.child_text("proxy-host")?,
        port: ssh.child_text("port").and_then(|p| p.parse().ok()),
        user: ssh.child_text("user").unwrap_or_default(),
        auth: auth(&ssh.child_text("auth-type").unwrap_or_default()),
        key_path: expand(&ssh.child_text("key-file").unwrap_or_default(), home),
        secret: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_data_sources_with_their_local_part() {
        let list = parse(
            include_str!("fixtures/jetbrains-dataSources.xml"),
            Some(include_str!("fixtures/jetbrains-dataSources.local.xml")),
            Some(include_str!("fixtures/jetbrains-sshConfigs.xml")),
            Some("/home/me"),
        )
        .expect("parses");
        assert_eq!(list.len(), 4);

        let shop = &list[0];
        assert_eq!(shop.saved.name, "shop@db.shop.example.com");
        assert_eq!(shop.saved.kind, DriverKind::Postgres);
        let c = &shop.saved.config;
        assert_eq!(
            (
                c.host.as_str(),
                c.port,
                c.database.as_str(),
                c.user.as_str()
            ),
            ("db.shop.example.com", Some(5432), "shop", "shop_admin")
        );
        assert_eq!(shop.folder.as_deref(), Some("Production"));
        assert_eq!(
            (
                c.ssh.enabled,
                c.ssh.host.as_str(),
                c.ssh.port,
                c.ssh.user.as_str()
            ),
            (true, "bastion.example.com", Some(2222), "me")
        );
        assert_eq!(c.ssh.auth, SshAuth::Key);
        assert_eq!(c.ssh.key_path, "/home/me/.ssh/id_ed25519");

        let app = &list[1];
        assert_eq!(app.saved.kind, DriverKind::Mysql);
        let c = &app.saved.config;
        assert_eq!(
            (c.host.as_str(), c.port, c.user.as_str()),
            ("localhost", Some(3306), "root")
        );
        assert!(c.options.is_empty(), "JDBC options are left out");
        assert_eq!(
            (
                c.ssh.host.as_str(),
                c.ssh.port,
                c.ssh.user.as_str(),
                c.ssh.auth
            ),
            ("jump.example.com", Some(2200), "deploy", SshAuth::Agent)
        );

        let notes = &list[2];
        assert_eq!(notes.saved.kind, DriverKind::Sqlite);
        assert_eq!(notes.saved.config.database, "/home/me/notes.db");
        assert!(notes.notes.is_empty());

        assert!(list[3].unsupported.is_some());
    }

    #[test]
    fn works_without_the_local_file() {
        let list = parse(
            include_str!("fixtures/jetbrains-dataSources.xml"),
            None,
            None,
            None,
        )
        .expect("parses");
        assert_eq!(list[0].saved.config.user, "");
        assert!(!list[0].saved.config.ssh.enabled);
        // A tunnel whose config is missing is reported, not guessed.
        let list = parse(
            include_str!("fixtures/jetbrains-dataSources.xml"),
            Some(include_str!("fixtures/jetbrains-dataSources.local.xml")),
            None,
            None,
        )
        .expect("parses");
        assert!(!list[0].saved.config.ssh.enabled);
        assert!(list[0].notes.iter().any(|n| n.contains("SSH")));
    }

    #[test]
    fn refuses_other_files() {
        assert!(parse("<html><body/></html>", None, None, None).is_err());
        assert!(parse("{}", None, None, None).is_err());
        assert_eq!(
            parse("<project/>", None, None, None).map(|l| l.len()),
            Ok(0)
        );
    }
}
