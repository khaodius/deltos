//! DBeaver's `data-sources.json`, in the `.dbeaver` folder of its workspace project.
//!
//! Each entry of `connections` has a `provider` and `driver`, a `name`, a `folder`, and a
//! `configuration` with `host`, `port`, `database`, `url`, the connection type (`dev`,
//! `test`, `prod` or one of `connection-types`) and an SSH tunnel under
//! `handlers.ssh_tunnel`. User names and passwords saved by newer versions are in
//! `credentials-config.json`, encrypted; they are not read.

use deltos_data::{ConnectionConfig, DriverKind, SshAuth, SshTunnel};
use serde_json::Value;

use super::{Imported, from_jdbc, kind_named, tag_named};

pub fn parse(text: &str) -> Result<Vec<Imported>, String> {
    let root: Value =
        serde_json::from_str(text).map_err(|e| format!("not a DBeaver data-sources file: {e}"))?;
    let Some(connections) = root.get("connections").and_then(Value::as_object) else {
        return Err("not a DBeaver data-sources file: it has no connections".into());
    };
    let types = root.get("connection-types").and_then(Value::as_object);
    let mut out = Vec::new();
    for (id, c) in connections {
        let text = |v: Option<&Value>| -> Option<String> {
            match v? {
                Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            }
        };
        let name = text(c.get("name")).unwrap_or_else(|| id.clone());
        let cfg = c.get("configuration").cloned().unwrap_or(Value::Null);
        let field = |key: &str| text(cfg.get(key));
        let driver = format!(
            "{} {}",
            text(c.get("provider")).unwrap_or_default(),
            text(c.get("driver")).unwrap_or_default()
        );
        let from_url = field("url").and_then(|u| from_jdbc(&u));
        let Some(kind) = kind_named(&driver).or(from_url.as_ref().map(|(k, _)| *k)) else {
            let what = text(c.get("driver"))
                .or_else(|| text(c.get("provider")))
                .unwrap_or_else(|| "This database".into());
            out.push(Imported::unsupported(name, &what));
            continue;
        };
        // The fields win; a URL-only connection fills them from its URL.
        let mut config = from_url
            .filter(|(k, _)| *k == kind)
            .map(|(_, c)| c)
            .unwrap_or_default();
        if let Some(host) = field("host") {
            config.host = host;
        }
        if let Some(port) = field("port").and_then(|p| p.parse().ok()) {
            config.port = Some(port);
        }
        if let Some(database) = field("database") {
            config.database = database;
        }
        if let Some(user) = field("user") {
            config.user = user;
        }
        if kind == DriverKind::Sqlite {
            config = ConnectionConfig {
                database: config.database,
                ..ConnectionConfig::default()
            };
        }
        let mut imported = Imported::new(name, kind, config);
        if let Some(t) = field("type") {
            let label = types
                .and_then(|types| types.get(&t))
                .and_then(|ty| text(ty.get("name")))
                .unwrap_or(t);
            imported.saved.tag = tag_named(&label);
        }
        imported.saved.read_only = c.get("read-only").and_then(Value::as_bool) == Some(true);
        imported.folder = text(c.get("folder"));
        if c.get("save-password").and_then(Value::as_bool) == Some(true) {
            imported
                .notes
                .push("Its password stays in DBeaver; Deltos asks for it".into());
        }
        if kind != DriverKind::Sqlite && imported.saved.config.user.is_empty() {
            imported
                .notes
                .push("No user name: DBeaver keeps it encrypted".into());
        }
        if let Some(ssh) = cfg
            .get("handlers")
            .and_then(|h| h.get("ssh_tunnel"))
            .filter(|s| s.get("enabled").and_then(Value::as_bool) == Some(true))
            && kind != DriverKind::Sqlite
        {
            let props = ssh.get("properties").cloned().unwrap_or(Value::Null);
            let prop = |key: &str| text(props.get(key)).or_else(|| text(ssh.get(key)));
            let auth = match prop("authType")
                .unwrap_or_default()
                .to_ascii_uppercase()
                .as_str()
            {
                "PASSWORD" => SshAuth::Password,
                "AGENT" => SshAuth::Agent,
                _ => SshAuth::Key,
            };
            imported.saved.config.ssh = SshTunnel {
                enabled: true,
                host: prop("host").unwrap_or_default(),
                port: prop("port").and_then(|p| p.parse().ok()),
                user: prop("user").unwrap_or_default(),
                auth,
                key_path: prop("keyPath").unwrap_or_default(),
                secret: None,
            };
        }
        out.push(imported);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use deltos_settings::ConnectionTag;

    #[test]
    fn reads_a_workspace_file() {
        let list = parse(include_str!("fixtures/dbeaver-data-sources.json")).expect("parses");
        assert_eq!(list.len(), 5);
        let by_name = |n: &str| list.iter().find(|i| i.saved.name == n).expect(n);

        let shop = by_name("Shop (prod)");
        assert_eq!(shop.saved.kind, DriverKind::Postgres);
        let c = &shop.saved.config;
        assert_eq!(
            (c.host.as_str(), c.port, c.database.as_str()),
            ("db.shop.example.com", Some(5432), "shop")
        );
        assert_eq!(shop.saved.tag, ConnectionTag::Production);
        assert!(shop.saved.read_only);
        assert_eq!(shop.folder.as_deref(), Some("Production/EU"));
        assert!(c.ssh.enabled);
        assert_eq!(
            (c.ssh.host.as_str(), c.ssh.port, c.ssh.auth),
            ("bastion.example.com", Some(2222), SshAuth::Key)
        );
        assert_eq!(c.ssh.key_path, "/home/me/.ssh/id_ed25519");
        assert!(c.password.is_none() && c.url.is_none());
        assert_eq!(shop.notes.len(), 2);

        let local = by_name("Local MySQL");
        assert_eq!(local.saved.kind, DriverKind::Mysql);
        assert_eq!(local.saved.config.user, "root");
        assert_eq!(local.saved.tag, ConnectionTag::Development);
        assert!(local.notes.is_empty());

        let maria = by_name("Staging Maria");
        assert_eq!(maria.saved.kind, DriverKind::Mysql);
        let c = &maria.saved.config;
        assert_eq!(
            (c.host.as_str(), c.port, c.database.as_str()),
            ("maria.stage.internal", Some(3307), "orders")
        );
        assert_eq!(maria.saved.tag, ConnectionTag::Staging);

        let notes = by_name("Notes");
        assert_eq!(notes.saved.kind, DriverKind::Sqlite);
        assert_eq!(notes.saved.config.database, "/home/me/notes.db");
        assert!(notes.notes.is_empty());

        let billing = by_name("Billing");
        assert!(billing.unsupported.is_some());
    }

    #[test]
    fn refuses_other_files() {
        assert!(parse("").is_err());
        assert!(parse("[1, 2]").is_err());
        assert!(parse("{\"folders\": {}}").is_err());
        assert_eq!(parse("{\"connections\": {}}").map(|l| l.len()), Ok(0));
        // Odd values are skipped rather than trusted.
        let odd = parse(
            "{\"connections\": {\"x\": {\"provider\": \"postgresql\", \"configuration\": {\"port\": \"many\", \"host\": 7}}, \"y\": 3}}",
        )
        .expect("parses");
        assert_eq!(odd.len(), 2);
        assert_eq!(odd[0].saved.config.port, None);
        assert_eq!(odd[0].saved.config.host, "7");
        assert!(odd[1].unsupported.is_some());
    }
}
