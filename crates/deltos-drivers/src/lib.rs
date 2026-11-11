//! Database drivers for Deltos: PostgreSQL, MySQL and MariaDB, SQLite and MongoDB, each
//! implementing [`deltos_data::Driver`] and [`deltos_data::Connection`] behind a cargo
//! feature of the same name (all on by default).
//!
//! # Design
//!
//! **Bounded memory everywhere.** Nothing here reads a whole result. `execute` hands back
//! a [`RowStream`](deltos_data::RowStream) whose batches are pulled by the caller, and each
//! driver reads at most one batch ahead of that:
//!
//! - PostgreSQL streams the statement with the extended protocol (`query_raw`). The client
//!   library stops reading the socket when its one-slot response channel is full, so TCP
//!   flow control pauses the server while the caller is not pulling. We chose this over
//!   portals (`query_portal`) because portals only live inside a transaction block: they
//!   would wrap every statement the person runs in a `BEGIN` we have to end for them,
//!   which breaks their own `BEGIN`/`COMMIT`, `CREATE DATABASE`, `VACUUM` and anything
//!   else that cannot run in a transaction. A stream dropped before its end sends a
//!   cancel request, so the server stops producing rows nobody will read.
//! - MySQL reads its text protocol result set row by row from a task that owns the
//!   connection, through a channel with room for one batch. A stream dropped early
//!   issues `KILL QUERY` from a side connection before the rest is drained.
//! - SQLite runs on one thread per connection that steps statements on request. Several
//!   statements can be open at once, and the thread keeps serving other requests while
//!   a result waits to be read further.
//! - MongoDB reads its server cursor from a task that fetches the next batch only once the
//!   caller has taken the last one. Dropping the stream closes the cursor on the server.
//!
//! **Two sessions for the SQL servers.** Statements typed by the person run on a query
//! session; the sidebar, the table view and edits use a separate metadata session, opened
//! first. A slow or half-read result therefore never blocks browsing, and the person's
//! own transaction state stays where they put it.
//!
//! **Browsing any table size.** [`browse`](deltos_data::Connection::browse) orders rows
//! by the sort columns, then the primary key, falling back to the row identity (`ctid`,
//! `rowid`, `_id`). With `seek_after` it seeks with a key condition instead of an offset;
//! see [`sql`] for how that condition is built and how NULLs are handled, and
//! [`mongodb`] for the document side. All values go
//! to the server as bound parameters, never spliced into SQL text.
//!
//! **Any executor.** The connections run their network work on Tokio. Every method spawns
//! onto the runtime the driver was first used from, or onto a small runtime of this crate
//! when there is none, so callers may poll the returned futures from any executor.

// Helpers shared by the drivers go unused in builds that leave some drivers out.
#![cfg_attr(
    not(all(
        feature = "postgres",
        feature = "mysql",
        feature = "sqlite",
        feature = "mongodb"
    )),
    allow(dead_code, unused_imports)
)]

use std::sync::Arc;

use deltos_data::{Driver, DriverKind};

pub mod classify;
mod runtime;
pub mod sql;
mod tunnel;
mod url;
mod util;

#[cfg(feature = "tls")]
mod tls;

#[cfg(feature = "mongodb")]
pub mod mongodb;
#[cfg(feature = "mysql")]
pub mod mysql;
#[cfg(feature = "postgres")]
pub mod postgres;
#[cfg(feature = "sqlite")]
pub mod sqlite;

pub use tunnel::{through_tunnel, tunnel_target};
pub use url::{kind_from_url, parse_url};

/// The driver for a kind of database, when this build includes one.
pub fn driver(kind: DriverKind) -> Option<Arc<dyn Driver>> {
    match kind {
        #[cfg(feature = "postgres")]
        DriverKind::Postgres => Some(Arc::new(postgres::PostgresDriver)),
        #[cfg(feature = "mysql")]
        DriverKind::Mysql => Some(Arc::new(mysql::MysqlDriver)),
        #[cfg(feature = "sqlite")]
        DriverKind::Sqlite => Some(Arc::new(sqlite::SqliteDriver)),
        #[cfg(feature = "mongodb")]
        DriverKind::Mongodb => Some(Arc::new(mongodb::MongoDriver)),
        _ => None,
    }
}

/// The kinds this build can connect to, in [`DriverKind::ALL`] order.
pub fn available() -> Vec<DriverKind> {
    DriverKind::ALL
        .into_iter()
        .filter(|kind| driver(*kind).is_some())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_matches_features() {
        let kinds = available();
        assert_eq!(
            kinds.contains(&DriverKind::Postgres),
            cfg!(feature = "postgres")
        );
        assert_eq!(kinds.contains(&DriverKind::Mysql), cfg!(feature = "mysql"));
        assert_eq!(
            kinds.contains(&DriverKind::Sqlite),
            cfg!(feature = "sqlite")
        );
        assert_eq!(
            kinds.contains(&DriverKind::Mongodb),
            cfg!(feature = "mongodb")
        );
        assert!(!kinds.contains(&DriverKind::Redis));
        for kind in kinds {
            assert_eq!(driver(kind).map(|d| d.kind()), Some(kind));
        }
    }
}
