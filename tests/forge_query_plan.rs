//! T-0033 · Q1a — Acceptance 4 and Falsifier 8: what the lowered SQL and the
//! derived indexes actually do on **the libsql engine FORGE links**, at 5k and
//! 500k rows.
//!
//! Queries come from real source chains through the classification pass;
//! indexes from `index::wanted` + `dedup`, created before any statement is
//! explained (a statement planned before a `CREATE INDEX` keeps its old plan).
//! Asserted plans are the ones Design 11 expects per shape; the shapes whose
//! plan is a trade-off (a range after unconstrained sort columns, OR) are
//! measured and printed, not asserted.
//!
//! **One database open at a time, in one test function**: several in-process
//! substrates open at once can crash the binary with `0xc0000005` and read as a
//! pass (`src/forge/libsql.rs`). Check this binary's exit status and its total.
//!
//! Set `ALBEDO_PLAN_OUT=<file>` to also write the measurements there.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use dom_render_compiler::forge::declare::CollectionDecl;
use dom_render_compiler::forge::query::index::{dedup, wanted, IndexSpec};
use dom_render_compiler::forge::query::{CollectionSchema, ParamRole, ParamSpec};
use dom_render_compiler::transforms::forge_queries::{classify_source, LoweredQuery};

fn schema() -> CollectionSchema {
    let decl: CollectionDecl = serde_json::from_str(
        r#"{ "fields": { "amount": "real", "due": "timestamp", "owner": "text", "status": "text" } }"#,
    )
    .expect("decl");
    CollectionSchema::from_decl("invoices", &decl)
}

/// The chain, lowered through the real classification pass.
fn lower(chain: &str) -> LoweredQuery {
    let mut schemas = BTreeMap::new();
    schemas.insert("invoices".to_string(), schema());
    let source = format!(
        "import {{ invoices }} from \"albedo/forge\";\nimport {{ or }} from \"albedo/query\";\nexport async function q(v) {{ return await {chain}; }}\n"
    );
    let mut c = classify_source("src/q.ts", &source, false, &schemas);
    assert!(c.diagnostics.is_empty(), "{chain}: {:?}", c.diagnostics);
    assert_eq!(c.queries.len(), 1, "{chain}");
    c.queries.remove(0)
}

struct Shape {
    name: &'static str,
    chain: &'static str,
    /// Values by column (and role for range bounds).
    values: &'static [(&'static str, Value)],
}

#[derive(Clone, Copy)]
enum Value {
    Text(&'static str),
    Int(i64),
    Real(f64),
    Json(&'static str),
}

fn bind(params: &[ParamSpec], binds: &[u16], values: &[(&str, Value)], lo_hi: (Value, Value)) -> Vec<libsql::Value> {
    binds
        .iter()
        .map(|slot| {
            let spec = params.iter().find(|p| p.slot == *slot).expect("param");
            let value = match spec.role {
                ParamRole::RangeLo => lo_hi.0,
                ParamRole::RangeHi => lo_hi.1,
                _ => values
                    .iter()
                    .find(|(c, _)| *c == spec.column)
                    .map(|(_, v)| *v)
                    .unwrap_or_else(|| panic!("no value for {}", spec.column)),
            };
            match value {
                Value::Text(t) | Value::Json(t) => libsql::Value::Text(t.to_string()),
                Value::Int(i) => libsql::Value::Integer(i),
                Value::Real(r) => libsql::Value::Real(r),
            }
        })
        .collect()
}

async fn plan(conn: &libsql::Connection, sql: &str, params: Vec<libsql::Value>) -> String {
    let mut rows = conn
        .query(&format!("EXPLAIN QUERY PLAN {sql}"), params)
        .await
        .expect("explain");
    let mut steps = Vec::new();
    while let Some(row) = rows.next().await.expect("row") {
        steps.push(row.get::<String>(3).expect("detail"));
    }
    steps.join(" | ")
}

/// Median wall time over `runs`, and the row count.
async fn time(conn: &libsql::Connection, sql: &str, params: Vec<libsql::Value>, runs: usize) -> (Duration, usize) {
    let mut times = Vec::new();
    let mut count = 0;
    for _ in 0..runs {
        let start = Instant::now();
        let mut rows = conn.query(sql, params.clone()).await.expect("query");
        count = 0;
        while rows.next().await.expect("row").is_some() {
            count += 1;
        }
        times.push(start.elapsed());
    }
    times.sort();
    (times[times.len() / 2], count)
}

async fn ids(conn: &libsql::Connection, sql: &str, params: Vec<libsql::Value>) -> Vec<i64> {
    let mut rows = conn.query(sql, params).await.expect("query");
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.expect("row") {
        out.push(row.get::<i64>(0).expect("id"));
    }
    out
}

const TABLE: &str = "CREATE TABLE invoices (id INTEGER PRIMARY KEY AUTOINCREMENT, amount REAL NOT NULL, due INTEGER NOT NULL, owner TEXT NOT NULL, status TEXT NOT NULL)";

async fn open(dir: &std::path::Path, name: &str) -> (libsql::Database, libsql::Connection) {
    let db = libsql::Builder::new_local(dir.join(name)).build().await.expect("open");
    let conn = db.connect().expect("connect");
    conn.execute(TABLE, ()).await.expect("table");
    (db, conn)
}

const SHAPES: &[Shape] = &[
    Shape {
        name: "equality + order + limit",
        chain: "invoices.where({ status: v }).orderBy(\"due\", \"desc\").limit(50)",
        values: &[("status", Value::Text("open"))],
    },
    Shape {
        name: "range on the first sort column + limit",
        chain: "invoices.where({ due: { gt: v } }).orderBy(\"due\").limit(50)",
        values: &[],
    },
    Shape {
        name: "equality + range, ordered by the range column",
        chain: "invoices.where({ status: v, amount: { gt: v } }).orderBy(\"amount\").limit(50)",
        values: &[("status", Value::Text("open"))],
    },
    Shape {
        name: "count with equality + range",
        chain: "invoices.where({ status: v, amount: { gt: v } }).count()",
        values: &[("status", Value::Text("open"))],
    },
    Shape {
        name: "count with equality",
        chain: "invoices.where({ status: v }).count()",
        values: &[("status", Value::Text("open"))],
    },
    Shape {
        name: "range on a non-sort column + order + limit",
        chain: "invoices.where({ status: v, amount: { gt: v } }).orderBy(\"due\").limit(50)",
        values: &[("status", Value::Text("open"))],
    },
    Shape {
        name: "or",
        chain: "invoices.where(or({ status: v }, { owner: v })).orderBy(\"due\").limit(50)",
        values: &[("status", Value::Text("void")), ("owner", Value::Text("o7"))],
    },
    Shape {
        name: "in + order",
        chain: "invoices.where({ owner: { in: v } }).orderBy(\"due\").limit(50)",
        values: &[("owner", Value::Json("[\"o1\",\"o2\",\"o3\"]"))],
    },
];

#[tokio::test]
async fn plans_per_shape_at_5k_and_500k_rows_on_libsql() {
    let dir = tempfile::tempdir().expect("tempdir");
    let queries: Vec<(&Shape, LoweredQuery)> = SHAPES.iter().map(|s| (s, lower(s.chain))).collect();
    let mut specs: Vec<IndexSpec> = Vec::new();
    for (_, q) in &queries {
        specs.extend(wanted(&q.canonical.descriptor, &schema()));
    }
    let indexes = dedup(specs, &[]);
    let mut report = String::new();
    writeln!(report, "derived indexes ({}):", indexes.len()).unwrap();
    for index in &indexes {
        writeln!(report, "  {}", index.ddl()).unwrap();
    }

    for rows in [5_000u32, 500_000] {
        let (db, conn) = open(dir.path(), &format!("plan-{rows}.db")).await;
        conn.execute(
            &format!(
                "INSERT INTO invoices(amount, due, owner, status) \
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < {rows}) \
                 SELECT (i * 7919) % 10000, (i * 104729) % 1000000, 'o' || (i % 100), \
                 CASE i % 3 WHEN 0 THEN 'open' WHEN 1 THEN 'paid' ELSE 'void' END FROM n"
            ),
            (),
        )
        .await
        .expect("seed");
        for index in &indexes {
            conn.execute(&index.ddl(), ()).await.expect("index");
        }
        let runs = if rows > 100_000 { 7 } else { 21 };
        writeln!(report, "\n== {rows} rows (no ANALYZE; median of {runs})").unwrap();
        for (shape, q) in &queries {
            let statement = &q.lowered.statements[0];
            let ranges: &[(&str, (Value, Value))] = match shape.name {
                "range on the first sort column + limit" => &[("", (Value::Int(500_000), Value::Int(0)))],
                "range on a non-sort column + order + limit" => &[
                    ("selective amount > 9990", (Value::Real(9990.0), Value::Real(0.0))),
                    ("non-selective amount > 10", (Value::Real(10.0), Value::Real(0.0))),
                ],
                _ => &[("", (Value::Real(5000.0), Value::Real(0.0)))],
            };
            for (label, lo_hi) in ranges {
                let params = bind(&q.lowered.params, &statement.binds, shape.values, *lo_hi);
                let detail = plan(&conn, &statement.sql, params.clone()).await;
                let (median, count) = time(&conn, &statement.sql, params, runs).await;
                writeln!(
                    report,
                    "{} {}\n   sql:  {}\n   plan: {}\n   {count} rows, median {:?}",
                    shape.name, label, statement.sql, detail, median
                )
                .unwrap();
                let search = detail.contains("SEARCH invoices USING");
                let temp_sort = detail.contains("USE TEMP B-TREE FOR ORDER BY");
                match shape.name {
                    "equality + order + limit" => {
                        assert!(search && detail.contains("(status=?)") && !temp_sort, "{rows}: {detail}");
                        assert!(detail.contains("idx_invoices_q"), "the derived index: {detail}");
                    }
                    "range on the first sort column + limit" => {
                        assert!(search && detail.contains("due>?") && !temp_sort, "{rows}: {detail}");
                    }
                    "equality + range, ordered by the range column" | "count with equality + range" => {
                        assert!(
                            search && detail.contains("status=? AND amount>?") && !temp_sort,
                            "{rows}: {detail}"
                        );
                    }
                    "count with equality" => {
                        assert!(search && detail.contains("(status=?)"), "{rows}: {detail}");
                    }
                    "range on a non-sort column + order + limit" => {
                        // Recorded, not asserted (AC4). With every derived index
                        // present the planner may prefer another query's
                        // (status, amount) index plus a sort for a selective
                        // range; the ESR claim is tested in isolation below.
                        assert!(search && detail.contains("status=?"), "{rows}: {detail}");
                    }
                    "in + order" => {
                        // R3-3: an `in` prefix gives SEARCH (owner=?) and a temp
                        // b-tree bounded by the matches — expected and labelled.
                        assert!(search && detail.contains("owner=?"), "{rows}: {detail}");
                        writeln!(report, "   (temp b-tree for ORDER BY expected here: {temp_sort})").unwrap();
                    }
                    _ => {} // or: recorded, not asserted
                }
            }
        }

        // Recorded for Q1b: the same competing-index choice once the planner has
        // statistics (`ANALYZE`), since without them it can pick another query's
        // index and sort the whole partition.
        conn.execute("ANALYZE", ()).await.expect("analyze");
        {
            let (shape, q) = queries
                .iter()
                .find(|(s, _)| s.name == "range on a non-sort column + order + limit")
                .unwrap();
            let statement = &q.lowered.statements[0];
            for (label, lo) in [("selective", 9990.0), ("non-selective", 10.0)] {
                let params = bind(&q.lowered.params, &statement.binds, shape.values, (Value::Real(lo), Value::Real(0.0)));
                let detail = plan(&conn, &statement.sql, params.clone()).await;
                let (median, count) = time(&conn, &statement.sql, params, runs).await;
                writeln!(report, "after ANALYZE, all indexes, {label}: {detail}\n   {count} rows, median {median:?}").unwrap();
            }
        }
        conn.execute("DROP TABLE IF EXISTS sqlite_stat1", ()).await.ok();
        conn.execute("DROP TABLE IF EXISTS sqlite_stat4", ()).await.ok();

        // Falsifier 8, part 1, in isolation: the ESR fixture (equality status,
        // range amount, order due) with only ITS derived index serves the order
        // without a sort; with the range column put before the sort column it
        // needs one — so the "no temp b-tree" assertion can fail.
        for index in &indexes {
            conn.execute(&format!("DROP INDEX {}", index.name()), ()).await.expect("drop");
        }
        let (shape, q) = queries
            .iter()
            .find(|(s, _)| s.name == "range on a non-sort column + order + limit")
            .unwrap();
        let own = dedup(wanted(&q.canonical.descriptor, &schema()), &[]);
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].column_spec(), "status, due, id, amount", "ESR order");
        conn.execute(&own[0].ddl(), ()).await.expect("esr index");
        let statement = &q.lowered.statements[0];
        for (label, lo) in [("selective", 9990.0), ("non-selective", 10.0)] {
            let params = bind(&q.lowered.params, &statement.binds, shape.values, (Value::Real(lo), Value::Real(0.0)));
            let detail = plan(&conn, &statement.sql, params.clone()).await;
            let (median, count) = time(&conn, &statement.sql, params, runs).await;
            writeln!(report, "ESR index alone, {label}: {detail}
   {count} rows, median {median:?}").unwrap();
            assert!(!detail.contains("USE TEMP B-TREE FOR ORDER BY"), "{rows} {label}: {detail}");
        }
        conn.execute(&format!("DROP INDEX {}", own[0].name()), ()).await.expect("drop");
        conn.execute("CREATE INDEX mutant_range_first ON invoices(status, amount, due, id)", ())
            .await
            .expect("mutant index");
        let params = bind(&q.lowered.params, &statement.binds, shape.values, (Value::Real(9990.0), Value::Real(0.0)));
        let detail = plan(&conn, &statement.sql, params).await;
        writeln!(report, "mutant (range column before sort): {detail}").unwrap();
        assert!(detail.contains("USE TEMP B-TREE FOR ORDER BY"), "the mutant must need a sort: {detail}");
        drop(conn);
        drop(db);
    }

    // Falsifier 8, part 2 (R4-5): equal `due`, keys inserted 3, 1, 2, `amount`
    // opposite to the key. The real query returns key order; the mutant with the
    // tie-break removed from ORDER BY and from the index returns amount order.
    let (db, conn) = open(dir.path(), "ties.db").await;
    for (id, amount) in [(3, 10.0), (1, 30.0), (2, 20.0)] {
        conn.execute(
            "INSERT INTO invoices(id, amount, due, owner, status) VALUES (?1, ?2, 10, 'o', 'open')",
            libsql::params![id, amount],
        )
        .await
        .expect("tie row");
    }
    let q = lower("invoices.where({ status: v, amount: { gt: v } }).orderBy(\"due\").limit(3)");
    for index in dedup(wanted(&q.canonical.descriptor, &schema()), &[]) {
        conn.execute(&index.ddl(), ()).await.expect("index");
    }
    let statement = &q.lowered.statements[0];
    let values = &[("status", Value::Text("open"))];
    let params = bind(&q.lowered.params, &statement.binds, values, (Value::Real(0.0), Value::Real(0.0)));
    let real = ids(&conn, &statement.sql, params.clone()).await;
    assert_eq!(real, vec![1, 2, 3], "ties come back in key order: {}", statement.sql);
    assert!(statement.sql.contains("ORDER BY due, id"), "{}", statement.sql);

    let (db2, conn2) = open(dir.path(), "ties-mutant.db").await;
    drop(conn);
    drop(db);
    for (id, amount) in [(3, 10.0), (1, 30.0), (2, 20.0)] {
        conn2
            .execute(
                "INSERT INTO invoices(id, amount, due, owner, status) VALUES (?1, ?2, 10, 'o', 'open')",
                libsql::params![id, amount],
            )
            .await
            .expect("tie row");
    }
    conn2
        .execute("CREATE INDEX mutant_no_key ON invoices(status, due, amount)", ())
        .await
        .expect("mutant index");
    let mutant_sql = statement.sql.replace("ORDER BY due, id", "ORDER BY due");
    let mutant = ids(&conn2, &mutant_sql, params).await;
    writeln!(report, "\nties: real {real:?}; tie-break removed {mutant:?}").unwrap();
    assert_ne!(mutant, real, "the mutant must reorder the ties, or this test proves nothing");
    drop(conn2);
    drop(db2);

    eprintln!("{report}");
    if let Ok(path) = std::env::var("ALBEDO_PLAN_OUT") {
        std::fs::write(path, &report).expect("write report");
    }
}
