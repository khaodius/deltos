//! TablePlus connection lists.
//!
//! TablePlus does not document its files, so this reads the shape its connection lists
//! are known to have: a list of connections, each a dictionary with `ConnectionName`,
//! `Driver`, `DatabaseHost`, `DatabasePort`, `DatabaseName`, `DatabaseUser`,
//! `DatabasePath` for SQLite, `Enviroment` (spelled so) for the tag, and `isOverSSH`,
//! `ServerAddress`, `ServerPort`, `ServerUser` and `isUsePrivateKey` for the SSH tunnel.
//! The list may be JSON (an array, or an object with a `connections` array) or an XML
//! property list, as in `Connections.plist`. Passwords are never imported, even when a
//! file carries them.

use deltos_data::{ConnectionConfig, DriverKind, SshAuth, SshTunnel};
use serde_json::{Map, Value};

use super::xml::{self, Element};
use super::{Imported, kind_named, tag_named};

pub fn parse(text: &str) -> Result<Vec<Imported>, String> {
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    let entries: Vec<Value> = if trimmed.starts_with('<') {
        let root = xml::parse(trimmed)?;
        let array = if root.name == "plist" {
            root.child("array")
        } else if root.name == "array" {
            Some(&root)
        } else {
            None
        };
        let Some(array) = array else {
            return Err("not a TablePlus connection list".into());
        };
        array.elements().map(plist_value).collect()
    } else {
        let root: Value = serde_json::from_str(trimmed)
            .map_err(|e| format!("not a TablePlus connection list: {e}"))?;
        match root {
            Value::Array(items) => items,
            Value::Object(mut map) => match map
                .remove("connections")
                .or_else(|| map.remove("Connections"))
            {
                Some(Value::Array(items)) => items,
                _ => return Err("not a TablePlus connection list".into()),
            },
            _ => return Err("not a TablePlus connection list".into()),
        }
    };
    Ok(entries
        .iter()
        .filter_map(Value::as_object)
        .map(connection)
        .collect())
}

/// A property list value as JSON: dictionaries, arrays, strings, numbers and booleans.
fn plist_value(e: &Element) -> Value {
    match e.name.as_str() {
        "dict" => {
            let mut map = Map::new();
            let mut key: Option<String> = None;
            for child in e.elements() {
                if child.name == "key" {
                    key = Some(child.text());
                } else if let Some(k) = key.take() {
                    map.insert(k, plist_value(child));
                }
            }
            Value::Object(map)
        }
        "array" => Value::Array(e.elements().map(plist_value).collect()),
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "integer" => e
            .text()
            .parse::<i64>()
            .map_or(Value::Null, |n| Value::Number(n.into())),
        "string" => Value::String(e.text()),
        _ => Value::Null,
    }
}

fn connection(c: &Map<String, Value>) -> Imported {
    let text = |key: &str| -> Option<String> {
        match c.get(key)? {
            Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    };
    let flag = |key: &str| match c.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64() == Some(1),
        Some(Value::String(s)) => matches!(s.as_str(), "1" | "true" | "YES" | "yes"),
        _ => false,
    };
    let name = text("ConnectionName").unwrap_or_else(|| "TablePlus connection".into());
    let driver = text("Driver").unwrap_or_default();
    let Some(kind) = kind_named(&driver) else {
        let what = if driver.is_empty() {
            "This database".to_owned()
        } else {
            driver
        };
        return Imported::unsupported(name, &what);
    };
    let mut notes = Vec::new();
    let config = if kind == DriverKind::Sqlite {
        ConnectionConfig {
            database: text("DatabasePath")
                .or_else(|| text("DatabaseName"))
                .unwrap_or_default(),
            ..ConnectionConfig::default()
        }
    } else {
        let mut config = ConnectionConfig {
            host: text("DatabaseHost").unwrap_or_default(),
            port: text("DatabasePort").and_then(|p| p.parse().ok()),
            user: text("DatabaseUser").unwrap_or_default(),
            database: text("DatabaseName").unwrap_or_default(),
            ..ConnectionConfig::default()
        };
        if flag("isOverSSH") {
            let key = flag("isUsePrivateKey");
            config.ssh = SshTunnel {
                enabled: true,
                host: text("ServerAddress").unwrap_or_default(),
                port: text("ServerPort").and_then(|p| p.parse().ok()),
                user: text("ServerUser").unwrap_or_default(),
                auth: if key { SshAuth::Key } else { SshAuth::Password },
                key_path: String::new(),
                secret: None,
            };
            if key && text("ServerPrivateKeyName").is_some() {
                notes.push("Its SSH key is in TablePlus; choose the key file".into());
            }
        }
        notes.push("Its password stays in TablePlus; Deltos asks for it".into());
        config
    };
    let mut imported = Imported::new(name, kind, config);
    imported.saved.tag = text("Enviroment")
        .or_else(|| text("Environment"))
        .map(|e| tag_named(&e))
        .unwrap_or_default();
    imported.folder = text("GroupName");
    imported.notes = notes;
    imported
}

#[cfg(test)]
mod tests {
    use super::*;
    use deltos_settings::ConnectionTag;

    #[test]
    fn reads_an_exported_list() {
        let list = parse(include_str!("fixtures/tableplus-connections.json")).expect("parses");
        assert_eq!(list.len(), 4);
        let shop = &list[0];
        assert_eq!(shop.saved.name, "Shop production");
        assert_eq!(shop.saved.kind, DriverKind::Postgres);
        assert_eq!(shop.saved.tag, ConnectionTag::Production);
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
        assert_eq!(
            (
                c.ssh.enabled,
                c.ssh.host.as_str(),
                c.ssh.port,
                c.ssh.user.as_str(),
                c.ssh.auth
            ),
            (true, "bastion.example.com", Some(22), "me", SshAuth::Key)
        );
        assert!(c.password.is_none());
        assert!(list[1].unsupported.is_some(), "Redis has no driver");
        let maria = &list[2];
        assert_eq!(maria.saved.kind, DriverKind::Mysql);
        assert_eq!(maria.saved.config.port, Some(3307));
        assert_eq!(maria.saved.tag, ConnectionTag::Local);
        assert!(!maria.saved.config.ssh.enabled);
        let notes = &list[3];
        assert_eq!(notes.saved.kind, DriverKind::Sqlite);
        assert_eq!(notes.saved.config.database, "/home/me/notes.sqlite");
        assert_eq!(notes.saved.tag, ConnectionTag::Testing);
    }

    #[test]
    fn reads_a_property_list() {
        let list = parse(include_str!("fixtures/tableplus-Connections.plist")).expect("parses");
        assert_eq!(list.len(), 2);
        let pg = &list[0];
        assert_eq!(pg.saved.kind, DriverKind::Postgres);
        assert_eq!(pg.saved.tag, ConnectionTag::Staging);
        assert_eq!(pg.saved.config.port, Some(6432));
        assert!(!pg.saved.config.ssh.enabled);
        let mongo = &list[1];
        assert_eq!(mongo.saved.kind, DriverKind::Mongodb);
        assert_eq!(mongo.saved.tag, ConnectionTag::Production);
        let ssh = &mongo.saved.config.ssh;
        assert_eq!(
            (ssh.enabled, ssh.host.as_str(), ssh.port, ssh.auth),
            (true, "jump.example.com", Some(2222), SshAuth::Password)
        );
    }

    #[test]
    fn refuses_other_files() {
        assert!(parse("").is_err());
        assert!(parse("{\"a\": 1}").is_err());
        assert!(parse("<plist><dict/></plist>").is_err());
        assert_eq!(
            parse("{\"connections\": [1, \"x\"]}").map(|l| l.len()),
            Ok(0)
        );
    }
}
