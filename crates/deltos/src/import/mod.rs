//! Connections from other database tools: DBeaver, DataGrip and the other JetBrains IDEs,
//! and TablePlus.
//!
//! Each parser turns a tool's own file into [`Imported`] connections without touching the
//! settings; the connection list shows them for the person to pick from, leaving out
//! the ones already saved. Passwords these tools store encrypted are never decrypted:
//! an imported connection asks for its password the first time it connects.
//!
//! Every file read is bounded in size, and nothing in a file can make a parser panic.

use std::path::{Path, PathBuf};

use deltos_data::{ConnectionConfig, DriverKind};
use deltos_settings::{ConnectionTag, SavedConnection};

mod dbeaver;
mod jetbrains;
mod tableplus;
mod xml;

/// Files larger than this are not connection lists.
const MAX_FILE: u64 = 16 * 1024 * 1024;

/// The tool a file comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    DBeaver,
    /// DataGrip, IntelliJ IDEA and the other JetBrains IDEs.
    JetBrains,
    TablePlus,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::DBeaver => "DBeaver",
            Source::JetBrains => "DataGrip / IntelliJ",
            Source::TablePlus => "TablePlus",
        }
    }
}

/// One connection read from another tool. `saved.id` is empty until it is added.
#[derive(Debug, Clone, PartialEq)]
pub struct Imported {
    pub saved: SavedConnection,
    /// The folder or group it was in there.
    pub folder: Option<String>,
    /// Why it cannot be added, such as a database Deltos does not support.
    pub unsupported: Option<String>,
    /// What did not come across, such as a stored password.
    pub notes: Vec<String>,
}

impl Imported {
    fn new(name: String, kind: DriverKind, config: ConnectionConfig) -> Self {
        Self {
            saved: SavedConnection {
                id: String::new(),
                name,
                kind,
                config,
                tag: ConnectionTag::None,
                save_password: true,
                read_only: false,
            },
            folder: None,
            unsupported: None,
            notes: Vec::new(),
        }
    }

    fn unsupported(name: String, what: &str) -> Self {
        let mut out = Self::new(name, DriverKind::Postgres, ConnectionConfig::default());
        out.unsupported = Some(format!("{what} is not supported"));
        out
    }
}

/// A connection file found on this computer, or picked by the person, and what it holds.
#[derive(Debug)]
pub struct Found {
    pub source: Source,
    pub path: PathBuf,
    pub result: Result<Vec<Imported>, String>,
}

/// Reads `path` as a `source` file. JetBrains files are read with the
/// `dataSources.local.xml` and `sshConfigs.xml` beside them, which hold the user names and
/// SSH tunnels.
pub fn read(source: Source, path: &Path) -> Found {
    let result = match source {
        Source::DBeaver => read_text(path).and_then(|t| dbeaver::parse(&t)),
        Source::TablePlus => read_text(path).and_then(|t| tableplus::parse(&t)),
        Source::JetBrains => jetbrains::read(path),
    };
    Found {
        source,
        path: path.to_owned(),
        result,
    }
}

/// Which tool wrote `path`, from its name, or from its content for an exported file.
pub fn detect(path: &Path) -> Option<Source> {
    let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    if name.starts_with("data-sources") && name.ends_with(".json") {
        return Some(Source::DBeaver);
    }
    if name.starts_with("datasources") && name.ends_with(".xml") {
        return Some(Source::JetBrains);
    }
    if name.contains("tableplus") || name == "connections.plist" || name == "connections.json" {
        return Some(Source::TablePlus);
    }
    // An exported file can have any name: look inside.
    let text = read_text(path).ok()?;
    let head: String = text.chars().take(4096).collect();
    if head.contains("DataSourceManagerImpl") || head.contains("dataSourceStorageLocal") {
        Some(Source::JetBrains)
    } else if head.contains("\"connections\"") && head.contains("\"provider\"") {
        Some(Source::DBeaver)
    } else if head.contains("ConnectionName") || head.contains("DatabaseHost") {
        Some(Source::TablePlus)
    } else {
        None
    }
}

/// Every connection file of the tools above in their usual places on this computer.
pub fn scan() -> Vec<Found> {
    known_locations(&|key| std::env::var_os(key).map(PathBuf::from))
        .into_iter()
        .filter(|(_, path)| path.is_file())
        .map(|(source, path)| read(source, &path))
        .collect()
}

/// Where the tools keep their connections, from environment variables: DBeaver's
/// workspace, the `options` folder of each JetBrains IDE and DataGrip's own projects, and
/// TablePlus's data folder. Folders are listed to find versioned names; files may not
/// exist.
pub fn known_locations(env: &dyn Fn(&str) -> Option<PathBuf>) -> Vec<(Source, PathBuf)> {
    let home = env("HOME").or_else(|| env("USERPROFILE"));
    let mut out = Vec::new();
    let data =
        env("XDG_DATA_HOME").or_else(|| home.as_ref().map(|h| h.join(".local").join("share")));
    let config = env("XDG_CONFIG_HOME").or_else(|| home.as_ref().map(|h| h.join(".config")));
    let appdata = env("APPDATA");
    let mac = home
        .as_ref()
        .map(|h| h.join("Library").join("Application Support"));

    // DBeaver: data-sources.json, and data-sources-*.json for extra projects files.
    let mut dbeaver_roots: Vec<PathBuf> = Vec::new();
    dbeaver_roots.extend(data.iter().map(|d| d.join("DBeaverData")));
    dbeaver_roots.extend(appdata.iter().map(|d| d.join("DBeaverData")));
    dbeaver_roots.extend(home.iter().map(|h| h.join("Library").join("DBeaverData")));
    dbeaver_roots.extend(home.iter().map(|h| {
        h.join(".var")
            .join("app")
            .join("io.dbeaver.DBeaverCommunity")
            .join("data")
            .join("DBeaverData")
    }));
    for root in dbeaver_roots {
        let dir = root.join("workspace6").join("General").join(".dbeaver");
        for file in list_dir(&dir) {
            let name = file_name(&file);
            if name.starts_with("data-sources") && name.ends_with(".json") {
                out.push((Source::DBeaver, file));
            }
        }
    }

    // JetBrains: <config>/JetBrains/<Product><Version>/options/dataSources.xml.
    let mut jetbrains_roots: Vec<PathBuf> = Vec::new();
    jetbrains_roots.extend(config.iter().map(|c| c.join("JetBrains")));
    jetbrains_roots.extend(appdata.iter().map(|a| a.join("JetBrains")));
    jetbrains_roots.extend(mac.iter().map(|m| m.join("JetBrains")));
    for root in jetbrains_roots {
        let mut products = list_dir(&root);
        products.sort();
        for product in products {
            out.push((
                Source::JetBrains,
                product.join("options").join("dataSources.xml"),
            ));
        }
    }
    if let Some(home) = &home {
        let mut projects = list_dir(&home.join("DataGripProjects"));
        projects.sort();
        for project in projects {
            out.push((
                Source::JetBrains,
                project.join(".idea").join("dataSources.xml"),
            ));
        }
    }

    // TablePlus keeps its list in its data folder.
    if let Some(appdata) = &appdata {
        out.push((
            Source::TablePlus,
            appdata
                .join("com.tinyapp.TablePlus")
                .join("Data")
                .join("Connections.json"),
        ));
    }
    if let Some(mac) = &mac {
        out.push((
            Source::TablePlus,
            mac.join("com.tinyapp.TablePlus")
                .join("Data")
                .join("Connections.plist"),
        ));
    }
    out
}

fn list_dir(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|entries| entries.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Reads a text file of at most [`MAX_FILE`] bytes.
pub(crate) fn read_text(path: &Path) -> Result<String, String> {
    let size = std::fs::metadata(path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?
        .len();
    if size > MAX_FILE {
        return Err(format!(
            "{} is too large to be a connection list",
            path.display()
        ));
    }
    let bytes =
        std::fs::read(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The database kind for a tool's driver or product name, or `None` when Deltos has no
/// driver for it.
pub(crate) fn kind_named(name: &str) -> Option<DriverKind> {
    let n = name.to_ascii_lowercase();
    let has = |s: &str| n.contains(s);
    if has("redshift") || has("greenplum") {
        // Close to PostgreSQL but not close enough to promise it works.
        None
    } else if has("postgres") || has("cockroach") {
        Some(DriverKind::Postgres)
    } else if has("mysql") || has("maria") {
        Some(DriverKind::Mysql)
    } else if has("sqlite") {
        Some(DriverKind::Sqlite)
    } else if has("mongo") {
        Some(DriverKind::Mongodb)
    } else {
        None
    }
    .filter(|k| deltos_drivers::driver(*k).is_some())
}

/// A JDBC URL as Deltos sees it: `jdbc:postgresql://h:5432/db`, `jdbc:mysql://...`,
/// `jdbc:mariadb://...`, `jdbc:sqlite:/path`. JDBC options are left out, since they are
/// meant for a Java driver, except the TLS ones Deltos understands.
pub(crate) fn from_jdbc(url: &str) -> Option<(DriverKind, ConnectionConfig)> {
    let url = url.trim();
    let rest = url.strip_prefix("jdbc:").unwrap_or(url);
    if let Some(path) = rest.strip_prefix("sqlite:") {
        let path = path.split('?').next().unwrap_or(path);
        return Some((
            DriverKind::Sqlite,
            ConnectionConfig {
                database: path.to_owned(),
                ..ConnectionConfig::default()
            },
        ));
    }
    let (kind, mut config) = deltos_drivers::parse_url(rest).ok()?;
    if kind == DriverKind::Sqlite {
        return None;
    }
    config.url = None;
    config.options.clear();
    config.password = None;
    Some((kind, config))
}

/// The environment tag for a tool's environment name, such as `prod` or `Staging`.
pub(crate) fn tag_named(name: &str) -> ConnectionTag {
    let n = name.trim().to_ascii_lowercase();
    if n.starts_with("prod") || n == "live" {
        ConnectionTag::Production
    } else if n.starts_with("stag") || n == "uat" || n == "preprod" {
        ConnectionTag::Staging
    } else if n.starts_with("test") || n == "qa" {
        ConnectionTag::Testing
    } else if n.starts_with("dev") {
        ConnectionTag::Development
    } else if n.starts_with("local") {
        ConnectionTag::Local
    } else {
        ConnectionTag::None
    }
}

/// What a connection reaches, for telling duplicates apart: the kind, then the file of a
/// SQLite database, or the host, port, database and user of a server.
fn target(c: &SavedConnection) -> (DriverKind, String, Option<u16>, String, String, String) {
    let mut config = c.config.clone();
    if let Some(url) = &c.config.url
        && let Ok((_, from_url)) = deltos_drivers::parse_url(url)
    {
        config = from_url;
    }
    let ssh = if c.config.ssh.enabled {
        c.config.ssh.host.to_ascii_lowercase()
    } else {
        String::new()
    };
    if c.kind == DriverKind::Sqlite {
        return (
            c.kind,
            String::new(),
            None,
            config.database,
            String::new(),
            ssh,
        );
    }
    let host = match config.host.trim().to_ascii_lowercase() {
        h if h.is_empty() || h == "127.0.0.1" || h == "::1" => "localhost".to_owned(),
        h => h,
    };
    (
        c.kind,
        host,
        config.port.or(c.kind.default_port()),
        config.database,
        config.user,
        ssh,
    )
}

/// Whether `a` reaches the same database as one of `existing`.
pub fn is_duplicate(a: &SavedConnection, existing: &[SavedConnection]) -> bool {
    let t = target(a);
    existing.iter().any(|e| target(e) == t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(
        kind: DriverKind,
        host: &str,
        port: Option<u16>,
        db: &str,
        user: &str,
    ) -> SavedConnection {
        SavedConnection {
            kind,
            config: ConnectionConfig {
                host: host.into(),
                port,
                database: db.into(),
                user: user.into(),
                ..ConnectionConfig::default()
            },
            ..SavedConnection::default()
        }
    }

    #[test]
    fn finds_duplicates_by_what_they_reach() {
        let existing = vec![
            saved(
                DriverKind::Postgres,
                "DB.example.com",
                Some(5432),
                "shop",
                "me",
            ),
            SavedConnection {
                kind: DriverKind::Mysql,
                config: ConnectionConfig {
                    url: Some("mysql://root:pw@127.0.0.1:3307/app".into()),
                    ..ConnectionConfig::default()
                },
                ..SavedConnection::default()
            },
        ];
        assert!(is_duplicate(
            &saved(DriverKind::Postgres, "db.example.com", None, "shop", "me"),
            &existing
        ));
        assert!(!is_duplicate(
            &saved(
                DriverKind::Postgres,
                "db.example.com",
                None,
                "shop",
                "other"
            ),
            &existing
        ));
        assert!(!is_duplicate(
            &saved(
                DriverKind::Postgres,
                "db.example.com",
                Some(5433),
                "shop",
                "me"
            ),
            &existing
        ));
        assert!(is_duplicate(
            &saved(DriverKind::Mysql, "localhost", Some(3307), "app", "root"),
            &existing
        ));
        // The same address behind an SSH tunnel is another database.
        let mut tunneled = saved(DriverKind::Mysql, "localhost", Some(3307), "app", "root");
        tunneled.config.ssh.enabled = true;
        tunneled.config.ssh.host = "bastion".into();
        assert!(!is_duplicate(&tunneled, &existing));
    }

    #[test]
    fn reads_jdbc_urls() {
        let (kind, c) =
            from_jdbc("jdbc:postgresql://db:6543/shop?sslmode=require&foo=1").expect("parses");
        assert_eq!(kind, DriverKind::Postgres);
        assert_eq!(
            (c.host.as_str(), c.port, c.database.as_str()),
            ("db", Some(6543), "shop")
        );
        assert_eq!(c.tls, deltos_data::TlsMode::Require);
        assert!(c.url.is_none() && c.options.is_empty());
        let (kind, c) = from_jdbc("jdbc:mariadb://h/app").expect("parses");
        assert_eq!((kind, c.host.as_str()), (DriverKind::Mysql, "h"));
        let (kind, c) = from_jdbc("jdbc:sqlite:/home/me/app.db").expect("parses");
        assert_eq!(
            (kind, c.database.as_str()),
            (DriverKind::Sqlite, "/home/me/app.db")
        );
        assert!(from_jdbc("jdbc:oracle:thin:@h:1521:x").is_none());
    }

    #[test]
    fn names_kinds_and_tags() {
        assert_eq!(kind_named("postgresql"), Some(DriverKind::Postgres));
        assert_eq!(kind_named("MariaDB"), Some(DriverKind::Mysql));
        assert_eq!(kind_named("mysql8"), Some(DriverKind::Mysql));
        assert_eq!(kind_named("sqlite_jdbc"), Some(DriverKind::Sqlite));
        assert_eq!(kind_named("oracle"), None);
        assert_eq!(kind_named("redshift"), None);
        assert_eq!(tag_named("prod"), ConnectionTag::Production);
        assert_eq!(tag_named("Staging"), ConnectionTag::Staging);
        assert_eq!(tag_named("dev"), ConnectionTag::Development);
        assert_eq!(tag_named("testing"), ConnectionTag::Testing);
        assert_eq!(tag_named("local"), ConnectionTag::Local);
        assert_eq!(tag_named("whatever"), ConnectionTag::None);
    }

    #[test]
    fn knows_where_the_tools_keep_their_files() {
        let dir = std::env::temp_dir().join(format!("deltos-import-{}", std::process::id()));
        let jb = dir.join("config").join("JetBrains").join("DataGrip2024.3");
        let dbeaver = dir
            .join("data")
            .join("DBeaverData")
            .join("workspace6")
            .join("General")
            .join(".dbeaver");
        std::fs::create_dir_all(jb.join("options")).unwrap();
        std::fs::create_dir_all(&dbeaver).unwrap();
        std::fs::write(dbeaver.join("data-sources.json"), "{}").unwrap();
        std::fs::write(dbeaver.join("data-sources-2.json"), "{}").unwrap();
        std::fs::write(dbeaver.join("credentials-config.json"), "x").unwrap();
        let (d, c) = (dir.join("data"), dir.join("config"));
        let env = move |key: &str| match key {
            "XDG_DATA_HOME" => Some(d.clone()),
            "XDG_CONFIG_HOME" => Some(c.clone()),
            "APPDATA" => Some(PathBuf::from("appdata")),
            _ => None,
        };
        let found = known_locations(&env);
        let dbeaver_files: Vec<_> = found
            .iter()
            .filter(|(s, _)| *s == Source::DBeaver)
            .collect();
        assert_eq!(dbeaver_files.len(), 2);
        assert!(found.contains(&(
            Source::JetBrains,
            jb.join("options").join("dataSources.xml")
        )));
        assert!(
            found
                .iter()
                .any(|(s, p)| *s == Source::TablePlus && p.starts_with("appdata"))
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn detects_files() {
        assert_eq!(
            detect(Path::new("/x/data-sources.json")),
            Some(Source::DBeaver)
        );
        assert_eq!(
            detect(Path::new("/x/.idea/dataSources.local.xml")),
            Some(Source::JetBrains)
        );
        assert_eq!(
            detect(Path::new("/x/Connections.plist")),
            Some(Source::TablePlus)
        );
        assert_eq!(detect(Path::new("/no/such/export.txt")), None);
    }
}
