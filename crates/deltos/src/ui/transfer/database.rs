//! Exporting a whole database, schema or MongoDB database: every table or collection
//! under the chosen namespaces, one after the other, each read a page at a time like a
//! single table's export, so a database of any size goes out in bounded memory.
//!
//! SQL goes to one file, a dump: each table's `CREATE` statement and its `INSERT`s, with
//! tables that others refer to by foreign key first, so the file runs again from the top.
//! The other formats go to a folder, a file per table. Files are written under a
//! temporary name and renamed when complete, as a single export does. The app's export
//! dialog and `deltos export --all` both run through here.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use deltos_data::{Connection, Error, ObjectEntry, ObjectKind, ObjectRef};
use deltos_grid::{BrowseConfig, BrowseLoader, BrowseQuery, ExportOptions, RowWriter};

use super::export::{safe_file_name, temp_path};
use super::{Outcome, Progress, count, file_label};

/// One table of the export, with what describing it said.
#[derive(Debug, Clone)]
pub struct Table {
    pub object: ObjectRef,
    /// Its `CREATE` statement, when described and the database gave one.
    pub definition: Option<String>,
    /// The server's row estimate, for progress.
    pub estimated_rows: Option<u64>,
    /// The tables its foreign keys point at.
    pub refers_to: Vec<ObjectRef>,
}

/// Makes the writer for one table, given the table and its `CREATE` statement.
pub type MakeWriter<'a> = dyn Fn(&Table) -> Box<dyn RowWriter> + Send + Sync + 'a;

/// Every table and collection under `under` (each a namespace path; an empty one means
/// every namespace), in the order to write them. `describe` reads each table's
/// definition and foreign keys, which the SQL dump needs and the other formats do not.
/// A namespace whose listing is refused is skipped unless it is all there is.
pub async fn tables(
    conn: &dyn Connection,
    under: &[Vec<String>],
    describe: bool,
) -> Result<Vec<Table>, Error> {
    let mut found: Vec<Table> = entries(conn, under, false)
        .await?
        .into_iter()
        .map(|entry| Table {
            object: entry.object,
            definition: None,
            estimated_rows: entry.estimated_rows,
            refers_to: Vec::new(),
        })
        .collect();
    if describe {
        for table in &mut found {
            // A table that cannot be described still has rows to write.
            if let Ok(info) = conn.describe(&table.object).await {
                table.definition = info.definition;
                table.refers_to = info
                    .foreign_keys
                    .into_iter()
                    .map(|k| k.target)
                    .filter(|t| *t != table.object)
                    .collect();
                if table.estimated_rows.is_none() {
                    table.estimated_rows = info.estimated_rows;
                }
            }
        }
    }
    Ok(in_dependency_order(found))
}

/// The tables and collections under `under`, as [`tables`] finds them, and their views
/// too when `views`, by namespace and name.
pub async fn entries(
    conn: &dyn Connection,
    under: &[Vec<String>],
    views: bool,
) -> Result<Vec<ObjectEntry>, Error> {
    let namespaces = conn.namespaces().await?;
    let mut paths: Vec<Vec<String>> = namespaces
        .into_iter()
        .map(|n| n.path)
        .filter(|p| under.is_empty() || under.iter().any(|u| p.starts_with(u)))
        .collect();
    // A namespace the listing does not show, such as one the user cannot list.
    for u in under {
        if !u.is_empty() && !paths.iter().any(|p| p.starts_with(u)) {
            paths.push(u.clone());
        }
    }
    let mut found: Vec<ObjectEntry> = Vec::new();
    let mut refused: Option<Error> = None;
    let mut listed = 0usize;
    for path in &paths {
        let list = match conn.objects(path).await {
            Ok(e) => e,
            Err(e) => {
                refused.get_or_insert(e);
                continue;
            }
        };
        listed += 1;
        for entry in list {
            let wanted = match entry.object.kind {
                ObjectKind::Table | ObjectKind::Collection => true,
                ObjectKind::View | ObjectKind::MaterializedView => views,
                _ => false,
            };
            if wanted && !found.iter().any(|t| t.object == entry.object) {
                found.push(entry);
            }
        }
    }
    if listed == 0
        && let Some(e) = refused
    {
        return Err(e);
    }
    found.sort_by(|a, b| {
        (&a.object.namespace, &a.object.name).cmp(&(&b.object.namespace, &b.object.name))
    });
    Ok(found)
}

/// `tables` with each one after the tables it refers to, otherwise by namespace and name.
/// Tables in a cycle of references keep name order among themselves.
pub fn in_dependency_order(mut tables: Vec<Table>) -> Vec<Table> {
    tables.sort_by(|a, b| {
        (&a.object.namespace, &a.object.name).cmp(&(&b.object.namespace, &b.object.name))
    });
    let n = tables.len();
    let index = |o: &ObjectRef| {
        tables
            .iter()
            .position(|t| t.object.namespace == o.namespace && t.object.name == o.name)
    };
    // waits[i]: how many tables in the export table i refers to and are not out yet.
    let mut waits = vec![0usize; n];
    let mut referrers: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, t) in tables.iter().enumerate() {
        let mut targets: Vec<usize> = t.refers_to.iter().filter_map(index).collect();
        targets.sort_unstable();
        targets.dedup();
        for j in targets {
            if j != i {
                waits[i] += 1;
                referrers[j].push(i);
            }
        }
    }
    let mut placed = vec![false; n];
    let mut order = Vec::with_capacity(n);
    while order.len() < n {
        // The first table, in name order, with nothing left to wait for; in a cycle, the
        // first one not placed.
        let next = (0..n)
            .find(|&i| !placed[i] && waits[i] == 0)
            .or_else(|| (0..n).find(|&i| !placed[i]));
        let Some(i) = next else { break };
        placed[i] = true;
        order.push(i);
        for &r in &referrers[i] {
            waits[r] = waits[r].saturating_sub(1);
        }
    }
    let mut slots: Vec<Option<Table>> = tables.into_iter().map(Some).collect();
    order.into_iter().filter_map(|i| slots[i].take()).collect()
}

/// Where the export goes.
pub enum Target<'a> {
    /// Every table into one file: the SQL dump.
    File(&'a Path),
    /// A file per table in this folder, named after the table with `extension`.
    Folder { dir: &'a Path, extension: &'a str },
}

/// The file name of `table` in a folder export: its name, and its namespace too when the
/// export spans several, so two tables of one name do not overwrite each other.
pub fn file_name(table: &ObjectRef, qualify: bool, extension: &str) -> String {
    let mut name = String::new();
    if qualify {
        for part in &table.namespace {
            name.push_str(part);
            name.push('.');
        }
    }
    name.push_str(&table.name);
    format!("{}.{extension}", safe_file_name(&name))
}

/// Writes every row of every table in `tables`, on the calling thread, which may block.
pub fn run(
    handle: &tokio::runtime::Handle,
    conn: &std::sync::Arc<dyn Connection>,
    tables: &[Table],
    make_writer: &MakeWriter<'_>,
    page_rows: u32,
    target: Target<'_>,
    progress: &Progress,
) -> Outcome {
    let total: u64 = tables.iter().filter_map(|t| t.estimated_rows).sum();
    progress.total_rows.store(total, Ordering::Relaxed);
    if tables.is_empty() {
        return Outcome::failed("There are no tables to export");
    }
    let what = count(tables.len() as u64, "table", "tables");
    match target {
        Target::File(path) => {
            let temp = temp_path(path);
            let result = (|| -> Result<u64, Error> {
                let file = File::create(&temp).map_err(|e| {
                    Error::Other(format!("could not create {}: {e}", temp.display()))
                })?;
                let mut out = BufWriter::with_capacity(256 * 1024, file);
                let rows = write_all(
                    handle,
                    conn,
                    tables,
                    make_writer,
                    page_rows,
                    &mut out,
                    progress,
                )?;
                finish_file(out, &temp, path)?;
                Ok(rows)
            })();
            match result {
                Ok(rows) => Outcome {
                    file: Some(path.to_path_buf()),
                    ..Outcome::done(format!(
                        "Exported {what} and {} to {}",
                        count(rows, "row", "rows"),
                        file_label(path)
                    ))
                },
                Err(e) => {
                    let _ = std::fs::remove_file(&temp);
                    failure(e, progress)
                }
            }
        }
        Target::Folder { dir, extension } => {
            if let Err(e) = std::fs::create_dir_all(dir) {
                return Outcome::failed(format!("Could not create {}: {e}", dir.display()));
            }
            let qualify = {
                let first = &tables[0].object.namespace;
                tables.iter().any(|t| &t.object.namespace != first)
            };
            let mut rows = 0u64;
            for table in tables {
                let path = dir.join(file_name(&table.object, qualify, extension));
                let temp = temp_path(&path);
                let result = (|| -> Result<u64, Error> {
                    let file = File::create(&temp).map_err(|e| {
                        Error::Other(format!("could not create {}: {e}", temp.display()))
                    })?;
                    let mut out = BufWriter::with_capacity(256 * 1024, file);
                    let n = write_table(
                        handle,
                        conn,
                        table,
                        make_writer,
                        page_rows,
                        &mut out,
                        rows,
                        progress,
                    )?;
                    finish_file(out, &temp, &path)?;
                    Ok(n)
                })();
                match result {
                    Ok(n) => rows += n,
                    Err(e) => {
                        let _ = std::fs::remove_file(&temp);
                        let mut outcome = failure(e, progress);
                        outcome.message = format!("{} ({})", outcome.message, table.object.name);
                        return outcome;
                    }
                }
            }
            Outcome {
                file: Some(dir.to_path_buf()),
                ..Outcome::done(format!(
                    "Exported {what} and {} to {}",
                    count(rows, "row", "rows"),
                    file_label(dir)
                ))
            }
        }
    }
}

/// Every table into `out`, one after the other. Returns the rows written.
pub fn write_all<W: Write>(
    handle: &tokio::runtime::Handle,
    conn: &std::sync::Arc<dyn Connection>,
    tables: &[Table],
    make_writer: &MakeWriter<'_>,
    page_rows: u32,
    mut out: W,
    progress: &Progress,
) -> Result<u64, Error> {
    let mut rows = 0u64;
    for (i, table) in tables.iter().enumerate() {
        if i > 0 {
            out.write_all(b"\n").map_err(write_error)?;
        }
        rows += write_table(
            handle,
            conn,
            table,
            make_writer,
            page_rows,
            &mut out,
            rows,
            progress,
        )?;
    }
    Ok(rows)
}

#[allow(clippy::too_many_arguments)]
fn write_table<W: Write>(
    handle: &tokio::runtime::Handle,
    conn: &std::sync::Arc<dyn Connection>,
    table: &Table,
    make_writer: &MakeWriter<'_>,
    page_rows: u32,
    out: W,
    before: u64,
    progress: &Progress,
) -> Result<u64, Error> {
    if progress.cancelled() {
        return Err(Error::Cancelled);
    }
    let loader = BrowseLoader::new(
        conn.clone(),
        table.object.clone(),
        BrowseQuery::default(),
        BrowseConfig::default(),
    );
    let mut writer = make_writer(table);
    let options = ExportOptions {
        page_rows: page_rows.max(1),
        columns: None,
    };
    handle.block_on(deltos_grid::export(
        &loader,
        &mut *writer,
        &options,
        out,
        |n| {
            progress.rows.store(before + n, Ordering::Relaxed);
            !progress.cancelled()
        },
    ))
}

fn finish_file(out: BufWriter<File>, temp: &Path, path: &Path) -> Result<(), Error> {
    let file = out
        .into_inner()
        .map_err(|e| Error::Other(format!("could not write the file: {e}")))?;
    file.sync_all()
        .map_err(|e| Error::Other(format!("could not write the file: {e}")))?;
    drop(file);
    std::fs::rename(temp, path)
        .map_err(|e| Error::Other(format!("could not save {}: {e}", path.display())))
}

fn failure(e: Error, progress: &Progress) -> Outcome {
    match e {
        Error::Cancelled => Outcome::failed(format!(
            "Cancelled after {}; the files finished before it are kept",
            count(progress.rows.load(Ordering::Relaxed), "row", "rows")
        )),
        e => Outcome::failed(format!("Export failed: {e}")),
    }
}

fn write_error(e: std::io::Error) -> Error {
    Error::Other(format!("could not write the export: {e}"))
}

/// The folder a folder export of `name` goes to by default, next to `file` or in it.
pub fn default_folder(parent: &Path, name: &str) -> PathBuf {
    parent.join(safe_file_name(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(name: &str, refers_to: &[&str]) -> Table {
        let r = |n: &str| ObjectRef {
            namespace: vec!["main".into()],
            name: n.into(),
            kind: ObjectKind::Table,
        };
        Table {
            object: r(name),
            definition: None,
            estimated_rows: None,
            refers_to: refers_to.iter().map(|n| r(n)).collect(),
        }
    }

    fn names(tables: &[Table]) -> Vec<&str> {
        tables.iter().map(|t| t.object.name.as_str()).collect()
    }

    #[test]
    fn referenced_tables_come_first() {
        let order = in_dependency_order(vec![
            table("order_items", &["orders", "products"]),
            table("orders", &["customers"]),
            table("products", &[]),
            table("customers", &[]),
            table("audit", &[]),
        ]);
        assert_eq!(
            names(&order),
            ["audit", "customers", "orders", "products", "order_items"]
        );
    }

    #[test]
    fn cycles_and_self_references_still_place_every_table() {
        let order = in_dependency_order(vec![
            table("b", &["a"]),
            table("a", &["b"]),
            table("tree", &["tree"]),
            table("c", &["missing"]),
        ]);
        assert_eq!(order.len(), 4);
        assert_eq!(names(&order), ["c", "tree", "a", "b"]);
    }

    #[test]
    fn folder_names_say_the_namespace_only_when_needed() {
        let o = ObjectRef {
            namespace: vec!["shop".into(), "public".into()],
            name: "a/b".into(),
            kind: ObjectKind::Table,
        };
        assert_eq!(file_name(&o, false, "csv"), "a_b.csv");
        assert_eq!(file_name(&o, true, "json"), "shop.public.a_b.json");
    }

    fn sqlite(rt: &tokio::runtime::Runtime, path: &Path) -> std::sync::Arc<dyn Connection> {
        let _ = std::fs::remove_file(path);
        rt.block_on(async {
            let driver = deltos_drivers::driver(deltos_data::DriverKind::Sqlite).unwrap();
            let config = deltos_data::ConnectionConfig {
                database: path.to_string_lossy().into_owned(),
                options: vec![("create".into(), "true".into())],
                ..Default::default()
            };
            driver.connect(&config).await
        })
        .unwrap()
    }

    /// A SQLite database with a foreign key, dumped to SQL and run into an empty one, and
    /// exported to a folder of CSV files.
    #[test]
    fn dumps_a_database_that_runs_back_and_exports_a_folder() {
        use deltos_data::DriverKind;
        let dir = std::env::temp_dir().join(format!("deltos-db-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let conn = sqlite(&rt, &dir.join("shop.db"));
        let options = deltos_data::ExecuteOptions::default();
        // "accounts" sorts before "people", which it refers to.
        for sql in [
            "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT)",
            "CREATE TABLE accounts (id INTEGER PRIMARY KEY, owner INTEGER REFERENCES people(id))",
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 1200) \
             INSERT INTO people SELECT i, 'person ' || i FROM n",
            "INSERT INTO accounts SELECT id, id FROM people WHERE id <= 300",
            "CREATE VIEW names AS SELECT name FROM people",
        ] {
            rt.block_on(conn.execute(sql, &options)).unwrap();
        }
        let listed = rt.block_on(tables(conn.as_ref(), &[], true)).unwrap();
        assert_eq!(names(&listed), ["people", "accounts"], "views are left out");

        let make = |t: &Table| -> Box<dyn RowWriter> {
            Box::new(
                super::super::sql::InsertWriter::new(DriverKind::Sqlite, &t.object, 100)
                    .with_preamble(t.definition.clone()),
            )
        };
        let dump = dir.join("shop.sql");
        let outcome = run(
            rt.handle(),
            &conn,
            &listed,
            &make,
            500,
            Target::File(&dump),
            &Progress::default(),
        );
        assert!(!outcome.failed, "{}", outcome.message);
        assert_eq!(
            outcome.message,
            "Exported 2 tables and 1,500 rows to shop.sql"
        );
        let sql = std::fs::read_to_string(&dump).unwrap();
        let copy = sqlite(&rt, &dir.join("copy.db"));
        for range in
            deltos_drivers::sql::split_statements(&sql, deltos_drivers::sql::Dialect::Sqlite)
        {
            let statement = &sql[range];
            rt.block_on(copy.execute(statement, &options))
                .unwrap_or_else(|e| panic!("{e}: {statement}"));
        }
        let copied = rt.block_on(tables(copy.as_ref(), &[], false)).unwrap();
        assert_eq!(copied.len(), 2);

        let make = |_: &Table| -> Box<dyn RowWriter> {
            Box::new(deltos_grid::CsvWriter::new(
                deltos_grid::CsvFormat::default(),
            ))
        };
        let folder = dir.join("csv");
        let outcome = run(
            rt.handle(),
            &copy,
            &copied,
            &make,
            500,
            Target::Folder {
                dir: &folder,
                extension: "csv",
            },
            &Progress::default(),
        );
        assert!(!outcome.failed, "{}", outcome.message);
        let lines = |name: &str| {
            std::fs::read_to_string(folder.join(name))
                .unwrap()
                .lines()
                .count()
        };
        assert_eq!(lines("people.csv"), 1201);
        assert_eq!(lines("accounts.csv"), 301);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
