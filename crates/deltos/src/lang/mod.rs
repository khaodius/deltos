//! What the query editor knows about the languages it edits: where statements start and
//! end, what to offer while typing, how to lay SQL out, and how to read the plans that
//! `EXPLAIN` returns. Everything here is plain functions over text, with no UI, so it is
//! tested on its own.

pub mod complete;
pub mod format;
pub mod lex;
pub mod plan;
pub mod split;

use deltos_data::DriverKind;
pub use deltos_drivers::sql::Dialect;

/// The language an editor holds, from the kind of its connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Sql(Dialect),
    /// The MongoDB shell subset the driver reads.
    Mongo,
}

impl Default for Language {
    fn default() -> Self {
        Language::Sql(Dialect::Postgres)
    }
}

impl Language {
    pub fn for_kind(kind: DriverKind) -> Self {
        match kind {
            DriverKind::Mysql => Language::Sql(Dialect::Mysql),
            DriverKind::Sqlite => Language::Sql(Dialect::Sqlite),
            DriverKind::Mongodb => Language::Mongo,
            _ => Language::Sql(Dialect::Postgres),
        }
    }

    pub fn dialect(self) -> Option<Dialect> {
        match self {
            Language::Sql(d) => Some(d),
            Language::Mongo => None,
        }
    }
}
