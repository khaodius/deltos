use deltos_data::{Column, ForeignKey, Index, ValueKind};

use super::*;

fn object(namespace: &[&str], name: &str) -> ObjectRef {
    ObjectRef {
        namespace: namespace.iter().map(|s| s.to_string()).collect(),
        name: name.into(),
        kind: ObjectKind::Table,
    }
}

fn column(name: &str, ty: &str, nullable: bool, key: Option<u16>) -> Column {
    Column {
        nullable: Some(nullable),
        primary_key: key,
        ..Column::new(name, ty, ValueKind::Text)
    }
}

fn index(name: &str, columns: &[&str], unique: bool, constraint: bool) -> Index {
    Index {
        name: name.into(),
        columns: columns.iter().map(|s| s.to_string()).collect(),
        unique,
        primary: false,
        method: None,
        constraint,
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// `orders(id pk, user_id, status default 'open', note)` with an index, a unique
/// constraint and a key to `users`.
fn orders_info(namespace: &[&str]) -> ObjectInfo {
    ObjectInfo {
        columns: vec![
            column("id", "integer", false, Some(0)),
            column("user_id", "integer", true, None),
            Column {
                default: Some("'open'::text".into()),
                ..column("status", "text", false, None)
            },
            Column {
                comment: Some("free text".into()),
                ..column("note", "text", true, None)
            },
        ],
        indexes: vec![
            Index {
                primary: true,
                ..index("orders_pkey", &["id"], true, true)
            },
            index("orders_status_idx", &["status"], false, false),
            index("orders_note_key", &["note"], true, true),
        ],
        foreign_keys: vec![ForeignKey {
            name: "orders_user_id_fkey".into(),
            columns: strings(&["user_id"]),
            target: object(namespace, "users"),
            target_columns: strings(&["id"]),
            on_delete: ForeignKeyAction::Cascade,
            on_update: ForeignKeyAction::NoAction,
        }],
        ..Default::default()
    }
}

fn find(draft: &Draft, name: &str) -> usize {
    draft
        .columns
        .iter()
        .position(|c| c.def.name == name)
        .expect("column")
}

#[test]
fn reads_versions() {
    assert_eq!(version_of("SQLite 3.45.1"), Some((3, 45)));
    assert_eq!(version_of("MySQL 8.0.36"), Some((8, 0)));
    assert_eq!(version_of("nothing"), None);
    assert_eq!(
        Flavor::of(DriverKind::Sqlite, "SQLite 3.34.0"),
        Some(Flavor::Sqlite { drop_column: false })
    );
    assert_eq!(
        Flavor::of(DriverKind::Sqlite, "SQLite 3.53.0"),
        Some(Flavor::Sqlite { drop_column: true })
    );
    assert_eq!(Flavor::of(DriverKind::Redis, ""), None);
}

#[test]
fn qualifies_per_database() {
    let ns = strings(&["shop", "sales"]);
    assert_eq!(qualifier(Flavor::Postgres, &ns), strings(&["sales"]));
    assert_eq!(
        qualifier(Flavor::Mysql, &strings(&["shop"])),
        strings(&["shop"])
    );
    let sqlite = Flavor::Sqlite { drop_column: true };
    assert!(qualifier(sqlite, &strings(&["main"])).is_empty());
    assert_eq!(qualifier(sqlite, &strings(&["aux"])), strings(&["aux"]));
    assert!(qualifier(Flavor::Mongo, &strings(&["shop"])).is_empty());
}

#[test]
fn unchanged_draft_has_no_statements() {
    for flavor in [
        Flavor::Postgres,
        Flavor::Mysql,
        Flavor::Sqlite { drop_column: true },
        Flavor::Mongo,
    ] {
        let table = TableDef::from_info(
            flavor,
            &object(&["shop", "public"], "orders"),
            &orders_info(&["shop", "public"]),
        );
        let plan = super::plan(flavor, &table, &Draft::of(&table)).expect("plan");
        assert!(plan.is_empty(), "{flavor:?}: {:?}", plan.statements);
    }
}

#[test]
fn postgres_alters_columns_in_order() {
    let ns = ["shop", "public"];
    let table = TableDef::from_info(Flavor::Postgres, &object(&ns, "orders"), &orders_info(&ns));
    let mut draft = Draft::of(&table);
    let status = find(&draft, "status");
    draft.columns[status].def.name = "state".into();
    draft.columns[status].def.type_name = "varchar(20)".into();
    draft.columns[status].def.nullable = true;
    draft.columns[status].def.default = None;
    let note = find(&draft, "note");
    draft.columns[note].def.comment = Some("it's free".into());
    let user = find(&draft, "user_id");
    draft.columns.remove(user);
    draft.columns.push(Staged {
        from: None,
        def: ColumnDef {
            nullable: false,
            default: Some("0".into()),
            comment: Some("new".into()),
            ..ColumnDef::new("we\"ird", "integer")
        },
    });
    // The plain index and the key go; a new index on the renamed column comes.
    draft
        .indexes
        .retain(|i| i.def.name != "orders_status_idx" && i.def.name != "orders_note_key");
    draft.foreign_keys.clear();
    draft.indexes.push(Staged {
        from: None,
        def: IndexDef {
            name: "orders_state_idx".into(),
            columns: strings(&["state", "we\"ird"]),
            unique: true,
            ..Default::default()
        },
    });
    let plan = super::plan(Flavor::Postgres, &table, &draft).expect("plan");
    let t = "\"public\".\"orders\"";
    assert_eq!(
        plan.statements,
        vec![
            format!("ALTER TABLE {t} DROP CONSTRAINT \"orders_user_id_fkey\""),
            "DROP INDEX \"public\".\"orders_status_idx\"".to_string(),
            format!("ALTER TABLE {t} DROP CONSTRAINT \"orders_note_key\""),
            format!("ALTER TABLE {t} DROP COLUMN \"user_id\""),
            format!("ALTER TABLE {t} RENAME COLUMN \"status\" TO \"state\""),
            format!("ALTER TABLE {t} ALTER COLUMN \"state\" DROP DEFAULT"),
            format!(
                "ALTER TABLE {t} ALTER COLUMN \"state\" TYPE varchar(20) USING \"state\"::varchar(20)"
            ),
            format!("ALTER TABLE {t} ALTER COLUMN \"state\" DROP NOT NULL"),
            format!("COMMENT ON COLUMN {t}.\"note\" IS 'it''s free'"),
            format!("ALTER TABLE {t} ADD COLUMN \"we\"\"ird\" integer DEFAULT 0 NOT NULL"),
            format!("COMMENT ON COLUMN {t}.\"we\"\"ird\" IS 'new'"),
            format!("CREATE UNIQUE INDEX \"orders_state_idx\" ON {t} (\"state\", \"we\"\"ird\")"),
        ]
    );
    assert!(plan.notes.is_empty());
}

#[test]
fn postgres_adds_foreign_keys_with_actions() {
    let ns = ["shop", "public"];
    let table = TableDef::from_info(Flavor::Postgres, &object(&ns, "orders"), &orders_info(&ns));
    let mut draft = Draft::of(&table);
    draft.foreign_keys.push(Staged {
        from: None,
        def: ForeignKeyDef {
            name: "orders_note_fkey".into(),
            columns: strings(&["note"]),
            target: object(&["shop", "other"], "notes"),
            target_columns: strings(&["body"]),
            on_delete: ForeignKeyAction::SetNull,
            on_update: ForeignKeyAction::Cascade,
        },
    });
    // Setting a default, making a column NOT NULL.
    let user = find(&draft, "user_id");
    draft.columns[user].def.nullable = false;
    draft.columns[user].def.default = Some("1".into());
    let plan = super::plan(Flavor::Postgres, &table, &draft).expect("plan");
    assert_eq!(
        plan.statements,
        vec![
            "ALTER TABLE \"public\".\"orders\" ALTER COLUMN \"user_id\" SET NOT NULL",
            "ALTER TABLE \"public\".\"orders\" ALTER COLUMN \"user_id\" SET DEFAULT 1",
            "ALTER TABLE \"public\".\"orders\" ADD CONSTRAINT \"orders_note_fkey\" FOREIGN KEY (\"note\") REFERENCES \"other\".\"notes\" (\"body\") ON DELETE SET NULL ON UPDATE CASCADE",
        ]
    );
}

#[test]
fn refuses_primary_key_changes_and_bad_drafts() {
    let ns = ["shop", "public"];
    let table = TableDef::from_info(Flavor::Postgres, &object(&ns, "orders"), &orders_info(&ns));
    let mut draft = Draft::of(&table);
    draft.columns.retain(|c| c.def.name != "id");
    assert!(
        plan(Flavor::Postgres, &table, &draft)
            .unwrap_err()
            .contains("primary key")
    );

    let mut draft = Draft::of(&table);
    let user = find(&draft, "user_id");
    draft.columns[user].def.primary_key = Some(1);
    assert!(
        plan(Flavor::Postgres, &table, &draft)
            .unwrap_err()
            .contains("primary key")
    );

    let mut draft = Draft::of(&table);
    draft.columns[1].def.name = "status".into();
    assert_eq!(
        plan(Flavor::Postgres, &table, &draft).unwrap_err(),
        "There are two columns named status."
    );

    let mut draft = Draft::of(&table);
    draft.columns.push(Staged {
        from: None,
        def: ColumnDef::new("x", " "),
    });
    assert_eq!(
        plan(Flavor::Postgres, &table, &draft).unwrap_err(),
        "Column x needs a type."
    );

    let mut draft = Draft::of(&table);
    draft.indexes.push(Staged {
        from: None,
        def: IndexDef {
            name: "i".into(),
            columns: strings(&["nope"]),
            ..Default::default()
        },
    });
    assert!(
        plan(Flavor::Postgres, &table, &draft)
            .unwrap_err()
            .contains("nope")
    );

    let mut draft = Draft::of(&table);
    draft.foreign_keys.push(Staged {
        from: None,
        def: ForeignKeyDef {
            name: "k".into(),
            columns: strings(&["note", "status"]),
            target: object(&ns, "users"),
            target_columns: strings(&["id"]),
            on_delete: ForeignKeyAction::NoAction,
            on_update: ForeignKeyAction::NoAction,
        },
    });
    assert!(
        plan(Flavor::Postgres, &table, &draft)
            .unwrap_err()
            .contains("as many")
    );

    let mut draft = Draft::of(&table);
    draft.columns.push(draft.columns[0].clone());
    assert!(
        plan(Flavor::Postgres, &table, &draft)
            .unwrap_err()
            .contains("twice")
    );
}

const MYSQL_DEFINITION: &str = "CREATE TABLE `orders` (
  `id` int unsigned NOT NULL AUTO_INCREMENT,
  `user_id` int DEFAULT NULL,
  `status` varchar(20) CHARACTER SET latin1 COLLATE latin1_bin NOT NULL DEFAULT 'open' COMMENT 'it''s the state',
  `total` decimal(10,2) NOT NULL DEFAULT '0.00',
  `seen` timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP,
  `made` datetime(3) DEFAULT current_timestamp(3),
  `flags` bit(1) DEFAULT b'0',
  `delta` int DEFAULT -1,
  `half` double GENERATED ALWAYS AS ((`total` / 2)) VIRTUAL,
  `note` text DEFAULT ('x,y'),
  PRIMARY KEY (`id`),
  KEY `orders_user_id_idx` (`user_id`),
  CONSTRAINT `orders_user_id_fkey` FOREIGN KEY (`user_id`) REFERENCES `users` (`id`) ON DELETE CASCADE
) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;";

#[test]
fn reads_mysql_column_clauses() {
    let columns = mysql_columns(MYSQL_DEFINITION);
    let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "id", "user_id", "status", "total", "seen", "made", "flags", "delta", "half", "note"
        ]
    );
    let by = |name: &str| columns.iter().find(|c| c.name == name).expect("column");
    assert_eq!(by("id").extra.as_deref(), Some("AUTO_INCREMENT"));
    assert_eq!(by("id").default, None);
    assert_eq!(by("user_id").default, None);
    assert_eq!(
        by("status").collation.as_deref(),
        Some("CHARACTER SET latin1 COLLATE latin1_bin")
    );
    assert_eq!(by("status").default.as_deref(), Some("'open'"));
    assert_eq!(by("total").default.as_deref(), Some("'0.00'"));
    assert_eq!(by("seen").default.as_deref(), Some("CURRENT_TIMESTAMP"));
    assert_eq!(
        by("seen").extra.as_deref(),
        Some("ON UPDATE CURRENT_TIMESTAMP")
    );
    assert_eq!(by("made").default.as_deref(), Some("current_timestamp(3)"));
    assert_eq!(by("flags").default.as_deref(), Some("b'0'"));
    assert_eq!(by("delta").default.as_deref(), Some("-1"));
    assert!(by("half").generated);
    assert_eq!(by("note").default.as_deref(), Some("('x,y')"));

    // Clauses in versioned comments, and MariaDB's bare INVISIBLE, are kept for MODIFY.
    let hidden = mysql_columns(
        "  `a` int DEFAULT NULL /*!80023 INVISIBLE */ COMMENT '/*!x */',\n  `b` int INVISIBLE AUTO_INCREMENT,\n  `g` point NOT NULL /*!80003 SRID 4326 */,",
    );
    let extras: Vec<Option<&str>> = hidden.iter().map(|c| c.extra.as_deref()).collect();
    assert_eq!(
        extras,
        [
            Some("/*!80023 INVISIBLE */"),
            Some("INVISIBLE AUTO_INCREMENT"),
            Some("/*!80003 SRID 4326 */")
        ]
    );
}

fn mysql_table() -> TableDef {
    let info = ObjectInfo {
        columns: vec![
            column("id", "int unsigned", false, Some(0)),
            column("user_id", "int", true, None),
            Column {
                default: Some("open".into()),
                comment: Some("it's the state".into()),
                ..column("status", "varchar(20)", false, None)
            },
            column("seen", "timestamp", false, None),
            column("half", "double", true, None),
        ],
        indexes: vec![
            Index {
                primary: true,
                ..index("PRIMARY", &["id"], true, true)
            },
            index("orders_user_id_idx", &["user_id"], false, false),
        ],
        foreign_keys: orders_info(&["shop"]).foreign_keys,
        definition: Some(MYSQL_DEFINITION.into()),
        ..Default::default()
    };
    TableDef::from_info(Flavor::Mysql, &object(&["shop"], "orders"), &info)
}

#[test]
fn mysql_restates_whole_columns() {
    let table = mysql_table();
    assert_eq!(table.columns[2].default.as_deref(), Some("'open'"));
    let mut draft = Draft::of(&table);
    // A comment change keeps the collation, default and comment quoting.
    draft.columns[2].def.comment = Some("back\\slash".into());
    // A rename keeps ON UPDATE.
    draft.columns[3].def.name = "seen_at".into();
    // The key column keeps AUTO_INCREMENT.
    draft.columns[0].def.type_name = "bigint unsigned".into();
    draft.columns.remove(1);
    draft.indexes.retain(|i| i.def.primary);
    draft.foreign_keys.clear();
    draft.columns.push(Staged {
        from: None,
        def: ColumnDef {
            comment: Some("added".into()),
            ..ColumnDef::new("added", "int")
        },
    });
    let plan = super::plan(Flavor::Mysql, &table, &draft).expect("plan");
    let t = "`shop`.`orders`";
    assert_eq!(
        plan.statements,
        vec![
            format!("ALTER TABLE {t} DROP FOREIGN KEY `orders_user_id_fkey`"),
            format!("ALTER TABLE {t} DROP INDEX `orders_user_id_idx`"),
            format!("ALTER TABLE {t} DROP COLUMN `user_id`"),
            format!("ALTER TABLE {t} MODIFY COLUMN `id` bigint unsigned NOT NULL AUTO_INCREMENT"),
            format!(
                "ALTER TABLE {t} MODIFY COLUMN `status` varchar(20) CHARACTER SET latin1 COLLATE latin1_bin NOT NULL DEFAULT 'open' COMMENT 'back\\\\slash'"
            ),
            format!(
                "ALTER TABLE {t} CHANGE COLUMN `seen` `seen_at` timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP"
            ),
            format!("ALTER TABLE {t} ADD COLUMN `added` int NULL COMMENT 'added'"),
        ]
    );
    assert!(plan.notes[0].contains("commits each statement"));
}

#[test]
fn mysql_keeps_collation_while_the_column_holds_text_and_refuses_generated() {
    let table = mysql_table();
    let mut draft = Draft::of(&table);
    draft.columns[2].def.type_name = "varchar(50)".into();
    let plan_text = plan(Flavor::Mysql, &table, &draft)
        .expect("plan")
        .statements;
    assert_eq!(
        plan_text,
        vec![
            "ALTER TABLE `shop`.`orders` MODIFY COLUMN `status` varchar(50) CHARACTER SET latin1 COLLATE latin1_bin NOT NULL DEFAULT 'open' COMMENT 'it''s the state'"
        ]
    );
    // A type that holds no text has no character set.
    draft.columns[2].def.type_name = "int".into();
    draft.columns[2].def.default = Some("0".into());
    let plan_text = plan(Flavor::Mysql, &table, &draft)
        .expect("plan")
        .statements;
    assert_eq!(
        plan_text,
        vec![
            "ALTER TABLE `shop`.`orders` MODIFY COLUMN `status` int NOT NULL DEFAULT 0 COMMENT 'it''s the state'"
        ]
    );
    let mut draft = Draft::of(&table);
    draft.columns[4].def.nullable = false;
    assert!(
        plan(Flavor::Mysql, &table, &draft)
            .unwrap_err()
            .contains("generated")
    );
}

#[test]
fn mysql_adds_indexes_and_keys() {
    let table = mysql_table();
    let mut draft = Draft::of(&table);
    draft.indexes.push(Staged {
        from: None,
        def: IndexDef {
            name: index_name(Flavor::Mysql, "orders", &strings(&["status", "seen"]), true),
            columns: strings(&["status", "seen"]),
            unique: true,
            ..Default::default()
        },
    });
    draft.foreign_keys.push(Staged {
        from: None,
        def: ForeignKeyDef {
            name: foreign_key_name("orders", &strings(&["status"])),
            columns: strings(&["status"]),
            target: object(&["shop"], "statuses"),
            target_columns: strings(&["code"]),
            on_delete: ForeignKeyAction::Restrict,
            on_update: ForeignKeyAction::NoAction,
        },
    });
    assert_eq!(
        plan(Flavor::Mysql, &table, &draft)
            .expect("plan")
            .statements,
        vec![
            "CREATE UNIQUE INDEX `orders_status_seen_key` ON `shop`.`orders` (`status`, `seen`)",
            "ALTER TABLE `shop`.`orders` ADD CONSTRAINT `orders_status_fkey` FOREIGN KEY (`status`) REFERENCES `shop`.`statuses` (`code`) ON DELETE RESTRICT",
        ]
    );
}

const SQLITE_DEFINITION: &str = "CREATE TABLE orders (id INTEGER PRIMARY KEY AUTOINCREMENT, user_id INTEGER REFERENCES users(id) ON DELETE CASCADE, status TEXT NOT NULL DEFAULT 'open', note TEXT UNIQUE);

CREATE INDEX orders_status_idx ON orders (status) WHERE status <> 'done';

CREATE TRIGGER orders_touch AFTER UPDATE ON orders BEGIN
  UPDATE orders SET note = 'touched; again' WHERE id = new.id;
END;";

fn sqlite_info() -> ObjectInfo {
    ObjectInfo {
        columns: vec![
            column("id", "INTEGER", false, Some(0)),
            column("user_id", "INTEGER", true, None),
            Column {
                default: Some("'open'".into()),
                ..column("status", "TEXT", false, None)
            },
            column("note", "TEXT", true, None),
        ],
        indexes: vec![
            index("orders_status_idx", &["status"], false, false),
            index("sqlite_autoindex_orders_1", &["note"], true, true),
        ],
        foreign_keys: vec![ForeignKey {
            name: "fk_orders_0".into(),
            columns: strings(&["user_id"]),
            target: object(&["main"], "users"),
            target_columns: strings(&["id"]),
            on_delete: ForeignKeyAction::Cascade,
            on_update: ForeignKeyAction::NoAction,
        }],
        definition: Some(SQLITE_DEFINITION.into()),
        ..Default::default()
    }
}

const SQLITE: Flavor = Flavor::Sqlite { drop_column: true };

#[test]
fn sqlite_reads_what_a_rebuild_needs() {
    let table = TableDef::from_info(SQLITE, &object(&["main"], "orders"), &sqlite_info());
    assert!(table.qualifier.is_empty());
    assert_eq!(table.rebuild.blocker, None);
    assert!(table.rebuild.autoincrement);
    assert_eq!(table.rebuild.triggers.len(), 1);
    assert!(table.rebuild.triggers[0].ends_with("END"));
    assert_eq!(
        table.indexes[0].sql.as_deref(),
        Some("CREATE INDEX orders_status_idx ON orders (status) WHERE status <> 'done'")
    );
    assert_eq!(table.indexes[1].sql, None);
}

#[test]
fn sqlite_changes_in_place_when_it_can() {
    let table = TableDef::from_info(SQLITE, &object(&["main"], "orders"), &sqlite_info());
    let mut draft = Draft::of(&table);
    draft.columns[2].def.name = "state".into();
    draft.indexes.retain(|i| i.def.name != "orders_status_idx");
    draft.columns.push(Staged {
        from: None,
        def: ColumnDef {
            nullable: false,
            default: Some("0".into()),
            ..ColumnDef::new("n", "INTEGER")
        },
    });
    draft.indexes.push(Staged {
        from: None,
        def: IndexDef {
            name: "orders_n_idx".into(),
            columns: strings(&["n"]),
            ..Default::default()
        },
    });
    let plan = super::plan(SQLITE, &table, &draft).expect("plan");
    assert_eq!(
        plan.statements,
        vec![
            "DROP INDEX \"orders_status_idx\"",
            "ALTER TABLE \"orders\" RENAME COLUMN \"status\" TO \"state\"",
            "ALTER TABLE \"orders\" ADD COLUMN \"n\" INTEGER NOT NULL DEFAULT 0",
            "CREATE INDEX \"orders_n_idx\" ON \"orders\" (\"n\")",
        ]
    );
    assert!(plan.notes.is_empty());

    // Dropping a column in place, in an attached schema.
    let aux = TableDef::from_info(SQLITE, &object(&["aux"], "orders"), &sqlite_info());
    let mut draft = Draft::of(&aux);
    draft.indexes.retain(|i| i.def.name != "orders_status_idx");
    draft.columns.retain(|c| c.def.name != "status");
    assert_eq!(
        super::plan(SQLITE, &aux, &draft).expect("plan").statements,
        vec![
            "DROP INDEX \"aux\".\"orders_status_idx\"",
            "ALTER TABLE \"aux\".\"orders\" DROP COLUMN \"status\"",
        ]
    );
}

#[test]
fn sqlite_rebuilds_for_other_changes() {
    let table = TableDef::from_info(SQLITE, &object(&["main"], "orders"), &sqlite_info());
    let mut draft = Draft::of(&table);
    draft.columns[2].def.type_name = "VARCHAR(20)".into();
    draft.columns[2].def.default = None;
    draft.columns.retain(|c| c.def.name != "user_id");
    draft.foreign_keys.clear();
    let plan = super::plan(SQLITE, &table, &draft).expect("plan");
    assert_eq!(
        plan.statements,
        vec![
            "CREATE TABLE \"_deltos_new_orders\" (\n  \"id\" INTEGER NOT NULL PRIMARY KEY AUTOINCREMENT,\n  \"status\" VARCHAR(20) NOT NULL,\n  \"note\" TEXT,\n  UNIQUE (\"note\")\n)".to_string(),
            "INSERT INTO \"_deltos_new_orders\" (\"id\", \"status\", \"note\") SELECT \"id\", \"status\", \"note\" FROM \"orders\"".to_string(),
            "INSERT INTO \"sqlite_sequence\" (name, seq) SELECT '_deltos_new_orders', 0 WHERE NOT EXISTS (SELECT 1 FROM \"sqlite_sequence\" WHERE name = '_deltos_new_orders')".to_string(),
            "UPDATE \"sqlite_sequence\" SET seq = max(seq, coalesce((SELECT max(seq) FROM \"sqlite_sequence\" WHERE name = 'orders'), 0)) WHERE name = '_deltos_new_orders'".to_string(),
            "DROP TABLE \"orders\"".to_string(),
            "PRAGMA legacy_alter_table = ON".to_string(),
            "ALTER TABLE \"_deltos_new_orders\" RENAME TO \"orders\"".to_string(),
            "PRAGMA legacy_alter_table = OFF".to_string(),
            "CREATE INDEX orders_status_idx ON orders (status) WHERE status <> 'done'".to_string(),
            table.rebuild.triggers[0].clone(),
        ]
    );
    assert!(plan.notes[0].contains("rebuilt"));

    // Without DROP COLUMN, dropping a column rebuilds too.
    let old = Flavor::Sqlite { drop_column: false };
    let mut draft = Draft::of(&table);
    draft.indexes.retain(|i| i.def.name != "orders_status_idx");
    draft.columns.retain(|c| c.def.name != "status");
    let statements = super::plan(old, &table, &draft).expect("plan").statements;
    assert!(statements[0].starts_with("CREATE TABLE \"_deltos_new_orders\""));
    assert!(
        statements[0]
            .contains("FOREIGN KEY (\"user_id\") REFERENCES \"users\" (\"id\") ON DELETE CASCADE")
    );
    assert_eq!(statements.len(), 9);
}

#[test]
fn sqlite_refuses_what_it_cannot_rebuild() {
    let table = TableDef::from_info(SQLITE, &object(&["main"], "orders"), &sqlite_info());
    let mut draft = Draft::of(&table);
    draft.columns[2].def.name = "state".into();
    draft.columns[3].def.nullable = false;
    assert!(
        plan(SQLITE, &table, &draft)
            .unwrap_err()
            .contains("renames on their own")
    );

    let mut draft = Draft::of(&table);
    draft.columns.retain(|c| c.def.name != "status");
    assert!(
        plan(SQLITE, &table, &draft)
            .unwrap_err()
            .contains("index orders_status_idx")
    );

    let mut info = sqlite_info();
    info.definition = Some(
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER, status TEXT CHECK (status <> ''), note TEXT)"
            .into(),
    );
    let checked = TableDef::from_info(SQLITE, &object(&["main"], "orders"), &info);
    assert_eq!(
        checked.rebuild.blocker.as_deref(),
        Some("CHECK constraints")
    );
    let mut draft = Draft::of(&checked);
    draft.columns[3].def.nullable = false;
    assert!(
        plan(SQLITE, &checked, &draft)
            .unwrap_err()
            .contains("CHECK constraints")
    );

    for (sql, blocker) in [
        (
            "CREATE TABLE t (a TEXT COLLATE NOCASE)",
            "column collations",
        ),
        (
            "CREATE TABLE t (a INT, b INT AS (a + 1))",
            "generated columns",
        ),
        (
            "CREATE TABLE t (a INT PRIMARY KEY) WITHOUT ROWID",
            "WITHOUT ROWID storage",
        ),
        ("CREATE TABLE t (a INT) STRICT", "STRICT typing"),
        (
            "CREATE VIRTUAL TABLE t USING fts5(a)",
            "virtual table module",
        ),
    ] {
        let rebuild = sqlite_rebuild(sql, &mut []);
        assert_eq!(rebuild.blocker.as_deref(), Some(blocker), "{sql}");
    }
    // Words inside strings and quoted names do not count.
    let rebuild = sqlite_rebuild(
        "CREATE TABLE t (\"check\" TEXT DEFAULT 'COLLATE', [strict] INT, b INT DEFAULT (CAST(1 AS INT)))",
        &mut [],
    );
    assert_eq!(rebuild.blocker, None);
}

#[test]
fn mongo_changes_only_indexes() {
    let info = ObjectInfo {
        columns: vec![column("_id", "objectId", false, Some(0))],
        indexes: vec![
            Index {
                primary: true,
                ..index("_id_", &["_id"], true, false)
            },
            index("status_1", &["status"], false, false),
        ],
        ..Default::default()
    };
    let table = TableDef::from_info(Flavor::Mongo, &object(&["shop"], "orders"), &info);
    let mut draft = Draft::of(&table);
    draft.indexes.remove(1);
    draft.indexes.push(Staged {
        from: None,
        def: IndexDef {
            name: index_name(Flavor::Mongo, "orders", &strings(&["user.id", "at"]), true),
            columns: strings(&["user.id", "at"]),
            unique: true,
            ..Default::default()
        },
    });
    assert_eq!(
        plan(Flavor::Mongo, &table, &draft)
            .expect("plan")
            .statements,
        vec![
            "db.runCommand({dropIndexes: \"orders\", index: \"status_1\"})",
            "db.runCommand({createIndexes: \"orders\", indexes: [{key: {\"user.id\": 1, \"at\": 1}, name: \"user.id_1_at_1\", unique: true}]})",
        ]
    );
    let mut draft = Draft::of(&table);
    draft.indexes.remove(0);
    assert!(
        plan(Flavor::Mongo, &table, &draft)
            .unwrap_err()
            .contains("primary key")
    );
}

#[test]
fn creates_tables_per_dialect() {
    let mut table = TableDef::new(Flavor::Postgres, &strings(&["shop", "public"]), "items");
    table.columns = vec![
        ColumnDef {
            nullable: false,
            primary_key: Some(0),
            ..ColumnDef::new("id", "bigserial")
        },
        ColumnDef {
            default: Some("'x'".into()),
            comment: Some("the name".into()),
            ..ColumnDef::new("name", "text")
        },
        ColumnDef::new("order_id", "integer"),
    ];
    table.indexes = vec![IndexDef {
        name: "items_name_idx".into(),
        columns: strings(&["name"]),
        ..Default::default()
    }];
    table.foreign_keys = vec![ForeignKeyDef {
        name: "items_order_id_fkey".into(),
        columns: strings(&["order_id"]),
        target: object(&["shop", "public"], "orders"),
        target_columns: strings(&["id"]),
        on_delete: ForeignKeyAction::Cascade,
        on_update: ForeignKeyAction::NoAction,
    }];
    assert_eq!(
        create_table(Flavor::Postgres, &table)
            .expect("create")
            .statements,
        vec![
            "CREATE TABLE \"public\".\"items\" (\n  \"id\" bigserial NOT NULL,\n  \"name\" text DEFAULT 'x',\n  \"order_id\" integer,\n  PRIMARY KEY (\"id\"),\n  CONSTRAINT \"items_order_id_fkey\" FOREIGN KEY (\"order_id\") REFERENCES \"public\".\"orders\" (\"id\") ON DELETE CASCADE\n)",
            "COMMENT ON COLUMN \"public\".\"items\".\"name\" IS 'the name'",
            "CREATE INDEX \"items_name_idx\" ON \"public\".\"items\" (\"name\")",
        ]
    );

    let mut mysql = table.clone();
    mysql.qualifier = strings(&["shop"]);
    mysql.columns[0].type_name = "bigint unsigned".into();
    mysql.columns[0].extra = Some("AUTO_INCREMENT".into());
    mysql.foreign_keys[0].target = object(&["shop"], "orders");
    assert_eq!(
        create_table(Flavor::Mysql, &mysql)
            .expect("create")
            .statements,
        vec![
            "CREATE TABLE `shop`.`items` (\n  `id` bigint unsigned NOT NULL AUTO_INCREMENT,\n  `name` text NULL DEFAULT 'x' COMMENT 'the name',\n  `order_id` integer NULL,\n  PRIMARY KEY (`id`),\n  CONSTRAINT `items_order_id_fkey` FOREIGN KEY (`order_id`) REFERENCES `shop`.`orders` (`id`) ON DELETE CASCADE\n)",
            "CREATE INDEX `items_name_idx` ON `shop`.`items` (`name`)",
        ]
    );

    let mut sqlite = table.clone();
    sqlite.qualifier.clear();
    sqlite.columns[0].type_name = "INTEGER".into();
    sqlite.columns[1].comment = None;
    assert_eq!(
        create_table(SQLITE, &sqlite).expect("create").statements,
        vec![
            "CREATE TABLE \"items\" (\n  \"id\" INTEGER NOT NULL,\n  \"name\" text DEFAULT 'x',\n  \"order_id\" integer,\n  PRIMARY KEY (\"id\"),\n  FOREIGN KEY (\"order_id\") REFERENCES \"orders\" (\"id\") ON DELETE CASCADE\n)",
            "CREATE INDEX \"items_name_idx\" ON \"items\" (\"name\")",
        ]
    );

    assert!(create_table(Flavor::Mongo, &table).is_err());
    table.name = " ".into();
    assert!(create_table(Flavor::Postgres, &table).is_err());
    table.name = "t".into();
    table.columns.clear();
    assert!(create_table(Flavor::Postgres, &table).is_err());
}

#[test]
fn drops_and_empties_tables() {
    let pg = object(&["shop", "sales"], "o\"rders");
    assert_eq!(
        drop_table(Flavor::Postgres, &pg),
        "DROP TABLE \"sales\".\"o\"\"rders\""
    );
    assert_eq!(
        truncate_table(Flavor::Postgres, &pg),
        "TRUNCATE TABLE \"sales\".\"o\"\"rders\""
    );
    let my = object(&["shop"], "o`rders");
    assert_eq!(
        drop_table(Flavor::Mysql, &my),
        "DROP TABLE `shop`.`o``rders`"
    );
    assert_eq!(
        truncate_table(Flavor::Mysql, &my),
        "TRUNCATE TABLE `shop`.`o``rders`"
    );
    let lite = object(&["main"], "orders");
    assert_eq!(drop_table(SQLITE, &lite), "DROP TABLE \"orders\"");
    assert_eq!(truncate_table(SQLITE, &lite), "DELETE FROM \"orders\"");
    let view = ObjectRef {
        kind: ObjectKind::View,
        ..lite.clone()
    };
    assert_eq!(drop_table(SQLITE, &view), "DROP VIEW \"orders\"");
    let mongo = object(&["shop"], "a\"b");
    assert_eq!(
        drop_table(Flavor::Mongo, &mongo),
        "db.getCollection(\"a\\\"b\").drop()"
    );
    assert_eq!(
        truncate_table(Flavor::Mongo, &mongo),
        "db.getCollection(\"a\\\"b\").deleteMany({})"
    );
}

#[test]
fn tokens_never_fail() {
    for text in [
        "'unterminated",
        "\"open",
        "[open",
        "`open",
        "(((",
        "))",
        "é\"ü",
        "/* x",
        "-- x",
    ] {
        let _ = tokens(text, false, true);
        let _ = tokens(text, true, true);
        let _ = mysql_columns(&format!("  `{text}` int DEFAULT {text}"));
        let _ = sqlite_rebuild(text, &mut []);
    }
}

#[cfg(feature = "sqlite")]
mod sqlite_live {
    use deltos_data::{Connection, ConnectionConfig, Driver, ExecuteOptions, Value};
    use futures::StreamExt;

    use super::*;
    use crate::sqlite::SqliteDriver;

    struct TempDb(std::path::PathBuf);

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    async fn open(name: &str) -> (TempDb, std::sync::Arc<dyn Connection>) {
        let path =
            std::env::temp_dir().join(format!("deltos-ddl-{}-{name}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let config = ConnectionConfig {
            database: path.to_string_lossy().into_owned(),
            options: vec![("create".into(), "true".into())],
            ..Default::default()
        };
        let conn = SqliteDriver.connect(&config).await.expect("connect");
        (TempDb(path), conn)
    }

    async fn rows(conn: &dyn Connection, sql: &str) -> Vec<Vec<Value>> {
        let mut stream = conn
            .execute(sql, &ExecuteOptions::default())
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        let mut out = Vec::new();
        while let Some(batch) = stream.batches.next().await {
            out.extend(batch.expect("batch").rows.into_iter().map(|r| r.to_vec()));
        }
        out
    }

    async fn run(conn: &dyn Connection, script: &str) {
        for statement in script.split(";\n") {
            rows(conn, statement).await;
        }
    }

    #[tokio::test]
    async fn rebuild_keeps_rows_indexes_triggers_and_views() {
        let (_db, conn) = open("rebuild").await;
        run(
            &*conn,
            "PRAGMA foreign_keys = ON;\n\
             CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);\n\
             CREATE TABLE orders (id INTEGER PRIMARY KEY AUTOINCREMENT, user_id INTEGER REFERENCES users(id) ON DELETE CASCADE, status TEXT NOT NULL DEFAULT 'open', note TEXT UNIQUE);\n\
             CREATE INDEX orders_status_idx ON orders (status) WHERE status <> 'done';\n\
             CREATE TABLE log (msg TEXT);\n\
             CREATE TRIGGER orders_log AFTER INSERT ON orders BEGIN INSERT INTO log VALUES ('new; order'); END;\n\
             CREATE VIEW open_orders AS SELECT id, status FROM orders WHERE status = 'open';\n\
             INSERT INTO users VALUES (1, 'a'), (2, 'b');\n\
             INSERT INTO orders (user_id, status, note) VALUES (1, 'open', 'x'), (2, 'done', 'y')",
        )
        .await;
        let object = object(&["main"], "orders");
        let info = conn.describe(&object).await.expect("describe");
        assert_eq!(info.foreign_keys[0].on_delete, ForeignKeyAction::Cascade);
        let table = TableDef::from_info(SQLITE, &object, &info);
        assert_eq!(table.rebuild.blocker, None);
        let mut draft = Draft::of(&table);
        let status = find(&draft, "status");
        draft.columns[status].def.type_name = "VARCHAR(10)".into();
        draft.columns[status].def.default = Some("'new'".into());
        let plan = super::plan(SQLITE, &table, &draft).expect("plan");
        assert!(plan.statements[0].starts_with("CREATE TABLE"));
        conn.apply_schema(&[], &plan.statements)
            .await
            .expect("apply");

        let after = conn.describe(&object).await.expect("describe");
        let status = after
            .columns
            .iter()
            .find(|c| c.name == "status")
            .expect("status");
        assert_eq!(status.type_name, "VARCHAR(10)");
        assert_eq!(status.default.as_deref(), Some("'new'"));
        assert_eq!(after.foreign_keys.len(), 1);
        assert!(after.indexes.iter().any(|i| i.name == "orders_status_idx"));
        assert!(
            after
                .indexes
                .iter()
                .any(|i| i.unique && i.columns == ["note"])
        );
        assert_eq!(
            rows(&*conn, "SELECT count(*) FROM orders").await,
            vec![vec![Value::Int(2)]]
        );
        assert_eq!(
            rows(&*conn, "SELECT count(*) FROM open_orders").await,
            vec![vec![Value::Int(1)]]
        );
        // The trigger came back, and AUTOINCREMENT carries on past the copied rows.
        rows(&*conn, "INSERT INTO orders (user_id) VALUES (1)").await;
        assert_eq!(
            rows(
                &*conn,
                "SELECT max(id), (SELECT count(*) FROM log) FROM orders"
            )
            .await,
            vec![vec![Value::Int(3), Value::Int(3)]]
        );
        // Foreign keys are on again.
        assert_eq!(
            rows(&*conn, "PRAGMA foreign_keys").await,
            vec![vec![Value::Int(1)]]
        );
        assert!(
            conn.execute(
                "INSERT INTO orders (user_id) VALUES (9)",
                &ExecuteOptions::default()
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn rebuild_keeps_the_autoincrement_counter() {
        let (_db, conn) = open("seq").await;
        run(
            &*conn,
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, a TEXT);\n\
             INSERT INTO t (a) VALUES ('x'), ('y'), ('z');\n\
             DELETE FROM t WHERE id = 3",
        )
        .await;
        let object = object(&["main"], "t");
        for (ty, next) in [("VARCHAR(5)", 4), ("TEXT", 5)] {
            let table = TableDef::from_info(
                SQLITE,
                &object,
                &conn.describe(&object).await.expect("describe"),
            );
            let mut draft = Draft::of(&table);
            draft.columns[1].def.type_name = ty.into();
            let plan = super::plan(SQLITE, &table, &draft).expect("plan");
            conn.apply_schema(&[], &plan.statements)
                .await
                .expect("apply");
            // A deleted number is never handed out again, also by an emptied table.
            rows(&*conn, "INSERT INTO t (a) VALUES ('w')").await;
            assert_eq!(
                rows(&*conn, "SELECT max(id) FROM t").await,
                vec![vec![Value::Int(next)]]
            );
            assert_eq!(
                rows(&*conn, "SELECT name FROM sqlite_sequence").await,
                vec![vec![Value::text("t")]]
            );
            rows(&*conn, "DELETE FROM t").await;
        }
    }

    #[tokio::test]
    async fn a_failed_rebuild_turns_the_legacy_rename_off_again() {
        let (_db, conn) = open("legacy").await;
        let statements = [
            "PRAGMA legacy_alter_table = ON".to_string(),
            "SELECT * FROM nowhere".to_string(),
        ];
        assert!(conn.apply_schema(&[], &statements).await.is_err());
        assert_eq!(
            rows(&*conn, "PRAGMA legacy_alter_table").await,
            vec![vec![Value::Int(0)]]
        );
    }

    #[tokio::test]
    async fn failed_change_leaves_the_table_alone() {
        let (_db, conn) = open("fail").await;
        run(
            &*conn,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT);\nINSERT INTO t VALUES (1, NULL)",
        )
        .await;
        let object = object(&["main"], "t");
        let table = TableDef::from_info(
            SQLITE,
            &object,
            &conn.describe(&object).await.expect("describe"),
        );
        let mut draft = Draft::of(&table);
        draft.columns[1].def.nullable = false;
        draft.columns.push(Staged {
            from: None,
            def: ColumnDef::new("b", "TEXT"),
        });
        let plan = super::plan(SQLITE, &table, &draft).expect("plan");
        let error = conn.apply_schema(&[], &plan.statements).await.unwrap_err();
        assert!(error.to_string().contains("NOT NULL"), "{error}");
        assert!(error.to_string().contains("In: INSERT INTO"), "{error}");
        let info = conn.describe(&object).await.expect("describe");
        assert_eq!(info.columns.len(), 2);
        assert_eq!(
            rows(&*conn, "SELECT count(*) FROM t").await,
            vec![vec![Value::Int(1)]]
        );
        assert!(
            rows(
                &*conn,
                "SELECT name FROM sqlite_master WHERE name LIKE '_deltos%'"
            )
            .await
            .is_empty()
        );
    }

    #[tokio::test]
    async fn broken_foreign_keys_undo_the_change() {
        let (_db, conn) = open("fk").await;
        run(
            &*conn,
            "PRAGMA foreign_keys = ON;\n\
             CREATE TABLE users (id INTEGER PRIMARY KEY);\n\
             CREATE TABLE orders (id INTEGER PRIMARY KEY, user_id INTEGER);\n\
             INSERT INTO orders VALUES (1, 7)",
        )
        .await;
        let object = object(&["main"], "orders");
        let table = TableDef::from_info(
            SQLITE,
            &object,
            &conn.describe(&object).await.expect("describe"),
        );
        let mut draft = Draft::of(&table);
        draft.foreign_keys.push(Staged {
            from: None,
            def: ForeignKeyDef {
                name: String::new(),
                columns: strings(&["user_id"]),
                target: super::object(&["main"], "users"),
                target_columns: strings(&["id"]),
                on_delete: ForeignKeyAction::NoAction,
                on_update: ForeignKeyAction::NoAction,
            },
        });
        let plan = super::plan(SQLITE, &table, &draft).expect("plan");
        let error = conn.apply_schema(&[], &plan.statements).await.unwrap_err();
        assert!(error.to_string().contains("foreign key"), "{error}");
        assert!(
            conn.describe(&object)
                .await
                .expect("describe")
                .foreign_keys
                .is_empty()
        );
    }
}

#[cfg(feature = "postgres")]
mod postgres_live {
    use deltos_data::{Connection, ConnectionConfig, Driver, ExecuteOptions, TlsMode};
    use futures::StreamExt;

    use super::*;
    use crate::postgres::PostgresDriver;

    async fn connect() -> Option<std::sync::Arc<dyn Connection>> {
        let url = std::env::var("DELTOS_TEST_POSTGRES_URL")
            .ok()
            .filter(|u| !u.trim().is_empty())?;
        let config = ConnectionConfig {
            url: Some(url),
            tls: TlsMode::Prefer,
            ..Default::default()
        };
        Some(PostgresDriver.connect(&config).await.expect("connect"))
    }

    async fn run(conn: &dyn Connection, sql: &str) {
        let mut stream = conn
            .execute(sql, &ExecuteOptions::default())
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
        while let Some(batch) = stream.batches.next().await {
            batch.expect("batch");
        }
    }

    #[tokio::test]
    async fn alters_and_rolls_back() {
        let Some(conn) = connect().await else { return };
        run(
            &*conn,
            "DROP SCHEMA IF EXISTS deltos_test_ddl CASCADE; CREATE SCHEMA deltos_test_ddl; \
             CREATE TABLE deltos_test_ddl.users (id int PRIMARY KEY); \
             CREATE TABLE deltos_test_ddl.orders (id serial PRIMARY KEY, user_id int, status text NOT NULL DEFAULT 'open', note text UNIQUE); \
             CREATE INDEX orders_status_idx ON deltos_test_ddl.orders (status); \
             INSERT INTO deltos_test_ddl.users VALUES (1); \
             INSERT INTO deltos_test_ddl.orders (user_id, status) VALUES (1, '12')",
        )
        .await;
        let namespaces = conn.namespaces().await.expect("namespaces");
        let db = namespaces[0].path[0].clone();
        let ns = [db.as_str(), "deltos_test_ddl"];
        let object = object(&ns, "orders");
        let info = conn.describe(&object).await.expect("describe");
        assert!(
            info.indexes
                .iter()
                .any(|i| i.name == "orders_note_key" && i.constraint)
        );
        let table = TableDef::from_info(Flavor::Postgres, &object, &info);
        let mut draft = Draft::of(&table);
        let status = find(&draft, "status");
        draft.columns[status].def.name = "state".into();
        draft.columns[status].def.type_name = "integer".into();
        draft.columns[status].def.default = Some("0".into());
        draft.columns[status].def.comment = Some("a number".into());
        draft
            .indexes
            .retain(|i| i.def.name != "orders_note_key" && i.def.name != "orders_status_idx");
        draft.foreign_keys.push(Staged {
            from: None,
            def: ForeignKeyDef {
                name: "orders_user_id_fkey".into(),
                columns: strings(&["user_id"]),
                target: super::object(&ns, "users"),
                target_columns: strings(&["id"]),
                on_delete: ForeignKeyAction::SetNull,
                on_update: ForeignKeyAction::Cascade,
            },
        });
        let plan = super::plan(Flavor::Postgres, &table, &draft).expect("plan");
        let namespace: Vec<String> = ns.iter().map(|s| s.to_string()).collect();
        conn.apply_schema(&namespace, &plan.statements)
            .await
            .expect("apply");
        let after = conn.describe(&object).await.expect("describe");
        let state = after
            .columns
            .iter()
            .find(|c| c.name == "state")
            .expect("state");
        assert_eq!(state.type_name, "integer");
        assert_eq!(state.default.as_deref(), Some("0"));
        assert_eq!(state.comment.as_deref(), Some("a number"));
        assert_eq!(after.indexes.len(), 1);
        assert_eq!(after.foreign_keys[0].on_delete, ForeignKeyAction::SetNull);
        assert_eq!(after.foreign_keys[0].on_update, ForeignKeyAction::Cascade);

        // A failing statement undoes the ones before it.
        let table = TableDef::from_info(Flavor::Postgres, &object, &after);
        let mut draft = Draft::of(&table);
        let note = find(&draft, "note");
        draft.columns[note].def.name = "remark".into();
        draft.columns[note].def.type_name = "integer".into();
        run(
            &*conn,
            "UPDATE deltos_test_ddl.orders SET note = 'not a number'",
        )
        .await;
        let plan = super::plan(Flavor::Postgres, &table, &draft).expect("plan");
        let error = conn
            .apply_schema(&namespace, &plan.statements)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("In: ALTER TABLE"), "{error}");
        let again = conn.describe(&object).await.expect("describe");
        assert!(again.columns.iter().any(|c| c.name == "note"));
        // The session is usable after the rollback.
        run(&*conn, "SELECT 1").await;
        run(&*conn, "DROP SCHEMA deltos_test_ddl CASCADE").await;
    }
}
