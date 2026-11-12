//! The command line: `deltos <command> ...` works with databases without opening a window.
//!
//! It shares everything below the views with the app: the saved connections and their
//! keychain passwords, SSH tunnels, the read-only guard and the confirmations of
//! [`crate::safety`], the drivers, the script splitter, and the export writers and import
//! readers of [`crate::ui::transfer`], which never touch the UI. Results stream a batch at
//! a time, so a query or export of any size runs in bounded memory.
//!
//! Anything that is not a command, such as a connection link or nothing at all, starts
//! the app as before.

mod args;
mod output;
mod target;

use std::io::{BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use deltos_data::{Connection, ExecuteOptions, ObjectRef};
use deltos_grid::{BrowseConfig, BrowseLoader, BrowseQuery, ExportOptions};
use deltos_settings::{Paths, Settings};
use futures::StreamExt;

use crate::net::Net;
use crate::safety;
use crate::ui::transfer::import::{self, CsvRead, FileFormat, Plan};
use crate::ui::transfer::read::ScriptReader;
use crate::ui::transfer::{Outcome, Progress};
use args::Args;
use output::Format;
use target::Target;

/// The commands, as the first argument names them.
const COMMANDS: [&str; 8] = [
    "help",
    "version",
    "connections",
    "test",
    "objects",
    "query",
    "export",
    "import",
];

const HELP: &str = "\
Deltos on the command line: work with databases without opening a window.

Usage: deltos <command> [options]
       deltos [connection link]        open the app

Commands:
  connections [--json]                 list the saved connections
  test <conn>                          connect and show the server's version
  objects <conn> [namespace] [--json]  list namespaces, or the objects in one
  query <conn> [-e SQL]... [-f FILE]   run statements and print their results
  export <conn> <table> [-o FILE]      write a table, view or collection's rows
  import <conn> <FILE> [--into TABLE]  load a CSV, TSV, JSON or SQL file
  help, version

<conn> is a saved connection's name or id, `demo`, or a link: postgres://...,
mysql://..., mariadb://..., mongodb://..., sqlite:///path or a SQLite file path.

Query:
  -e, --execute SQL      a statement to run; repeat for more. A script with several
                         statements runs them in order
  -f, --file FILE        statements from a file, `-` for standard input. Without -e or
                         -f, statements are read from standard input when it is piped
  --format FORMAT        table, csv, tsv, json, jsonl or md. Default: table on a
                         terminal, tsv otherwise
  -o, --output FILE      write to a file; its extension picks the format
  --max-rows N           stop each result after N rows
  -n, --namespace NS     the database or schema to run in, as db or db.schema

Export:
  <table>                a name, or schema.name; --namespace narrows the search
  --format FORMAT        csv, tsv, json, jsonl, md or sql (INSERT statements).
                         Default: from the file extension, else csv
  -o, --output FILE      the file to write; it is replaced only when the export
                         finishes. Without it rows go to standard output
  --where CONDITION      only rows matching this condition, in the database's syntax

Import:
  --into TABLE           the table or collection for CSV and JSON rows; a SQL file
                         runs in its namespace, or in --namespace
  --format FORMAT        csv, tsv, json or sql. Default: from the file extension
  --delimiter CHAR       the CSV delimiter. --no-header when the first line is data
  --null TEXT            the CSV text that stands for NULL
  --truncate             empty the table first. --skip-bad skips rows that fail

Every command:
  -y, --yes              run statements that Deltos would ask about in the app: on a
                         staging or production connection, writes and imports; and
                         anywhere, DROP, TRUNCATE and UPDATE or DELETE without WHERE
  --accept-host-key      trust an SSH server not yet in known_hosts
  DELTOS_PASSWORD, DELTOS_SSH_SECRET
                         passwords for machines without a keychain

A read-only connection refuses anything that could change data, as it does in the app.
Exit status: 0 when everything ran, 1 when something failed, 2 for a usage error.";

/// Runs the command the arguments name and returns the exit status, or `None` when the
/// arguments are for the app.
pub fn run_from_args() -> Option<i32> {
    let mut all: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let first = all.first()?.as_str();
    let command = match first {
        "-h" | "--help" => "help",
        "-V" | "--version" => "version",
        c if COMMANDS.contains(&c) => c,
        _ => return None,
    }
    .to_owned();
    all.remove(0);
    // Logs go to standard error so they never mix into results on standard output.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("DELTOS_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    Some(match run(&command, all) {
        Ok(()) => 0,
        Err(Fail { code, message }) => {
            if !message.is_empty() {
                eprintln!("deltos {command}: {message}");
            }
            code
        }
    })
}

/// Why a command stopped, and its exit status.
#[derive(Debug)]
struct Fail {
    code: i32,
    message: String,
}

impl Fail {
    fn usage(message: impl Into<String>) -> Self {
        Fail {
            code: 2,
            message: format!("{}\nRun `deltos help` for the options.", message.into()),
        }
    }
}

impl From<String> for Fail {
    fn from(message: String) -> Self {
        Fail { code: 1, message }
    }
}

impl From<&str> for Fail {
    fn from(message: &str) -> Self {
        Fail::from(message.to_owned())
    }
}

type Done = Result<(), Fail>;

fn run(command: &str, rest: Vec<String>) -> Done {
    let args = Args::parse(rest).map_err(Fail::usage)?;
    if command == "help" || args.flag("help") {
        println!("{HELP}");
        return Ok(());
    }
    if command == "version" {
        println!("deltos {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let settings = load_settings()?;
    match command {
        "connections" => connections(&args, &settings),
        "test" => test(&args, &settings),
        "objects" => objects(&args, &settings),
        "query" => query(&args, &settings),
        "export" => export(&args, &settings),
        "import" => import(&args, &settings),
        _ => Err(Fail::usage(format!("unknown command {command}"))),
    }
}

fn load_settings() -> Result<Settings, Fail> {
    let Some(paths) = Paths::detect() else {
        return Ok(Settings::default());
    };
    let file = paths.settings_file();
    if !file.exists() {
        return Ok(Settings::default());
    }
    Settings::load(&file).map_err(|e| Fail::from(format!("{}: {e}", file.display())))
}

/// The first positional argument: the connection.
fn target_of(args: &Args, settings: &Settings) -> Result<Target, Fail> {
    let spec = args
        .positional
        .first()
        .ok_or_else(|| Fail::usage("name a connection"))?;
    target::resolve(spec, settings).map_err(Fail::from)
}

fn runtime() -> Result<Net, Fail> {
    Net::new().map_err(|e| Fail::from(format!("could not start the async runtime: {e}")))
}

fn connect(args: &Args, target: &Target, net: &Net) -> Result<target::Open, Fail> {
    target::open(target, net, args.flag("accept-host-key"))
        .map_err(|e| Fail::from(format!("could not connect to {}: {e}", target.name)))
}

// ---- connections, test, objects ---------------------------------------------------------

fn connections(args: &Args, settings: &Settings) -> Done {
    args.only_flags(&["json"]).map_err(Fail::usage)?;
    args.only_values(&[]).map_err(Fail::usage)?;
    let list = &settings.connections;
    if args.flag("json") {
        let rows: Vec<_> = list
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id,
                    "name": c.name,
                    "kind": c.kind.label(),
                    "tag": c.tag.label(),
                    "location": target::location(c),
                    "read_only": c.read_only,
                    "ssh": c.config.ssh.enabled,
                })
            })
            .collect();
        return print_out(&format!(
            "{}\n",
            serde_json::to_string_pretty(&rows).unwrap_or_default()
        ));
    }
    if list.is_empty() {
        eprintln!("No saved connections. Add one in the app, or use a link.");
        return Ok(());
    }
    let columns = text_columns(&["name", "kind", "tag", "location", "id"]);
    let rows: Vec<Vec<deltos_data::Value>> = list
        .iter()
        .map(|c| {
            let mut name = c.name.clone();
            if c.read_only {
                name.push_str(" (read-only)");
            }
            [
                name,
                c.kind.label().to_owned(),
                if c.tag == deltos_settings::ConnectionTag::None {
                    String::new()
                } else {
                    c.tag.label().to_owned()
                },
                target::location(c),
                c.id.clone(),
            ]
            .into_iter()
            .map(deltos_data::Value::text)
            .collect()
        })
        .collect();
    print_table(&columns, &rows)
}

fn test(args: &Args, settings: &Settings) -> Done {
    args.only_flags(&["accept-host-key"]).map_err(Fail::usage)?;
    args.only_values(&[]).map_err(Fail::usage)?;
    let target = target_of(args, settings)?;
    let net = runtime()?;
    let started = Instant::now();
    let open = connect(args, &target, &net)?;
    let took = started.elapsed();
    println!(
        "Connected to {} ({}) in {} ms{}",
        target.name,
        open.conn.server_version(),
        took.as_millis(),
        if target.read_only { ", read-only" } else { "" }
    );
    net.handle().block_on(open.conn.close());
    Ok(())
}

fn objects(args: &Args, settings: &Settings) -> Done {
    args.only_flags(&["json", "accept-host-key"])
        .map_err(Fail::usage)?;
    args.only_values(&[]).map_err(Fail::usage)?;
    let target = target_of(args, settings)?;
    let net = runtime()?;
    let open = connect(args, &target, &net)?;
    let h = net.handle();
    let json = args.flag("json");
    let Some(ns) = args.positional.get(1).map(|n| args::path(n)) else {
        let list = h
            .block_on(open.conn.namespaces())
            .map_err(|e| Fail::from(e.to_string()))?;
        if json {
            return print_out(&format!(
                "{}\n",
                serde_json::to_string_pretty(&list).unwrap_or_default()
            ));
        }
        let rows: Vec<Vec<deltos_data::Value>> = list
            .iter()
            .map(|n| {
                vec![
                    deltos_data::Value::text(n.path.join(".")),
                    deltos_data::Value::text(format!("{:?}", n.kind).to_lowercase()),
                ]
            })
            .collect();
        return print_table(&text_columns(&["namespace", "kind"]), &rows);
    };
    let list = h
        .block_on(open.conn.objects(&ns))
        .map_err(|e| Fail::from(e.to_string()))?;
    if json {
        let rows: Vec<_> = list
            .iter()
            .map(|o| {
                serde_json::json!({
                    "name": o.object.name,
                    "namespace": o.object.namespace,
                    "kind": format!("{:?}", o.object.kind).to_lowercase(),
                    "estimated_rows": o.estimated_rows,
                    "size_bytes": o.size_bytes,
                })
            })
            .collect();
        return print_out(&format!(
            "{}\n",
            serde_json::to_string_pretty(&rows).unwrap_or_default()
        ));
    }
    let rows: Vec<Vec<deltos_data::Value>> = list
        .iter()
        .map(|o| {
            vec![
                deltos_data::Value::text(o.object.name.clone()),
                deltos_data::Value::text(format!("{:?}", o.object.kind).to_lowercase()),
                o.estimated_rows.map_or(deltos_data::Value::text(""), |n| {
                    deltos_data::Value::Int(n as i64)
                }),
            ]
        })
        .collect();
    let mut columns = text_columns(&["name", "kind", "rows"]);
    columns[2].kind = deltos_data::ValueKind::Int;
    print_table(&columns, &rows)
}

// ---- query --------------------------------------------------------------------------

fn query(args: &Args, settings: &Settings) -> Done {
    args.only_flags(&["yes", "accept-host-key"])
        .map_err(Fail::usage)?;
    args.only_values(&[
        "execute",
        "file",
        "format",
        "output",
        "max-rows",
        "namespace",
    ])
    .map_err(Fail::usage)?;
    if args.positional.len() > 1 {
        return Err(Fail::usage(
            "give statements with -e or -f, not as more arguments",
        ));
    }
    let target = target_of(args, settings)?;
    let output = args.value("output").map(PathBuf::from);
    let fallback = if output.is_none() && std::io::stdout().is_terminal() {
        Format::Table
    } else {
        Format::Tsv
    };
    let format =
        Format::pick(args.value("format"), output.as_deref(), fallback).map_err(Fail::usage)?;
    if format == Format::Sql {
        return Err(Fail::usage(
            "query results have no table to INSERT into: use export --format sql",
        ));
    }
    let max_rows = match args.value("max-rows") {
        Some(n) => Some(
            n.parse::<u64>()
                .map_err(|_| Fail::usage("--max-rows takes a number"))?,
        ),
        None => None,
    };
    let source: Box<dyn BufRead> = match (args.values("execute"), args.value("file")) {
        (e, Some(_)) if !e.is_empty() => {
            return Err(Fail::usage("give statements with -e or -f, not both"));
        }
        (e, None) if !e.is_empty() => Box::new(std::io::Cursor::new(e.join(";\n").into_bytes())),
        (_, Some("-")) => Box::new(BufReader::new(std::io::stdin())),
        (_, Some(file)) => Box::new(BufReader::new(
            std::fs::File::open(file).map_err(|e| Fail::from(format!("{file}: {e}")))?,
        )),
        (_, None) if !std::io::stdin().is_terminal() => Box::new(BufReader::new(std::io::stdin())),
        _ => {
            return Err(Fail::usage(
                "give a statement with -e, a file with -f, or pipe statements in",
            ));
        }
    };
    let net = runtime()?;
    let open = connect(args, &target, &net)?;
    let mut out = Out::new(output.as_deref())?;
    let options = ExecuteOptions {
        batch_size: 1000,
        namespace: args.value("namespace").map(args::path),
    };
    let env = target.environment();
    let mut script = ScriptReader::new(source, import::dialect(target.kind));
    let mut ran = 0u64;
    let mut results = 0u64;
    loop {
        let statement = match script.next_statement() {
            Ok(Some(s)) => s,
            Ok(None) => break,
            Err(e) => return Err(Fail::from(format!("could not read the statements: {e}"))),
        };
        if let Some(reason) = safety::confirmation(&statement.text, Some(&env), &settings.data)
            && !args.flag("yes")
        {
            return Err(Fail::from(format!(
                "not run: {reason} On {} that needs --yes. ({} ran before it.)",
                env.name,
                count(ran, "statement", "statements")
            )));
        }
        let at = |e: deltos_data::Error| {
            Fail::from(format!(
                "statement {} (line {}) failed: {e}",
                ran + 1,
                statement.line
            ))
        };
        let started = Instant::now();
        let h = net.handle();
        let mut stream = h
            .block_on(open.conn.execute(&statement.text, &options))
            .map_err(at)?;
        for m in &stream.messages {
            eprintln!("{m}");
        }
        if stream.columns.is_empty() {
            while let Some(batch) = h.block_on(stream.batches.next()) {
                batch.map_err(at)?;
            }
            let what = match stream.affected {
                Some(n) => count(n, "row", "rows") + " affected",
                None => "done".into(),
            };
            eprintln!("{what} ({} ms)", started.elapsed().as_millis());
        } else {
            if results > 0 && format == Format::Table {
                out.write(b"\n")?;
            }
            results += 1;
            let mut writer = format.writer(target.kind, None).map_err(Fail::usage)?;
            let mut buf = Vec::with_capacity(64 * 1024);
            writer.begin(&stream.columns, &mut buf);
            let mut rows = 0u64;
            let mut cut = false;
            'batches: while let Some(batch) = h.block_on(stream.batches.next()) {
                for row in batch.map_err(at)?.rows {
                    if max_rows.is_some_and(|m| rows >= m) {
                        cut = true;
                        break 'batches;
                    }
                    writer.row(&row, &mut buf);
                    rows += 1;
                }
                out.write(&buf)?;
                buf.clear();
            }
            writer.finish(&mut buf);
            out.write(&buf)?;
            eprintln!(
                "{}{} ({} ms)",
                count(rows, "row", "rows"),
                if cut { ", stopped at --max-rows" } else { "" },
                started.elapsed().as_millis()
            );
        }
        ran += 1;
    }
    out.finish()?;
    net.handle().block_on(open.conn.close());
    if ran == 0 {
        eprintln!("No statements to run.");
    }
    Ok(())
}

// ---- export and import ----------------------------------------------------------------

fn export(args: &Args, settings: &Settings) -> Done {
    args.only_flags(&["accept-host-key"]).map_err(Fail::usage)?;
    args.only_values(&["format", "output", "namespace", "where"])
        .map_err(Fail::usage)?;
    let target = target_of(args, settings)?;
    let spec = args
        .positional
        .get(1)
        .ok_or_else(|| Fail::usage("name the table, view or collection to export"))?;
    let output = args.value("output").map(PathBuf::from);
    let format =
        Format::pick(args.value("format"), output.as_deref(), Format::Csv).map_err(Fail::usage)?;
    if format == Format::Table {
        return Err(Fail::usage(
            "export writes csv, tsv, json, jsonl, md or sql",
        ));
    }
    let net = runtime()?;
    let open = connect(args, &target, &net)?;
    let h = net.handle();
    let object = find_object(&h, &open.conn, spec, args.value("namespace"))?;
    let writer = format
        .writer(target.kind, Some(&object))
        .map_err(Fail::usage)?;
    let query = BrowseQuery {
        raw_where: args.value("where").map(str::to_owned),
        ..BrowseQuery::default()
    };
    let loader = BrowseLoader::new(
        open.conn.clone(),
        object.clone(),
        query,
        BrowseConfig::default(),
    );
    let options = ExportOptions::default();
    match output {
        Some(path) => {
            let progress = Progress::default();
            let outcome = with_progress(&progress, "rows", || {
                crate::ui::transfer::export::run(&h, &loader, writer, &options, &path, &progress)
            });
            report(outcome)
        }
        None => {
            let mut writer = writer;
            let stdout = std::io::stdout().lock();
            let written = h.block_on(deltos_grid::export(
                &loader,
                &mut *writer,
                &options,
                stdout,
                |_| true,
            ));
            match written {
                Ok(n) => {
                    eprintln!("Exported {}", count(n, "row", "rows"));
                    Ok(())
                }
                Err(e) if is_broken_pipe(&e.to_string()) => Ok(()),
                Err(e) => Err(Fail::from(format!("export failed: {e}"))),
            }
        }
    }
}

fn import(args: &Args, settings: &Settings) -> Done {
    args.only_flags(&[
        "yes",
        "accept-host-key",
        "no-header",
        "truncate",
        "skip-bad",
    ])
    .map_err(Fail::usage)?;
    args.only_values(&["into", "format", "namespace", "delimiter", "null"])
        .map_err(Fail::usage)?;
    let target = target_of(args, settings)?;
    let file = args
        .positional
        .get(1)
        .map(PathBuf::from)
        .ok_or_else(|| Fail::usage("name the file to import"))?;
    let (format, tab) = match args.value("format").map(str::to_ascii_lowercase).as_deref() {
        Some("csv") => (FileFormat::Csv, false),
        Some("tsv") => (FileFormat::Csv, true),
        Some("json" | "jsonl" | "ndjson") => (FileFormat::Json, false),
        Some("sql") => (FileFormat::Sql, false),
        Some(other) => return Err(Fail::usage(format!("unknown import format “{other}”"))),
        None => {
            let tab = file
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("tsv"));
            let f = FileFormat::guess(&file).ok_or_else(|| {
                Fail::usage("can't tell the file's format from its name: give --format")
            })?;
            (f, tab)
        }
    };
    let env = target.environment();
    if env.read_only {
        return Err(Fail::from(format!(
            "{} is read-only, so nothing can be imported into it",
            env.name
        )));
    }
    if (env.is_sensitive() || args.flag("truncate")) && !args.flag("yes") {
        return Err(Fail::from(format!(
            "importing into {}{} needs --yes",
            env.name,
            if args.flag("truncate") {
                " and emptying the table first"
            } else {
                ", a staging or production connection,"
            }
        )));
    }
    let net = runtime()?;
    let open = connect(args, &target, &net)?;
    let h = net.handle();
    let namespace = args.value("namespace").map(args::path);
    let progress = Progress::default();
    if format == FileFormat::Sql {
        let namespace = match args.value("into") {
            Some(spec) => {
                Some(find_object(&h, &open.conn, spec, args.value("namespace"))?.namespace)
            }
            None => namespace,
        };
        let outcome = with_progress(&progress, "statements", || {
            import::run_script(&h, &open.conn, namespace, &file, &progress)
        });
        return report(outcome);
    }
    let spec = args
        .value("into")
        .ok_or_else(|| Fail::usage("name the table or collection to import into with --into"))?;
    let object = find_object(&h, &open.conn, spec, args.value("namespace"))?;
    let info = h
        .block_on(open.conn.describe(&object))
        .map_err(|e| Fail::from(e.to_string()))?;
    let delimiter = match args.value("delimiter") {
        Some("\\t" | "tab") => b'\t',
        Some(d) if d.len() == 1 => d.as_bytes()[0],
        Some(_) => return Err(Fail::usage("--delimiter takes one character, or tab")),
        None if tab => b'\t',
        None => b',',
    };
    let csv = CsvRead {
        delimiter,
        header: !args.flag("no-header"),
        null_text: args.value("null").unwrap_or_default().to_owned(),
    };
    let preview = import::preview(&file, format, &csv).map_err(Fail::from)?;
    let targets = import::auto_map(&preview.columns, &info.columns);
    if targets.iter().all(Option::is_none) {
        return Err(Fail::from(format!(
            "none of the file's columns ({}) match a column of {}",
            preview.columns.join(", "),
            object.name
        )));
    }
    for (name, t) in preview.columns.iter().zip(&targets) {
        if t.is_none() {
            eprintln!(
                "Skipping the file's column {name}: {} has no such column",
                object.name
            );
        }
    }
    let plan = Plan {
        object,
        columns: info.columns,
        format,
        csv,
        file_columns: preview.columns,
        targets,
        skip_bad: args.flag("skip-bad"),
        truncate: args.flag("truncate"),
    };
    let outcome = with_progress(&progress, "rows", || {
        import::run_import(&h, &open.conn, &plan, &file, &progress)
    });
    report(outcome)
}

/// Finds a table, view or collection by name, `schema.name`, or `db.schema.name`, in
/// `namespace` when given. Several of the same name in different places is an error that
/// lists them.
fn find_object(
    h: &tokio::runtime::Handle,
    conn: &Arc<dyn Connection>,
    spec: &str,
    namespace: Option<&str>,
) -> Result<ObjectRef, Fail> {
    let mut parts = args::path(spec);
    let name = parts
        .pop()
        .ok_or_else(|| Fail::usage("the table name is empty"))?;
    let mut prefix = namespace.map(args::path).unwrap_or_default();
    prefix.extend(parts);
    let namespaces = h
        .block_on(conn.namespaces())
        .map_err(|e| Fail::from(e.to_string()))?;
    let mut found: Vec<ObjectRef> = Vec::new();
    let mut searched = 0;
    for ns in namespaces.iter().filter(|n| n.path.ends_with(&prefix)) {
        searched += 1;
        // Another database's schemas can refuse a listing; they are skipped.
        let Ok(list) = h.block_on(conn.objects(&ns.path)) else {
            continue;
        };
        let exact = list
            .iter()
            .filter(|o| o.object.kind.has_rows() && o.object.name == name);
        found.extend(exact.map(|o| o.object.clone()));
    }
    if found.is_empty() && searched == 0 && !prefix.is_empty() {
        // A namespace the listing does not show, such as one the user cannot list: try
        // the object directly.
        return Ok(ObjectRef {
            namespace: prefix,
            name,
            kind: deltos_data::ObjectKind::Table,
        });
    }
    match found.len() {
        0 => Err(Fail::from(format!(
            "no table, view or collection named {name}{}",
            if prefix.is_empty() {
                String::new()
            } else {
                format!(" in {}", prefix.join("."))
            }
        ))),
        1 => Ok(found.remove(0)),
        _ => Err(Fail::from(format!(
            "{name} is in several places: {}. Name one, such as {}.{name}",
            found
                .iter()
                .map(|o| o.namespace.join("."))
                .collect::<Vec<_>>()
                .join(", "),
            found[0].namespace.join(".")
        ))),
    }
}

/// Runs `job` while a line on standard error counts what it has done, when that is a
/// terminal.
fn with_progress(progress: &Progress, unit: &str, job: impl FnOnce() -> Outcome) -> Outcome {
    if !std::io::stderr().is_terminal() {
        return job();
    }
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut shown = false;
            while !done.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
                let n = progress.rows.load(Ordering::Relaxed);
                if n > 0 {
                    eprint!("\r{n} {unit}…");
                    shown = true;
                }
            }
            if shown {
                eprint!("\r\x1b[2K");
            }
        });
        let outcome = job();
        done.store(true, Ordering::Relaxed);
        outcome
    })
}

fn report(outcome: Outcome) -> Done {
    for line in &outcome.details {
        eprintln!("{line}");
    }
    if outcome.failed {
        return Err(Fail::from(outcome.message));
    }
    eprintln!("{}", outcome.message);
    Ok(())
}

// ---- output helpers ---------------------------------------------------------------------

/// Standard output or a file. A reader that goes away early, as `head` does, ends the
/// command quietly.
struct Out {
    inner: Box<dyn Write>,
    closed: bool,
}

impl Out {
    fn new(path: Option<&Path>) -> Result<Self, Fail> {
        let inner: Box<dyn Write> = match path {
            Some(p) => Box::new(std::io::BufWriter::new(
                std::fs::File::create(p)
                    .map_err(|e| Fail::from(format!("{}: {e}", p.display())))?,
            )),
            None => Box::new(std::io::BufWriter::new(std::io::stdout().lock())),
        };
        Ok(Out {
            inner,
            closed: false,
        })
    }

    fn write(&mut self, bytes: &[u8]) -> Done {
        if self.closed || bytes.is_empty() {
            return Ok(());
        }
        match self.inner.write_all(bytes) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                self.closed = true;
                Ok(())
            }
            Err(e) => Err(Fail::from(format!("could not write the output: {e}"))),
        }
    }

    fn finish(&mut self) -> Done {
        if self.closed {
            return Ok(());
        }
        match self.inner.flush() {
            Err(e) if e.kind() != std::io::ErrorKind::BrokenPipe => {
                Err(Fail::from(format!("could not write the output: {e}")))
            }
            _ => Ok(()),
        }
    }
}

fn print_out(text: &str) -> Done {
    let mut out = Out::new(None)?;
    out.write(text.as_bytes())?;
    out.finish()
}

fn print_table(columns: &[deltos_data::Column], rows: &[Vec<deltos_data::Value>]) -> Done {
    let mut text = deltos_grid::write_rows(&mut output::TableWriter::default(), columns, rows);
    text.push('\n');
    print_out(&text)
}

fn text_columns(names: &[&str]) -> Vec<deltos_data::Column> {
    names
        .iter()
        .map(|n| deltos_data::Column::new(*n, "text", deltos_data::ValueKind::Text))
        .collect()
}

fn is_broken_pipe(message: &str) -> bool {
    message.contains("Broken pipe") || message.contains("os error 32")
}

fn count(n: u64, one: &str, many: &str) -> String {
    crate::ui::transfer::count(n, one, many)
}
