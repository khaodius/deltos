//! Query plans: what each database answers to its `EXPLAIN`, read into one tree of
//! nodes with costs, row counts and times where the database gives them.
//!
//! - PostgreSQL: `EXPLAIN (FORMAT JSON [, ANALYZE])`, one row with a JSON array.
//! - MySQL and MariaDB: `EXPLAIN FORMAT=JSON`, one row with a `query_block` document.
//! - SQLite: `EXPLAIN QUERY PLAN`, rows of `id`, `parent`, `notused`, `detail`.
//! - MongoDB: `explain()`, one document with `queryPlanner` and, when the plan ran,
//!   `executionStats`.
//!
//! Nothing here panics on odd input: fields that are missing or of another type are left
//! out, and only a result that is not a plan at all is an error.

use deltos_data::Value;
use serde_json::{Map, Value as Json};

use super::Dialect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanSource {
    Postgres,
    Mysql,
    Sqlite,
    Mongo,
}

/// Deeper plans than this are cut, rather than risking the stack.
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlanNode {
    /// What the node does: `Seq Scan on orders`, `IXSCAN { status: 1 }`.
    pub title: String,
    /// Conditions, keys and other facts, one per line.
    pub details: Vec<String>,
    /// The planner's cost of this node and everything under it.
    pub cost: Option<f64>,
    /// This node's own share of the cost.
    pub self_cost: Option<f64>,
    /// Rows the planner expects.
    pub estimated_rows: Option<f64>,
    /// Rows the node produced when the plan ran.
    pub actual_rows: Option<f64>,
    /// Milliseconds in this node and everything under it, when the plan ran.
    pub time_ms: Option<f64>,
    pub self_time_ms: Option<f64>,
    /// A step worth a look even without numbers, such as a full scan.
    pub warning: bool,
    pub children: Vec<PlanNode>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Plan {
    pub roots: Vec<PlanNode>,
    pub planning_ms: Option<f64>,
    pub execution_ms: Option<f64>,
    /// The cost of the whole statement, when the database states it apart from the nodes.
    pub total_cost: Option<f64>,
    /// Whether the statement ran to measure the plan.
    pub analyzed: bool,
}

/// One line of the plan as shown: a node at a depth.
#[derive(Debug, Clone, Copy)]
pub struct Line<'a> {
    pub depth: usize,
    pub node: &'a PlanNode,
}

impl Plan {
    /// Every node, depth first, as the tree is drawn.
    pub fn lines(&self) -> Vec<Line<'_>> {
        fn walk<'a>(node: &'a PlanNode, depth: usize, out: &mut Vec<Line<'a>>) {
            out.push(Line { depth, node });
            for child in &node.children {
                walk(child, depth + 1, out);
            }
        }
        let mut out = Vec::new();
        for root in &self.roots {
            walk(root, 0, &mut out);
        }
        out
    }

    /// The index in [`Plan::lines`] of the node to point at: the one with the most time
    /// of its own, else the most cost of its own, else the first warning.
    pub fn hottest(&self) -> Option<usize> {
        let lines = self.lines();
        let best = |f: fn(&PlanNode) -> Option<f64>| {
            lines
                .iter()
                .enumerate()
                .filter_map(|(i, l)| f(l.node).filter(|v| *v > 0.0).map(|v| (i, v)))
                .max_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i)
        };
        best(|n| n.self_time_ms)
            .or_else(|| best(|n| n.self_cost))
            .or_else(|| lines.iter().position(|l| l.node.warning))
    }

    /// The largest value of `f` over the nodes, to scale bars by.
    pub fn max_of(&self, f: impl Fn(&PlanNode) -> Option<f64>) -> f64 {
        self.lines()
            .iter()
            .filter_map(|l| f(l.node))
            .fold(0.0, f64::max)
    }
}

/// What starts the server's notice that carries a measured PostgreSQL plan.
const PLAN_NOTICE: &str = "deltos-plan:";

/// The statement that asks for the plan of `statement`; with `analyze`, one that runs it
/// to measure the plan. `statement` is one statement, which the caller checks.
///
/// A measured PostgreSQL plan runs the statement in a block of its own that is then
/// rolled back, inside the session's transaction if it has one, so nothing it changes
/// stays; the plan comes back as a notice ([`measured_plan`] reads it). A MongoDB
/// statement fails to read rather than lose its `.explain()` to an open comment.
pub fn explain_statement(
    source: PlanSource,
    statement: &str,
    analyze: bool,
) -> Result<String, String> {
    let statement = statement.trim().trim_end_matches(';').trim_end();
    Ok(match source {
        PlanSource::Postgres if analyze => {
            let explain = format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {statement}");
            let outer = dollar_tag(statement, "deltos_block");
            let inner = dollar_tag(statement, "deltos_sql");
            // The block raises its own error to roll back what the statement did, then
            // reports the plan it kept. INFO reaches the client whatever its
            // `client_min_messages`.
            format!(
                "DO {outer}\nDECLARE\n    plan text;\nBEGIN\n    BEGIN\n        \
                 EXECUTE {inner}{explain}\n{inner} INTO plan;\n        \
                 RAISE EXCEPTION USING ERRCODE = 'DLT01';\n    \
                 EXCEPTION WHEN SQLSTATE 'DLT01' THEN\n        NULL;\n    END;\n    \
                 RAISE INFO '{PLAN_NOTICE}%', plan;\nEND\n{outer}"
            )
        }
        PlanSource::Postgres => format!("EXPLAIN (FORMAT JSON) {statement}"),
        PlanSource::Mysql => format!("EXPLAIN FORMAT=JSON {statement}"),
        PlanSource::Sqlite => format!("EXPLAIN QUERY PLAN {statement}"),
        PlanSource::Mongo => {
            if ends_in_open_comment(statement) {
                return Err("The statement ends inside a /* comment.".into());
            }
            let verbosity = if analyze {
                "executionStats"
            } else {
                "queryPlanner"
            };
            // On a line of its own, so a `//` comment at the end cannot hide it.
            format!("{statement}\n.explain(\"{verbosity}\")")
        }
    })
}

/// A measured PostgreSQL plan asked for with a plain `EXPLAIN ANALYZE`, which returns the
/// plan as a row and keeps what the statement changes. For a read-only connection, which
/// refuses the block [`explain_statement`] wraps it in but knows `EXPLAIN ANALYZE` of a
/// read to be a read.
pub fn measured_in_place(statement: &str) -> String {
    let statement = statement.trim().trim_end_matches(';').trim_end();
    format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) {statement}")
}

/// A dollar-quote tag that `text` does not hold.
fn dollar_tag(text: &str, name: &str) -> String {
    let mut n = 0u32;
    loop {
        let tag = if n == 0 {
            format!("${name}$")
        } else {
            format!("${name}{n}$")
        };
        if !text.contains(&tag) {
            return tag;
        }
        n += 1;
    }
}

/// Whether a MongoDB shell statement ends inside a block comment, which would swallow
/// anything added after it.
fn ends_in_open_comment(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            q @ (b'\'' | b'"' | b'`') => {
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => match text[i + 2..].find("*/") {
                Some(p) => i += 2 + p + 1,
                None => return true,
            },
            _ => {}
        }
        i += 1;
    }
    false
}

/// The `EXPLAIN` that a measured PostgreSQL [`explain_statement`] wraps, for a stand-in
/// server such as the demo to answer.
pub fn measured_explain(sql: &str) -> Option<&str> {
    let rest = sql.strip_prefix("DO $deltos_block")?;
    let rest = &rest[rest.find("EXECUTE $deltos_sql")? + "EXECUTE ".len()..];
    let tag_end = rest.get(1..)?.find('$')? + 2;
    let (tag, body) = rest.split_at(tag_end);
    let end = body.find(&format!("\n{tag} INTO plan"))?;
    Some(&body[..end])
}

/// The notice a measured PostgreSQL plan comes back in, as the server sends it.
pub fn plan_notice(json: &str) -> String {
    format!("INFO: {PLAN_NOTICE}{json}")
}

/// The plan a measured PostgreSQL `explain_statement` reported, as the one cell an
/// `EXPLAIN` would have returned.
pub fn measured_plan(messages: &[String]) -> Option<Vec<Vec<Value>>> {
    messages.iter().find_map(|m| {
        let at = m.find(PLAN_NOTICE)?;
        let json = &m[at + PLAN_NOTICE.len()..];
        Some(vec![vec![Value::text(json)]])
    })
}

impl PlanSource {
    pub fn for_dialect(dialect: Option<Dialect>) -> Self {
        match dialect {
            Some(Dialect::Postgres) => PlanSource::Postgres,
            Some(Dialect::Mysql) => PlanSource::Mysql,
            Some(Dialect::Sqlite) => PlanSource::Sqlite,
            None => PlanSource::Mongo,
        }
    }

    /// Whether the plan can be measured by running the statement.
    pub fn can_analyze(self) -> bool {
        matches!(self, PlanSource::Postgres | PlanSource::Mongo)
    }
}

/// Reads the rows an `EXPLAIN` returned.
pub fn parse(source: PlanSource, columns: &[String], rows: &[Vec<Value>]) -> Result<Plan, String> {
    match source {
        PlanSource::Postgres => postgres(&json_cell(rows)?),
        PlanSource::Mysql => mysql(&json_cell(rows)?),
        PlanSource::Sqlite => Ok(sqlite(columns, rows)),
        PlanSource::Mongo => mongo(&document(columns, rows)?),
    }
}

/// The JSON in the first cell of the first row.
fn json_cell(rows: &[Vec<Value>]) -> Result<Json, String> {
    let cell = rows
        .first()
        .and_then(|r| r.first())
        .ok_or("The database returned no plan.")?;
    let text = match cell {
        Value::Text(s) | Value::Json(s) => s,
        _ => return Err("The plan is not JSON.".into()),
    };
    serde_json::from_str(text).map_err(|e| format!("Could not read the plan: {e}"))
}

/// The first row as one JSON object, field by field, as the document drivers return it.
fn document(columns: &[String], rows: &[Vec<Value>]) -> Result<Json, String> {
    let row = rows.first().ok_or("The database returned no plan.")?;
    if columns.len() == 1
        && let Some(Value::Json(s) | Value::Text(s)) = row.first()
        && let Ok(Json::Object(doc)) = serde_json::from_str::<Json>(s)
    {
        // A whole document in one column.
        if doc.contains_key("queryPlanner") || doc.contains_key("stages") {
            return Ok(Json::Object(doc));
        }
    }
    let mut map = Map::new();
    for (name, value) in columns.iter().zip(row) {
        let json = match value {
            Value::Null => Json::Null,
            Value::Bool(b) => Json::Bool(*b),
            Value::Int(n) => Json::from(*n),
            Value::Float(f) => serde_json::Number::from_f64(*f).map_or(Json::Null, Json::Number),
            Value::Text(s) => Json::String(s.to_string()),
            Value::Json(s) => {
                serde_json::from_str(s).unwrap_or_else(|_| Json::String(s.to_string()))
            }
            Value::Bytes(_) => Json::Null,
        };
        map.insert(name.clone(), json);
    }
    Ok(Json::Object(map))
}

fn num(v: Option<&Json>) -> Option<f64> {
    match v? {
        Json::Number(n) => n.as_f64(),
        Json::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn text(v: Option<&Json>) -> Option<String> {
    match v? {
        Json::String(s) => Some(s.clone()),
        Json::Number(n) => Some(n.to_string()),
        Json::Bool(b) => Some(b.to_string()),
        Json::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(|i| text(Some(i))).collect();
            (!parts.is_empty()).then(|| parts.join(", "))
        }
        Json::Object(_) => Some(compact(v?)),
        Json::Null => None,
    }
}

/// JSON on one line, cut to a readable length.
fn compact(v: &Json) -> String {
    let mut s = v.to_string();
    if s.len() > 300 {
        let mut cut = 300;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push('…');
    }
    s
}

// ---- PostgreSQL --------------------------------------------------------------------------

fn postgres(json: &Json) -> Result<Plan, String> {
    let top = match json {
        Json::Array(items) => items.first(),
        Json::Object(_) => Some(json),
        _ => None,
    }
    .and_then(Json::as_object)
    .ok_or("The plan is not in PostgreSQL's JSON form.")?;
    let root = top
        .get("Plan")
        .and_then(Json::as_object)
        .ok_or("The plan has no nodes.")?;
    let node = pg_node(root, 0);
    Ok(Plan {
        analyzed: node.time_ms.is_some(),
        planning_ms: num(top.get("Planning Time")),
        execution_ms: num(top.get("Execution Time")),
        total_cost: node.cost,
        roots: vec![node],
    })
}

fn pg_node(n: &Map<String, Json>, depth: usize) -> PlanNode {
    let kind = text(n.get("Node Type")).unwrap_or_else(|| "Node".into());
    let mut title = kind.clone();
    if let Some(strategy) = text(n.get("Strategy")).filter(|s| s != "Plain" && kind == "Aggregate")
    {
        title = format!("{strategy} {title}");
    }
    if let Some(join) = text(n.get("Join Type")).filter(|j| j != "Inner") {
        title = format!("{title} ({join})");
    }
    if let Some(index) = text(n.get("Index Name")) {
        title.push_str(&format!(" using {index}"));
    }
    if let Some(relation) = text(n.get("Relation Name"))
        .or_else(|| text(n.get("CTE Name")))
        .or_else(|| text(n.get("Function Name")))
    {
        title.push_str(&format!(" on {relation}"));
        if let Some(alias) = text(n.get("Alias")).filter(|a| *a != relation) {
            title.push_str(&format!(" {alias}"));
        }
    }
    let mut details = Vec::new();
    if let Some(sub) = text(n.get("Subplan Name")) {
        details.push(sub);
    }
    for key in [
        "Index Cond",
        "Recheck Cond",
        "Hash Cond",
        "Merge Cond",
        "Join Filter",
        "Filter",
        "Sort Key",
        "Group Key",
        "Presorted Key",
        "Sort Method",
        "Rows Removed by Filter",
        "Rows Removed by Join Filter",
        "Heap Fetches",
        "Workers Planned",
        "Workers Launched",
    ] {
        if let Some(v) = text(n.get(key)) {
            details.push(format!("{key}: {v}"));
        }
    }
    let loops = num(n.get("Actual Loops")).unwrap_or(1.0).max(1.0);
    let children: Vec<PlanNode> = if depth < MAX_DEPTH {
        n.get("Plans")
            .and_then(Json::as_array)
            .map(|plans| {
                plans
                    .iter()
                    .filter_map(Json::as_object)
                    .map(|c| pg_node(c, depth + 1))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let cost = num(n.get("Total Cost"));
    let time_ms = num(n.get("Actual Total Time")).map(|t| t * loops);
    let actual_rows = num(n.get("Actual Rows")).map(|r| r * loops);
    let child_cost: f64 = children.iter().filter_map(|c| c.cost).sum();
    let child_time: f64 = children.iter().filter_map(|c| c.time_ms).sum();
    PlanNode {
        warning: kind == "Seq Scan",
        title,
        details,
        self_cost: cost.map(|c| (c - child_cost).max(0.0)),
        cost,
        estimated_rows: num(n.get("Plan Rows")),
        actual_rows,
        self_time_ms: time_ms.map(|t| (t - child_time).max(0.0)),
        time_ms,
        children,
    }
}

// ---- MySQL and MariaDB -------------------------------------------------------------------

fn mysql(json: &Json) -> Result<Plan, String> {
    let top = json
        .as_object()
        .ok_or("The plan is not in MySQL's JSON form.")?;
    let block = top
        .get("query_block")
        .and_then(Json::as_object)
        .ok_or("The plan has no query block.")?;
    let node = mysql_block(block, 0);
    Ok(Plan {
        total_cost: node.cost,
        roots: vec![node],
        ..Plan::default()
    })
}

fn mysql_block(b: &Map<String, Json>, depth: usize) -> PlanNode {
    let id = text(b.get("select_id"));
    let mut node = PlanNode {
        title: match id {
            Some(id) => format!("Query block #{id}"),
            None => "Query block".into(),
        },
        cost: num(b.get("cost_info").and_then(|c| c.get("query_cost")))
            .or_else(|| num(b.get("cost"))),
        ..PlanNode::default()
    };
    if let Some(message) = text(b.get("message")) {
        node.details.push(message);
    }
    node.children = mysql_children(b, depth);
    node
}

/// The steps inside a block or operation, in the order MySQL lists them.
fn mysql_children(o: &Map<String, Json>, depth: usize) -> Vec<PlanNode> {
    if depth >= MAX_DEPTH {
        return Vec::new();
    }
    // Keys in a fixed order, since JSON objects do not keep theirs.
    const KEYS: &[&str] = &[
        "table",
        "nested_loop",
        "ordering_operation",
        "grouping_operation",
        "duplicates_removal",
        "windowing",
        "buffer_result",
        "union_result",
        "materialized_from_subquery",
        "attached_subqueries",
        "optimized_away_subqueries",
        "order_by_subqueries",
        "group_by_subqueries",
        "having_subqueries",
        "select_list_subqueries",
        "subqueries",
    ];
    let mut out = Vec::new();
    for (key, value) in KEYS.iter().filter_map(|k| o.get(*k).map(|v| (*k, v))) {
        match (key, value) {
            ("table", Json::Object(t)) => out.push(mysql_table(t, depth + 1)),
            ("nested_loop", Json::Array(items)) => {
                let mut join = PlanNode {
                    title: "Nested loop".into(),
                    ..PlanNode::default()
                };
                for item in items.iter().filter_map(Json::as_object) {
                    join.children.extend(mysql_children(item, depth + 1));
                }
                join.cost = join.children.iter().filter_map(|c| c.cost).reduce(f64::max);
                out.push(join);
            }
            (
                "ordering_operation" | "grouping_operation" | "duplicates_removal" | "windowing"
                | "buffer_result",
                Json::Object(inner),
            ) => {
                let title = match key {
                    "ordering_operation" => "Sort",
                    "grouping_operation" => "Group",
                    "duplicates_removal" => "Distinct",
                    "windowing" => "Window",
                    _ => "Buffer",
                };
                let mut node = PlanNode {
                    title: title.into(),
                    ..PlanNode::default()
                };
                if inner.get("using_filesort").and_then(Json::as_bool) == Some(true) {
                    node.details.push("Using filesort".into());
                    node.warning = true;
                }
                if inner.get("using_temporary_table").and_then(Json::as_bool) == Some(true) {
                    node.details.push("Using a temporary table".into());
                    node.warning = true;
                }
                node.children = mysql_children(inner, depth + 1);
                node.cost = node.children.iter().filter_map(|c| c.cost).reduce(f64::max);
                out.push(node);
            }
            ("union_result", Json::Object(u)) => {
                let mut node = PlanNode {
                    title: "Union".into(),
                    ..PlanNode::default()
                };
                for spec in u
                    .get("query_specifications")
                    .and_then(Json::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Json::as_object)
                {
                    if let Some(Json::Object(block)) = spec.get("query_block") {
                        node.children.push(mysql_block(block, depth + 1));
                    }
                }
                out.push(node);
            }
            ("materialized_from_subquery", Json::Object(m)) => {
                if let Some(Json::Object(block)) = m.get("query_block") {
                    let mut node = PlanNode {
                        title: "Materialize".into(),
                        children: vec![mysql_block(block, depth + 1)],
                        ..PlanNode::default()
                    };
                    node.cost = node.children[0].cost;
                    out.push(node);
                }
            }
            (
                "attached_subqueries"
                | "optimized_away_subqueries"
                | "order_by_subqueries"
                | "group_by_subqueries"
                | "having_subqueries"
                | "select_list_subqueries"
                | "subqueries",
                Json::Array(items),
            ) => {
                for item in items.iter().filter_map(Json::as_object) {
                    if let Some(Json::Object(block)) = item.get("query_block") {
                        let mut sub = mysql_block(block, depth + 1);
                        if item.get("dependent").and_then(Json::as_bool) == Some(true) {
                            sub.details
                                .push("Dependent: runs once per outer row".into());
                            sub.warning = true;
                        }
                        out.push(sub);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn mysql_table(t: &Map<String, Json>, depth: usize) -> PlanNode {
    let name = text(t.get("table_name")).unwrap_or_else(|| "?".into());
    let access = text(t.get("access_type")).unwrap_or_default();
    let what = match access.as_str() {
        "ALL" => "Full table scan",
        "index" => "Full index scan",
        "range" => "Index range scan",
        "ref" | "ref_or_null" => "Index lookup",
        "eq_ref" => "Unique index lookup",
        "const" | "system" => "Constant row",
        "fulltext" => "Full-text search",
        "index_merge" => "Index merge",
        "" => "Table",
        _ => "Access",
    };
    let mut title = format!("{what} on {name}");
    if let Some(key) = text(t.get("key")) {
        title.push_str(&format!(" using {key}"));
    }
    let mut details = Vec::new();
    if !access.is_empty() {
        details.push(format!("Access type: {access}"));
    }
    for (key, label) in [
        ("possible_keys", "Possible keys"),
        ("used_key_parts", "Key parts"),
        ("ref", "Ref"),
        ("attached_condition", "Condition"),
        ("index_condition", "Index condition"),
        ("filtered", "Filtered %"),
    ] {
        if let Some(v) = text(t.get(key)) {
            details.push(format!("{label}: {v}"));
        }
    }
    let costs = t.get("cost_info");
    let read = num(costs.and_then(|c| c.get("read_cost")));
    let eval = num(costs.and_then(|c| c.get("eval_cost")));
    let own = match (read, eval) {
        (None, None) => num(t.get("cost")),
        (r, e) => Some(r.unwrap_or(0.0) + e.unwrap_or(0.0)),
    };
    let mut node = PlanNode {
        title,
        details,
        cost: num(costs.and_then(|c| c.get("prefix_cost"))).or(own),
        self_cost: own,
        estimated_rows: num(t.get("rows_examined_per_scan")).or_else(|| num(t.get("rows"))),
        actual_rows: num(t.get("r_rows")),
        time_ms: num(t.get("r_table_time_ms")).or_else(|| num(t.get("r_total_time_ms"))),
        warning: access == "ALL",
        children: Vec::new(),
        self_time_ms: None,
    };
    node.self_time_ms = node.time_ms;
    // A table can carry the sub-queries that feed it.
    let mut rest = t.clone();
    rest.remove("table");
    node.children = mysql_children(&rest, depth);
    node
}

// ---- SQLite ------------------------------------------------------------------------------

fn sqlite(columns: &[String], rows: &[Vec<Value>]) -> Plan {
    let col = |name: &str, fallback: usize| {
        columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(name))
            .unwrap_or(fallback)
    };
    let (id_col, parent_col, detail_col) = (col("id", 0), col("parent", 1), col("detail", 3));
    let int = |v: Option<&Value>| match v {
        Some(Value::Int(n)) => Some(*n),
        Some(Value::Text(s)) => s.parse().ok(),
        _ => None,
    };
    // (id, parent, node) in row order; a parent always comes before its children.
    let mut flat: Vec<(i64, i64, PlanNode)> = Vec::new();
    for row in rows {
        let detail = match row.get(detail_col) {
            Some(Value::Text(s)) => s.to_string(),
            Some(other) => other.preview(200),
            None => continue,
        };
        let upper = detail.to_ascii_uppercase();
        let scan = (upper.starts_with("SCAN ") && !upper.contains("COVERING INDEX"))
            || upper.contains("TEMP B-TREE");
        flat.push((
            int(row.get(id_col)).unwrap_or(0),
            int(row.get(parent_col)).unwrap_or(0),
            PlanNode {
                title: detail,
                warning: scan,
                ..PlanNode::default()
            },
        ));
    }
    // Build the tree from the leaves up.
    let mut roots = Vec::new();
    while let Some((id, parent, node)) = flat.pop() {
        match flat
            .iter_mut()
            .rev()
            .find(|(pid, _, _)| *pid == parent && *pid != id)
        {
            Some((_, _, p)) if parent != 0 => p.children.insert(0, node),
            _ => roots.insert(0, node),
        }
    }
    Plan {
        roots,
        ..Plan::default()
    }
}

// ---- MongoDB -----------------------------------------------------------------------------

fn mongo(doc: &Json) -> Result<Plan, String> {
    let top = doc.as_object().ok_or("The plan is not a document.")?;
    let stats = top.get("executionStats").and_then(Json::as_object);
    let mut plan = Plan {
        execution_ms: stats.and_then(|s| num(s.get("executionTimeMillis"))),
        analyzed: stats.is_some(),
        ..Plan::default()
    };
    if let Some(stages) = top.get("stages").and_then(Json::as_array) {
        // An aggregation: the first stage holds the query plan.
        let mut pipeline = PlanNode {
            title: "Aggregation pipeline".into(),
            ..PlanNode::default()
        };
        for stage in stages.iter().filter_map(Json::as_object) {
            let Some((name, body)) = stage.iter().find(|(k, _)| k.starts_with('$')) else {
                continue;
            };
            if name == "$cursor"
                && let Some(inner) = body.as_object()
            {
                let mut cursor = mongo_query(inner)?;
                if plan.execution_ms.is_none() {
                    plan.execution_ms = inner
                        .get("executionStats")
                        .and_then(|s| num(s.get("executionTimeMillis")));
                }
                plan.analyzed |= inner.contains_key("executionStats");
                pipeline.children.append(&mut cursor.roots);
                continue;
            }
            pipeline.children.push(PlanNode {
                title: name.clone(),
                details: vec![compact(body)],
                actual_rows: num(stage.get("nReturned")),
                time_ms: num(stage.get("executionTimeMillisEstimate")),
                ..PlanNode::default()
            });
        }
        mongo_self_times(&mut pipeline);
        plan.roots.push(pipeline);
        return Ok(plan);
    }
    let query = mongo_query(top)?;
    plan.roots = query.roots;
    Ok(plan)
}

/// The plan of one query, from `executionStats.executionStages` when it ran (it has the
/// numbers), else `queryPlanner.winningPlan`.
fn mongo_query(top: &Map<String, Json>) -> Result<Plan, String> {
    let executed = top
        .get("executionStats")
        .and_then(|s| s.get("executionStages"))
        .and_then(Json::as_object);
    let winning = top
        .get("queryPlanner")
        .and_then(|q| q.get("winningPlan"))
        .and_then(Json::as_object);
    // Newer servers nest the classic plan under `queryPlan`.
    let winning = winning.map(|w| w.get("queryPlan").and_then(Json::as_object).unwrap_or(w));
    let root = executed
        .or(winning)
        .ok_or("The answer has no query plan (queryPlanner.winningPlan).")?;
    let mut node = mongo_stage(root, 0);
    mongo_self_times(&mut node);
    Ok(Plan {
        roots: vec![node],
        ..Plan::default()
    })
}

fn mongo_stage(s: &Map<String, Json>, depth: usize) -> PlanNode {
    let stage = text(s.get("stage")).unwrap_or_else(|| "STAGE".into());
    let mut title = stage.clone();
    if let Some(index) = text(s.get("indexName")) {
        title.push_str(&format!(" using {index}"));
    }
    let mut details = Vec::new();
    for (key, label) in [
        ("filter", "Filter"),
        ("keyPattern", "Key pattern"),
        ("indexBounds", "Bounds"),
        ("direction", "Direction"),
        ("sortPattern", "Sort"),
        ("transformBy", "Projection"),
        ("limitAmount", "Limit"),
        ("skipAmount", "Skip"),
        ("docsExamined", "Documents examined"),
        ("keysExamined", "Keys examined"),
    ] {
        if let Some(v) = text(s.get(key)) {
            details.push(format!("{label}: {v}"));
        }
    }
    let mut children = Vec::new();
    if depth < MAX_DEPTH {
        if let Some(Json::Object(input)) = s.get("inputStage") {
            children.push(mongo_stage(input, depth + 1));
        }
        for key in ["inputStages", "thenStages", "elseStages"] {
            if let Some(Json::Array(items)) = s.get(key) {
                children.extend(
                    items
                        .iter()
                        .filter_map(Json::as_object)
                        .map(|c| mongo_stage(c, depth + 1)),
                );
            }
        }
        if let Some(Json::Object(shards)) = s.get("shards") {
            for shard in shards.values().filter_map(Json::as_object) {
                children.push(mongo_stage(shard, depth + 1));
            }
        }
    }
    PlanNode {
        warning: stage == "COLLSCAN" || stage == "SORT",
        title,
        details,
        actual_rows: num(s.get("nReturned")),
        time_ms: num(s.get("executionTimeMillisEstimate")),
        children,
        ..PlanNode::default()
    }
}

fn mongo_self_times(node: &mut PlanNode) {
    for child in &mut node.children {
        mongo_self_times(child);
    }
    let below: f64 = node.children.iter().filter_map(|c| c.time_ms).sum();
    node.self_time_ms = node.time_ms.map(|t| (t - below).max(0.0));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(json: &str) -> Vec<Vec<Value>> {
        vec![vec![Value::Json(json.into())]]
    }

    /// `EXPLAIN (ANALYZE, FORMAT JSON)` of a join on PostgreSQL 16.
    const PG_ANALYZE: &str = r#"[
      {
        "Plan": {
          "Node Type": "Hash Join",
          "Parallel Aware": false,
          "Join Type": "Inner",
          "Startup Cost": 1.09,
          "Total Cost": 26.37,
          "Plan Rows": 350,
          "Plan Width": 44,
          "Actual Startup Time": 0.041,
          "Actual Total Time": 0.162,
          "Actual Rows": 350,
          "Actual Loops": 1,
          "Hash Cond": "(o.customer_id = c.id)",
          "Plans": [
            {
              "Node Type": "Seq Scan",
              "Parent Relationship": "Outer",
              "Relation Name": "orders",
              "Alias": "o",
              "Startup Cost": 0.00,
              "Total Cost": 22.70,
              "Plan Rows": 1270,
              "Plan Width": 20,
              "Actual Startup Time": 0.006,
              "Actual Total Time": 0.081,
              "Actual Rows": 1270,
              "Actual Loops": 1,
              "Filter": "(total > '10'::numeric)",
              "Rows Removed by Filter": 30
            },
            {
              "Node Type": "Hash",
              "Parent Relationship": "Inner",
              "Startup Cost": 1.04,
              "Total Cost": 1.04,
              "Plan Rows": 4,
              "Plan Width": 36,
              "Actual Startup Time": 0.012,
              "Actual Total Time": 0.013,
              "Actual Rows": 4,
              "Actual Loops": 1,
              "Plans": [
                {
                  "Node Type": "Index Scan",
                  "Parent Relationship": "Outer",
                  "Index Name": "customers_pkey",
                  "Relation Name": "customers",
                  "Alias": "c",
                  "Startup Cost": 0.15,
                  "Total Cost": 1.04,
                  "Plan Rows": 4,
                  "Actual Total Time": 0.009,
                  "Actual Rows": 1,
                  "Actual Loops": 4,
                  "Index Cond": "(id = ANY ('{1,2}'::integer[]))"
                }
              ]
            }
          ]
        },
        "Planning Time": 0.215,
        "Triggers": [],
        "Execution Time": 0.201
      }
    ]"#;

    #[test]
    fn reads_postgres_plans() {
        let plan = parse(
            PlanSource::Postgres,
            &["QUERY PLAN".into()],
            &cell(PG_ANALYZE),
        )
        .unwrap();
        assert!(plan.analyzed);
        assert_eq!(plan.planning_ms, Some(0.215));
        assert_eq!(plan.execution_ms, Some(0.201));
        let lines = plan.lines();
        let titles: Vec<(usize, &str)> = lines
            .iter()
            .map(|l| (l.depth, l.node.title.as_str()))
            .collect();
        assert_eq!(
            titles,
            vec![
                (0, "Hash Join"),
                (1, "Seq Scan on orders o"),
                (1, "Hash"),
                (2, "Index Scan using customers_pkey on customers c"),
            ]
        );
        let join = lines[0].node;
        assert_eq!(join.details, vec!["Hash Cond: (o.customer_id = c.id)"]);
        assert!((join.self_cost.unwrap() - (26.37 - 22.70 - 1.04)).abs() < 1e-9);
        // Loops multiply the per-loop numbers.
        let index = lines[3].node;
        assert_eq!(index.actual_rows, Some(4.0));
        assert!((index.time_ms.unwrap() - 0.036).abs() < 1e-9);
        // The scan of orders has the most time of its own.
        assert_eq!(plan.hottest(), Some(1));
        assert!(lines[1].node.warning);
        assert!(
            lines[1]
                .node
                .details
                .contains(&"Rows Removed by Filter: 30".to_string())
        );
    }

    #[test]
    fn postgres_plain_explain_uses_costs() {
        let json = r#"[{"Plan": {"Node Type": "Limit", "Total Cost": 0.29, "Plan Rows": 10,
            "Plans": [{"Node Type": "Seq Scan", "Relation Name": "orders", "Alias": "orders",
            "Total Cost": 18.5, "Plan Rows": 850}]}}]"#;
        let plan = parse(PlanSource::Postgres, &[], &cell(json)).unwrap();
        assert!(!plan.analyzed);
        let lines = plan.lines();
        assert_eq!(lines[1].node.title, "Seq Scan on orders");
        // A limit costs less than its input; its own share is not negative.
        assert_eq!(lines[0].node.self_cost, Some(0.0));
        assert_eq!(plan.hottest(), Some(1));
        assert_eq!(plan.max_of(|n| n.estimated_rows), 850.0);
    }

    /// `EXPLAIN FORMAT=JSON` of a join with ORDER BY on MySQL 8.4.
    const MYSQL: &str = r#"{
      "query_block": {
        "select_id": 1,
        "cost_info": { "query_cost": "1.70" },
        "ordering_operation": {
          "using_temporary_table": true,
          "using_filesort": true,
          "nested_loop": [
            {
              "table": {
                "table_name": "c",
                "access_type": "ALL",
                "possible_keys": ["PRIMARY"],
                "rows_examined_per_scan": 3,
                "rows_produced_per_join": 3,
                "filtered": "100.00",
                "cost_info": { "read_cost": "0.25", "eval_cost": "0.30", "prefix_cost": "0.55", "data_read_per_join": "96" },
                "used_columns": ["id", "name"]
              }
            },
            {
              "table": {
                "table_name": "o",
                "access_type": "ref",
                "possible_keys": ["customer_id"],
                "key": "customer_id",
                "used_key_parts": ["customer_id"],
                "key_length": "5",
                "ref": ["shop.c.id"],
                "rows_examined_per_scan": 1,
                "rows_produced_per_join": 3,
                "filtered": "100.00",
                "cost_info": { "read_cost": "0.75", "eval_cost": "0.30", "prefix_cost": "1.60", "data_read_per_join": "72" },
                "used_columns": ["id", "customer_id", "total"],
                "attached_condition": "(`shop`.`o`.`total` > 10)"
              }
            }
          ]
        }
      }
    }"#;

    #[test]
    fn reads_mysql_plans() {
        let plan = parse(PlanSource::Mysql, &["EXPLAIN".into()], &cell(MYSQL)).unwrap();
        assert_eq!(plan.total_cost, Some(1.70));
        let lines = plan.lines();
        let titles: Vec<(usize, &str)> = lines
            .iter()
            .map(|l| (l.depth, l.node.title.as_str()))
            .collect();
        assert_eq!(
            titles,
            vec![
                (0, "Query block #1"),
                (1, "Sort"),
                (2, "Nested loop"),
                (3, "Full table scan on c"),
                (3, "Index lookup on o using customer_id"),
            ]
        );
        assert!(
            lines[1]
                .node
                .details
                .contains(&"Using filesort".to_string())
        );
        let o = lines[4].node;
        assert_eq!(o.self_cost, Some(1.05));
        assert_eq!(o.cost, Some(1.60));
        assert_eq!(o.estimated_rows, Some(1.0));
        assert!(
            o.details
                .contains(&"Condition: (`shop`.`o`.`total` > 10)".to_string())
        );
        assert_eq!(plan.hottest(), Some(4));
    }

    #[test]
    fn reads_mysql_subqueries_and_unions() {
        let json = r#"{"query_block": {"union_result": {"using_temporary_table": true,
            "query_specifications": [
              {"query_block": {"select_id": 1, "message": "No tables used"}},
              {"query_block": {"select_id": 2, "table": {"table_name": "t", "access_type": "index", "key": "PRIMARY"},
               "attached_subqueries": [{"dependent": true, "query_block": {"select_id": 3,
                 "table": {"table_name": "u", "access_type": "eq_ref"}}}]}}
            ]}}}"#;
        let plan = parse(PlanSource::Mysql, &[], &cell(json)).unwrap();
        let titles: Vec<&str> = plan.lines().iter().map(|l| l.node.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "Query block",
                "Union",
                "Query block #1",
                "Query block #2",
                "Full index scan on t using PRIMARY",
                "Query block #3",
                "Unique index lookup on u",
            ]
        );
        assert_eq!(plan.lines()[2].node.details, vec!["No tables used"]);
    }

    #[test]
    fn reads_sqlite_plans() {
        let columns: Vec<String> = ["id", "parent", "notused", "detail"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let row = |id: i64, parent: i64, detail: &str| {
            vec![
                Value::Int(id),
                Value::Int(parent),
                Value::Int(0),
                Value::text(detail),
            ]
        };
        let rows = vec![
            row(3, 0, "SCAN o"),
            row(7, 0, "SEARCH c USING INTEGER PRIMARY KEY (rowid=?)"),
            row(12, 0, "CORRELATED SCALAR SUBQUERY 1"),
            row(
                16,
                12,
                "SEARCH i USING COVERING INDEX idx_items (order_id=?)",
            ),
            row(30, 0, "USE TEMP B-TREE FOR ORDER BY"),
        ];
        let plan = parse(PlanSource::Sqlite, &columns, &rows).unwrap();
        let titles: Vec<(usize, &str)> = plan
            .lines()
            .iter()
            .map(|l| (l.depth, l.node.title.as_str()))
            .collect();
        assert_eq!(
            titles,
            vec![
                (0, "SCAN o"),
                (0, "SEARCH c USING INTEGER PRIMARY KEY (rowid=?)"),
                (0, "CORRELATED SCALAR SUBQUERY 1"),
                (1, "SEARCH i USING COVERING INDEX idx_items (order_id=?)"),
                (0, "USE TEMP B-TREE FOR ORDER BY"),
            ]
        );
        // No numbers: the full scan is what stands out.
        assert_eq!(plan.hottest(), Some(0));
    }

    #[test]
    fn reads_mongo_plans() {
        let planner = r#"{"namespace": "shop.orders", "winningPlan": {"stage": "FETCH",
            "filter": {"total": {"$gt": 10}},
            "inputStage": {"stage": "IXSCAN", "keyPattern": {"status": 1}, "indexName": "status_1",
            "direction": "forward", "indexBounds": {"status": ["[\"open\", \"open\"]"]}}},
            "rejectedPlans": []}"#;
        let stats = r#"{"executionSuccess": true, "nReturned": 12, "executionTimeMillis": 3,
            "totalKeysExamined": 40, "totalDocsExamined": 40,
            "executionStages": {"stage": "FETCH", "nReturned": 12, "executionTimeMillisEstimate": 3,
              "docsExamined": 40, "filter": {"total": {"$gt": 10}},
              "inputStage": {"stage": "IXSCAN", "nReturned": 40, "executionTimeMillisEstimate": 1,
                "indexName": "status_1", "keysExamined": 40}}}"#;
        let columns = vec![
            "explainVersion".to_string(),
            "queryPlanner".into(),
            "executionStats".into(),
            "ok".into(),
        ];
        let rows = vec![vec![
            Value::text("1"),
            Value::Json(planner.into()),
            Value::Json(stats.into()),
            Value::Float(1.0),
        ]];
        let plan = parse(PlanSource::Mongo, &columns, &rows).unwrap();
        assert!(plan.analyzed);
        assert_eq!(plan.execution_ms, Some(3.0));
        let lines = plan.lines();
        assert_eq!(lines[0].node.title, "FETCH");
        assert_eq!(lines[1].node.title, "IXSCAN using status_1");
        assert_eq!(lines[0].node.self_time_ms, Some(2.0));
        assert_eq!(lines[1].node.actual_rows, Some(40.0));
        assert_eq!(plan.hottest(), Some(0));

        // The plan alone, without running it.
        let plan = parse(PlanSource::Mongo, &columns[..2], &[rows[0][..2].to_vec()]).unwrap();
        assert!(!plan.analyzed);
        assert!(
            lines[0]
                .node
                .details
                .iter()
                .any(|d| d.starts_with("Filter: "))
        );
        assert_eq!(
            plan.lines()[1].node.details[0],
            "Key pattern: {\"status\":1}"
        );
    }

    #[test]
    fn reads_mongo_aggregations() {
        let doc = r#"{"stages": [
            {"$cursor": {"queryPlanner": {"winningPlan": {"stage": "COLLSCAN", "direction": "forward"}}}},
            {"$group": {"_id": "$status", "n": {"$sum": 1}}, "nReturned": 3, "executionTimeMillisEstimate": 5}
          ], "ok": 1}"#;
        let plan = parse(PlanSource::Mongo, &["doc".into()], &cell(doc)).unwrap();
        let titles: Vec<(usize, &str)> = plan
            .lines()
            .iter()
            .map(|l| (l.depth, l.node.title.as_str()))
            .collect();
        assert_eq!(
            titles,
            vec![(0, "Aggregation pipeline"), (1, "COLLSCAN"), (1, "$group")]
        );
        assert!(plan.lines()[1].node.warning);
    }

    #[test]
    fn odd_input_is_an_error_not_a_panic() {
        for source in [PlanSource::Postgres, PlanSource::Mysql, PlanSource::Mongo] {
            assert!(parse(source, &[], &[]).is_err());
            assert!(parse(source, &["x".into()], &cell("not json")).is_err());
            assert!(parse(source, &["x".into()], &cell("[1, 2]")).is_err());
            assert!(parse(source, &["x".into()], &[vec![Value::Int(3)]]).is_err());
        }
        // A deep plan is cut, not followed to the bottom of the stack.
        let mut deep = String::from(r#"{"Node Type": "Leaf"}"#);
        for _ in 0..500 {
            deep = format!(r#"{{"Node Type": "Wrap", "Plans": [{deep}]}}"#);
        }
        let deep = format!(r#"[{{"Plan": {deep}}}]"#);
        let plan = parse(PlanSource::Postgres, &[], &cell(&deep));
        // serde_json refuses nesting this deep; either way nothing panics.
        if let Ok(plan) = plan {
            assert!(plan.lines().len() <= MAX_DEPTH + 1);
        }
        assert!(parse(PlanSource::Sqlite, &[], &[vec![Value::Null]]).is_ok());
    }

    #[test]
    fn explain_statements_per_dialect() {
        let explain =
            |source, statement, analyze| explain_statement(source, statement, analyze).unwrap();
        assert_eq!(
            explain(PlanSource::Postgres, "select 1;", false),
            "EXPLAIN (FORMAT JSON) select 1"
        );
        assert_eq!(
            explain(PlanSource::Mysql, "select 1", false),
            "EXPLAIN FORMAT=JSON select 1"
        );
        assert_eq!(
            explain(PlanSource::Sqlite, "select 1", false),
            "EXPLAIN QUERY PLAN select 1"
        );
        assert_eq!(
            explain(PlanSource::Mongo, "db.a.find({})", true),
            "db.a.find({})\n.explain(\"executionStats\")"
        );
        // A trailing `//` comment ends at the line break; an open `/*` refuses.
        assert_eq!(
            explain(PlanSource::Mongo, "db.a.find({}) // all", false),
            "db.a.find({}) // all\n.explain(\"queryPlanner\")"
        );
        assert!(explain_statement(PlanSource::Mongo, "db.a.deleteMany({}) /* x", false).is_err());
        assert!(!ends_in_open_comment("db.a.find({a: '/*'}) /* ok */"));
    }

    #[test]
    fn measured_postgres_plans_run_in_a_block_that_is_rolled_back() {
        let sql = explain_statement(
            PlanSource::Postgres,
            "delete from t where note = '$deltos_sql$' -- why",
            true,
        )
        .unwrap();
        assert!(sql.starts_with("DO $deltos_block$\n"), "{sql}");
        assert!(sql.ends_with("\nEND\n$deltos_block$"), "{sql}");
        assert!(sql.contains(
            "EXECUTE $deltos_sql1$EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) delete from t where note = '$deltos_sql$' -- why\n$deltos_sql1$ INTO plan;"
        ));
        assert!(sql.contains("RAISE EXCEPTION USING ERRCODE = 'DLT01'"));
        // The driver reads it as one statement.
        assert_eq!(
            deltos_drivers::sql::split_statements(&sql, Dialect::Postgres).len(),
            1
        );
        assert_eq!(
            measured_explain(&sql),
            Some(
                "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) delete from t where note = '$deltos_sql$' -- why"
            )
        );
        assert_eq!(measured_explain("select 1"), None);
        let rows = measured_plan(&["NOTICE: hi".into(), plan_notice("[{}]")]).unwrap();
        assert_eq!(rows, vec![vec![Value::text("[{}]")]]);
        assert!(measured_plan(&["NOTICE: hi".into()]).is_none());
    }
}
