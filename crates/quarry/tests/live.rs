//! The SQL drivers exercised against real servers.
//!
//! These are `#[ignore]`d: they need something listening, so they must never run in a normal
//! `cargo test`. They expect the containers from `tools/quarry/docker-compose.yml` with its
//! seed data loaded, and run as
//!
//! ```text
//! cargo test -p quarry --test live -- --ignored --nocapture --test-threads=1
//! ```
//!
//! They exist because the alternative is guessing from the UI whether a blank tree means a
//! broken driver or a broken panel, and that guess is expensive to get wrong. Every test leaves
//! the data as it found it: edits are made and then reverted.

use quarry::driver::{CellEdit, Driver, Engine, ObjectKind, ObjectNode, ResultSet};
use quarry::drivers::{postgres::PostgresDriver, sqlserver::SqlServerDriver};
use std::{sync::Arc, time::Duration};
use ui::SharedString;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("building a tokio runtime")
}

fn path(segments: &[&str]) -> Vec<SharedString> {
    segments
        .iter()
        .map(|segment| SharedString::from(segment.to_string()))
        .collect()
}

fn find<'a>(nodes: &'a [ObjectNode], name: &str, kind: ObjectKind) -> &'a ObjectNode {
    nodes
        .iter()
        .find(|node| node.name.as_ref() == name && node.kind == kind)
        .unwrap_or_else(|| panic!("no {kind:?} named {name} in {nodes:#?}"))
}

fn print_nodes(label: &str, nodes: &[ObjectNode]) {
    println!("--- {label} ---");
    for node in nodes {
        println!(
            "  {} [{:?}]{}  path={:?}",
            node.name,
            node.kind,
            node.detail
                .as_ref()
                .map(|detail| format!(" — {detail}"))
                .unwrap_or_default(),
            node.path
        );
    }
}

fn only(mut sets: Vec<ResultSet>) -> ResultSet {
    assert_eq!(sets.len(), 1, "expected one result set, got {sets:#?}");
    sets.remove(0)
}

fn cell(set: &ResultSet, row: usize, column: usize) -> Option<String> {
    set.rows[row][column]
        .as_ref()
        .map(|value| value.to_string())
}

/// Runs `slow` in its own task, gives it a second to get going, stops it with `stop`, and
/// returns how long the task took to finish after that and what it returned (`None` when the
/// task was aborted rather than finishing).
async fn interrupt<D: Driver + 'static>(
    driver: Arc<D>,
    slow: &'static str,
    database: Option<&'static str>,
    abort: bool,
) -> (Duration, Option<anyhow::Result<Vec<ResultSet>>>) {
    let task = tokio::spawn({
        let driver = driver.clone();
        async move { driver.query(slow, database, 10).await }
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    let stopped_at = std::time::Instant::now();
    if abort {
        task.abort();
        let outcome = task.await;
        assert!(outcome.is_err(), "the aborted task finished anyway");
        (stopped_at.elapsed(), None)
    } else {
        driver.cancel().await.expect("cancelling");
        let outcome = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the cancelled query was still running after 10s")
            .expect("joining the query task");
        (stopped_at.elapsed(), Some(outcome))
    }
}

async fn postgres() -> Arc<PostgresDriver> {
    Arc::new(
        PostgresDriver::connect("localhost", 55432, Some("app"), Some("app"), Some("app"))
            .await
            .expect("connecting to postgres"),
    )
}

#[test]
#[ignore = "needs a live postgres"]
fn postgres_tree() {
    runtime().block_on(async {
        let driver = postgres().await;

        let databases = driver.children(&[]).await.expect("databases");
        print_nodes("databases", &databases);
        let app = find(&databases, "app", ObjectKind::Database);
        assert_eq!(app.detail.as_deref(), Some("connected"));

        let schemas = driver.children(&app.path).await.expect("schemas");
        print_nodes("schemas", &schemas);
        let shop = find(&schemas, "shop", ObjectKind::Schema);
        find(&schemas, "hr", ObjectKind::Schema);

        let objects = driver.children(&shop.path).await.expect("objects");
        print_nodes("shop", &objects);
        let customers = find(&objects, "customers", ObjectKind::Table);
        assert_eq!(customers.path, path(&["app", "shop", "customers"]));
        assert!(
            customers
                .detail
                .as_ref()
                .is_some_and(|detail| detail.ends_with("rows")),
            "no row estimate on customers"
        );
        find(&objects, "customer_summary", ObjectKind::View);
        find(&objects, "monthly_revenue", ObjectKind::View);
        find(&objects, "customers_id_seq", ObjectKind::Sequence);
        let first_view = objects
            .iter()
            .position(|node| node.kind == ObjectKind::View)
            .expect("a view");
        let last_table = objects
            .iter()
            .rposition(|node| node.kind == ObjectKind::Table)
            .expect("a table");
        assert!(
            last_table < first_view,
            "tables are not listed before views"
        );

        let columns = driver.children(&customers.path).await.expect("columns");
        print_nodes("shop.customers", &columns);
        let id = find(&columns, "id", ObjectKind::Column);
        assert_eq!(id.detail.as_deref(), Some("integer not null"));
        assert_eq!(id.path, path(&["app", "shop", "customers", "id"]));
        let primary = find(&columns, "customers_pkey", ObjectKind::Index);
        assert_eq!(primary.detail.as_deref(), Some("primary key btree (id)"));
        let unique = find(&columns, "customers_email_key", ObjectKind::Index);
        assert_eq!(unique.detail.as_deref(), Some("unique btree (email)"));
        let last_column = columns
            .iter()
            .rposition(|node| node.kind == ObjectKind::Column)
            .expect("a column");
        assert!(
            last_column < columns.len() - 2,
            "indexes are not after columns"
        );

        let view_columns = driver
            .children(&path(&["app", "shop", "customer_summary"]))
            .await
            .expect("view columns");
        find(&view_columns, "full_name", ObjectKind::Column);

        assert!(
            driver
                .databases()
                .await
                .expect("databases")
                .iter()
                .any(|name| name.as_ref() == "app")
        );
        assert_eq!(driver.default_database().as_deref(), Some("app"));
    });
}

#[test]
#[ignore = "needs a live postgres"]
fn postgres_query() {
    runtime().block_on(async {
        let driver = postgres().await;

        let selected = only(
            driver
                .query("SELECT id, phone FROM shop.customers ORDER BY id", None, 3)
                .await
                .expect("select"),
        );
        println!("select: {:?} {:?}", selected.columns, selected.rows);
        assert_eq!(selected.columns, path(&["id", "phone"]));
        assert_eq!(selected.rows.len(), 3);
        assert!(selected.truncated);
        assert_eq!(selected.rows_affected, None);

        let nulls = only(
            driver
                .query("SELECT 1 AS one, NULL::text AS nothing", Some("app"), 10)
                .await
                .expect("select literals"),
        );
        assert_eq!(nulls.rows, vec![vec![Some("1".into()), None]]);

        let empty = only(
            driver
                .query("SELECT id, email FROM shop.customers WHERE false", None, 10)
                .await
                .expect("empty select"),
        );
        println!(
            "empty: {:?} affected={:?}",
            empty.columns, empty.rows_affected
        );
        assert_eq!(empty.columns, path(&["id", "email"]));
        assert!(empty.rows.is_empty());
        assert_eq!(empty.rows_affected, None);

        let updated = only(
            driver
                .query(
                    "UPDATE shop.customers SET city = city WHERE id <= 3",
                    None,
                    10,
                )
                .await
                .expect("update"),
        );
        println!("update: affected={:?}", updated.rows_affected);
        assert!(updated.columns.is_empty());
        assert_eq!(updated.rows_affected, Some(3));

        let none_updated = only(
            driver
                .query(
                    "UPDATE shop.customers SET city = city WHERE false",
                    None,
                    10,
                )
                .await
                .expect("update nothing"),
        );
        assert_eq!(none_updated.rows_affected, Some(0));
        assert!(none_updated.columns.is_empty());

        let error = driver
            .query("SELECT * FROM no_such_table", None, 10)
            .await
            .expect_err("querying a missing table");
        println!("error: {error:#}");
        assert!(format!("{error:#}").contains("no_such_table"));

        // Rows past the limit are drained, so the session is still in step afterwards.
        let long = only(
            driver
                .query("SELECT generate_series(1, 100000) AS n", None, 5)
                .await
                .expect("long select"),
        );
        assert_eq!(long.rows.len(), 5);
        assert!(long.truncated);
        for _ in 0..6 {
            let after = only(driver.query("SELECT 2", None, 10).await.expect("after"));
            assert_eq!(cell(&after, 0, 0).as_deref(), Some("2"));
        }
    });
}

#[test]
#[ignore = "needs a live postgres"]
fn postgres_session() {
    runtime().block_on(async {
        let driver = postgres().await;
        let run = |statement: &'static str| {
            let driver = driver.clone();
            async move {
                driver
                    .query(statement, None, 100)
                    .await
                    .unwrap_or_else(|error| panic!("{statement}: {error:#}"))
            }
        };
        let count =
            |statement: &'static str| async move { cell(&only(run(statement).await), 0, 0) };

        run("DROP TABLE IF EXISTS public.quarry_live_scratch").await;
        run("CREATE TABLE public.quarry_live_scratch (id integer PRIMARY KEY, note text)").await;

        // Each statement is its own call, as the editor sends them.
        run("BEGIN").await;
        let inserted =
            only(run("INSERT INTO public.quarry_live_scratch VALUES (1, 'rolled back')").await);
        assert_eq!(inserted.rows_affected, Some(1));
        assert_eq!(
            count("SELECT count(*) FROM public.quarry_live_scratch")
                .await
                .as_deref(),
            Some("1"),
            "the insert is not visible to the next statement of its own transaction"
        );
        // The pool is a different session, so it must not see the uncommitted row.
        let preview = driver
            .preview(&path(&["app", "public", "quarry_live_scratch"]), 10)
            .await
            .expect("preview during a transaction");
        assert!(
            preview.rows.is_empty(),
            "the preview saw an uncommitted row"
        );
        run("ROLLBACK").await;
        assert_eq!(
            count("SELECT count(*) FROM public.quarry_live_scratch")
                .await
                .as_deref(),
            Some("0"),
            "ROLLBACK did not undo the insert"
        );

        run("BEGIN").await;
        run("INSERT INTO public.quarry_live_scratch VALUES (2, 'committed')").await;
        run("COMMIT").await;
        let committed = driver
            .preview(&path(&["app", "public", "quarry_live_scratch"]), 10)
            .await
            .expect("preview after commit");
        println!("after commit: {:?}", committed.rows);
        assert_eq!(
            committed.rows,
            vec![vec![Some("2".into()), Some("committed".into())]]
        );

        // An error inside a transaction leaves it aborted, as in psql, until it is rolled back.
        run("BEGIN").await;
        driver
            .query("SELECT 1 / 0", None, 10)
            .await
            .expect_err("dividing by zero");
        let aborted = driver
            .query("SELECT 1", None, 10)
            .await
            .expect_err("a statement in an aborted transaction");
        println!("aborted transaction: {aborted:#}");
        run("ROLLBACK").await;
        assert_eq!(count("SELECT 3").await.as_deref(), Some("3"));

        run("SET search_path TO shop, public").await;
        let customers = count("SELECT count(*) FROM customers").await;
        println!("customers through search_path: {customers:?}");
        assert!(customers.is_some());
        assert_eq!(
            count("SHOW search_path").await.as_deref(),
            Some("shop, public")
        );

        // Abandoning a statement mid-transaction loses the session and its transaction; the
        // next statement gets a fresh session with default settings.
        run("BEGIN").await;
        run("INSERT INTO public.quarry_live_scratch VALUES (3, 'abandoned')").await;
        interrupt(driver.clone(), "SELECT pg_sleep(30)", None, true).await;
        assert_eq!(
            count("SELECT count(*) FROM public.quarry_live_scratch WHERE id = 3")
                .await
                .as_deref(),
            Some("0")
        );
        assert_eq!(
            count("SHOW search_path").await.as_deref(),
            Some("\"$user\", public")
        );

        run("DROP TABLE public.quarry_live_scratch").await;
    });
}

#[test]
#[ignore = "needs a live postgres"]
fn postgres_preview_and_edit() {
    runtime().block_on(async {
        let driver = postgres().await;
        let customers = path(&["app", "shop", "customers"]);

        let preview = driver.preview(&customers, 5).await.expect("preview");
        println!(
            "preview: {:?} target={:?}",
            preview.columns, preview.edit_target
        );
        assert_eq!(preview.rows.len(), 5);
        assert!(preview.truncated);
        let target = preview.edit_target.clone().expect("customers is editable");
        assert_eq!(target.path, customers);
        assert_eq!(target.key_columns, vec![0]);

        let composite = driver
            .preview(&path(&["app", "shop", "order_items"]), 2)
            .await
            .expect("preview order_items");
        assert_eq!(
            composite
                .edit_target
                .expect("order_items is editable")
                .key_columns,
            vec![0, 1]
        );

        let view = driver
            .preview(&path(&["app", "shop", "customer_summary"]), 2)
            .await
            .expect("preview view");
        assert_eq!(view.rows.len(), 2);
        assert_eq!(view.edit_target, None);
        let materialized = driver
            .preview(&path(&["app", "shop", "monthly_revenue"]), 2)
            .await
            .expect("preview materialized view");
        assert_eq!(materialized.edit_target, None);

        let empty = driver
            .preview(&customers, 0)
            .await
            .expect("preview nothing");
        assert!(!empty.columns.is_empty());

        let phone_column = preview
            .columns
            .iter()
            .position(|column| column.as_ref() == "phone")
            .expect("a phone column");
        let id = cell(&preview, 0, 0).expect("an id");
        let original = cell(&preview, 0, phone_column);
        let edit = |value: Option<String>| CellEdit {
            path: customers.clone(),
            key: vec![("id".into(), Some(id.clone().into()))],
            column: "phone".into(),
            value,
        };

        let changed = driver
            .update_cell(&edit(Some("+90 it's quarry".into())))
            .await
            .expect("updating a cell");
        assert_eq!(changed, 1);
        let read = only(
            driver
                .query(
                    &format!("SELECT phone FROM shop.customers WHERE id = {id}"),
                    None,
                    1,
                )
                .await
                .expect("reading back"),
        );
        assert_eq!(cell(&read, 0, 0).as_deref(), Some("+90 it's quarry"));

        driver.update_cell(&edit(None)).await.expect("setting NULL");
        let read = only(
            driver
                .query(
                    &format!("SELECT phone FROM shop.customers WHERE id = {id}"),
                    None,
                    1,
                )
                .await
                .expect("reading back"),
        );
        assert_eq!(cell(&read, 0, 0), None);

        driver
            .update_cell(&edit(original.clone()))
            .await
            .expect("restoring");

        let missing = driver
            .update_cell(&CellEdit {
                key: vec![("id".into(), Some("-1".into()))],
                ..edit(Some("x".into()))
            })
            .await
            .expect_err("updating a row that does not exist");
        println!("missing row: {missing:#}");

        let too_many = driver
            .update_cell(&CellEdit {
                key: vec![("is_active".into(), Some("t".into()))],
                ..edit(Some("x".into()))
            })
            .await
            .expect_err("updating with a key that matches many rows");
        println!("too many rows: {too_many:#}");
        let read = only(
            driver
                .query(
                    "SELECT count(*) FROM shop.customers WHERE phone = 'x'",
                    None,
                    1,
                )
                .await
                .expect("checking the rollback"),
        );
        assert_eq!(cell(&read, 0, 0).as_deref(), Some("0"));
    });
}

#[test]
#[ignore = "needs a live postgres"]
fn postgres_definitions_and_catalog() {
    runtime().block_on(async {
        let driver = postgres().await;
        driver
            .query(
                "CREATE OR REPLACE FUNCTION public.quarry_live_add(a integer, b integer)
                 RETURNS integer LANGUAGE sql AS 'SELECT a + b'",
                None,
                10,
            )
            .await
            .expect("creating a function");
        driver
            .query(
                "CREATE OR REPLACE FUNCTION public.quarry_live_add(a text, b text)
                 RETURNS text LANGUAGE sql AS 'SELECT a || b'",
                None,
                10,
            )
            .await
            .expect("creating an overload");

        let view = driver
            .definition(&path(&["app", "shop", "customer_summary"]))
            .await
            .expect("view definition");
        println!("{view}");
        assert!(
            view.starts_with("CREATE OR REPLACE VIEW \"shop\".\"customer_summary\" AS\nSELECT")
        );
        let materialized = driver
            .definition(&path(&["app", "shop", "monthly_revenue"]))
            .await
            .expect("materialized view definition");
        assert!(materialized.starts_with("CREATE MATERIALIZED VIEW"));

        let public = driver
            .children(&path(&["app", "public"]))
            .await
            .expect("public objects");
        print_nodes("public", &public);
        let overloads: Vec<&ObjectNode> = public
            .iter()
            .filter(|node| node.name.as_ref() == "quarry_live_add")
            .collect();
        assert_eq!(overloads.len(), 2);
        assert!(
            overloads
                .iter()
                .all(|node| node.kind == ObjectKind::Routine)
        );
        let mut definitions = Vec::new();
        for overload in &overloads {
            let definition = driver.definition(&overload.path).await.expect("function");
            println!("{definition}");
            assert!(definition.starts_with("CREATE OR REPLACE FUNCTION public.quarry_live_add"));
            definitions.push(definition);
        }
        assert_ne!(definitions[0], definitions[1]);

        assert!(
            driver
                .definition(&path(&["app", "shop", "customers"]))
                .await
                .is_err()
        );

        let catalog = driver.catalog(None).await.expect("catalog");
        println!("catalog: {} entries", catalog.len());
        let has = |kind: ObjectKind, label: &str, parent: Option<&str>| {
            catalog.iter().any(|entry| {
                entry.kind == kind
                    && entry.label.as_ref() == label
                    && entry.parent.as_deref() == parent
            })
        };
        assert!(has(ObjectKind::Schema, "shop", None));
        assert!(has(ObjectKind::Table, "customers", Some("shop")));
        assert!(has(ObjectKind::View, "customer_summary", Some("shop")));
        assert!(has(ObjectKind::Column, "email", Some("customers")));
        assert!(has(ObjectKind::Routine, "quarry_live_add", Some("public")));

        driver
            .query(
                "DROP FUNCTION public.quarry_live_add(integer, integer)",
                None,
                10,
            )
            .await
            .expect("dropping the function");
        driver
            .query("DROP FUNCTION public.quarry_live_add(text, text)", None, 10)
            .await
            .expect("dropping the overload");
    });
}

#[test]
#[ignore = "needs a live postgres"]
fn postgres_cancel() {
    runtime().block_on(async {
        let driver = postgres().await;

        let (elapsed, outcome) =
            interrupt(driver.clone(), "SELECT pg_sleep(30)", None, false).await;
        let error = outcome
            .expect("the task finished")
            .expect_err("the cancelled query succeeded");
        println!("cancelled after {elapsed:?}: {error:#}");
        assert!(elapsed < Duration::from_secs(5));

        let after = only(
            driver
                .query("SELECT 1", None, 10)
                .await
                .expect("after cancel"),
        );
        assert_eq!(cell(&after, 0, 0).as_deref(), Some("1"));

        let (elapsed, _) = interrupt(driver.clone(), "SELECT pg_sleep(30)", None, true).await;
        println!("aborted after {elapsed:?}");
        let started = std::time::Instant::now();
        let after = only(
            driver
                .query("SELECT 1", None, 10)
                .await
                .expect("after abort"),
        );
        assert_eq!(cell(&after, 0, 0).as_deref(), Some("1"));
        assert!(started.elapsed() < Duration::from_secs(5));

        // The abandoned backend is cancelled in the background, not left sleeping.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let sleeping = only(
            driver
                .query(
                    "SELECT count(*) FROM pg_stat_activity
                     WHERE query = 'SELECT pg_sleep(30)' AND state = 'active'",
                    None,
                    10,
                )
                .await
                .expect("looking for the sleeping backend"),
        );
        assert_eq!(cell(&sleeping, 0, 0).as_deref(), Some("0"));
    });
}

async fn sqlserver() -> Arc<SqlServerDriver> {
    Arc::new(
        SqlServerDriver::connect(
            "localhost",
            1433,
            Some("app"),
            Some("sa"),
            Some("Quarry_Pass123!"),
        )
        .await
        .expect("connecting to sql server"),
    )
}

#[test]
#[ignore = "needs a live sql server"]
fn sqlserver_tree() {
    runtime().block_on(async {
        let driver = sqlserver().await;

        let databases = driver.children(&[]).await.expect("databases");
        print_nodes("databases", &databases);
        let app = find(&databases, "app", ObjectKind::Database);
        assert_eq!(app.detail.as_deref(), Some("connected"));
        let master = find(&databases, "master", ObjectKind::Database);
        assert_eq!(master.detail.as_deref(), Some("system"));
        assert_eq!(
            databases.first().map(|node| node.name.as_ref()),
            Some("app")
        );

        let schemas = driver.children(&app.path).await.expect("schemas");
        print_nodes("schemas", &schemas);
        let sales = find(&schemas, "sales", ObjectKind::Schema);
        find(&schemas, "inventory", ObjectKind::Schema);
        assert!(schemas.iter().all(|node| node.name.as_ref() != "db_owner"));

        let objects = driver.children(&sales.path).await.expect("objects");
        print_nodes("sales", &objects);
        let receipts = find(&objects, "Receipts", ObjectKind::Table);
        assert_eq!(receipts.path, path(&["app", "sales", "Receipts"]));
        assert!(
            receipts
                .detail
                .as_ref()
                .is_some_and(|detail| detail.starts_with('~') && detail.ends_with("rows"))
        );
        find(&objects, "StoreDailyRevenue", ObjectKind::View);
        let procedure = find(&objects, "TopItems", ObjectKind::Routine);
        assert_eq!(procedure.detail.as_deref(), Some("procedure"));

        let columns = driver.children(&receipts.path).await.expect("columns");
        print_nodes("sales.Receipts", &columns);
        let total = find(&columns, "Total", ObjectKind::Column);
        assert_eq!(total.detail.as_deref(), Some("money not null"));
        let payment = find(&columns, "PaymentType", ObjectKind::Column);
        assert_eq!(payment.detail.as_deref(), Some("nvarchar(20) not null"));
        let customer = find(&columns, "CustomerId", ObjectKind::Column);
        assert_eq!(customer.detail.as_deref(), Some("int"));
        let sold_at = find(&columns, "IX_Receipts_SoldAt", ObjectKind::Index);
        assert_eq!(sold_at.detail.as_deref(), Some("nonclustered (SoldAt)"));
        assert_eq!(
            sold_at.path,
            path(&["app", "sales", "Receipts", "IX_Receipts_SoldAt"])
        );
        assert!(columns.iter().any(|node| node.kind == ObjectKind::Index
            && node.detail.as_deref() == Some("primary key (ReceiptId)")));

        let lines = driver
            .children(&path(&["app", "sales", "ReceiptLines"]))
            .await
            .expect("receipt lines");
        assert!(
            lines
                .iter()
                .any(|node| node.detail.as_deref() == Some("primary key (ReceiptId, LineNumber)"))
        );

        let databases = driver.databases().await.expect("databases");
        assert_eq!(databases.first().map(|name| name.as_ref()), Some("app"));
        assert!(databases.iter().any(|name| name.as_ref() == "master"));
        assert_eq!(driver.default_database().as_deref(), Some("app"));
    });
}

#[test]
#[ignore = "needs a live sql server"]
fn sqlserver_query() {
    runtime().block_on(async {
        let driver = sqlserver().await;

        let sets = driver
            .query(
                "SELECT TOP 5 ReceiptId, Total, SoldAt FROM sales.Receipts ORDER BY ReceiptId;
                 SELECT 1 AS one WHERE 1 = 0;
                 SELECT N'İstanbul' AS city",
                None,
                3,
            )
            .await
            .expect("multi-result batch");
        for set in &sets {
            println!(
                "{:?} {:?} truncated={}",
                set.columns, set.rows, set.truncated
            );
        }
        assert_eq!(sets.len(), 3);
        assert_eq!(sets[0].rows.len(), 3);
        assert!(sets[0].truncated);
        assert_eq!(sets[1].columns, path(&["one"]));
        assert!(sets[1].rows.is_empty());
        assert_eq!(cell(&sets[2], 0, 0).as_deref(), Some("İstanbul"));

        let values = only(
            driver
                .query(
                    "SELECT CAST('2026-10-22 13:04:56.997' AS datetime) AS legacy,
                            CAST('2026-10-22 13:04:00' AS smalldatetime) AS small,
                            CAST('2026-10-22 13:04:56.1234567' AS datetime2) AS precise,
                            CAST('2026-10-22 13:04:56' AS datetime2(0)) AS seconds,
                            CAST('2026-10-22' AS date) AS day,
                            CAST('13:04:56.123' AS time(3)) AS clock,
                            CAST('2026-10-22 13:04:56 +03:00' AS datetimeoffset(0)) AS offset,
                            CAST(1234.5 AS money) AS price,
                            CAST(12.30 AS decimal(10, 2)) AS amount,
                            CAST(1 AS bit) AS flag,
                            CAST('6F9619FF-8B86-D011-B42D-00C04FC964FF' AS uniqueidentifier) AS id,
                            0x0102AB AS bytes,
                            CAST(NULL AS int) AS nothing",
                    None,
                    10,
                )
                .await
                .expect("typed values"),
        );
        let rendered: Vec<(String, Option<String>)> = values
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| (column.to_string(), cell(&values, 0, index)))
            .collect();
        println!("{rendered:#?}");
        let expected = [
            Some("2026-10-22 13:04:56.997"),
            Some("2026-10-22 13:04:00"),
            Some("2026-10-22 13:04:56.123456700"),
            Some("2026-10-22 13:04:56"),
            Some("2026-10-22"),
            Some("13:04:56.123"),
            Some("2026-10-22 13:04:56 +03:00"),
            Some("1234.5"),
            Some("12.30"),
            Some("1"),
            Some("6F9619FF-8B86-D011-B42D-00C04FC964FF"),
            Some("0x0102AB"),
            None,
        ];
        for ((column, value), expected) in rendered.iter().zip(expected) {
            assert_eq!(value.as_deref(), expected, "rendering {column}");
        }

        let updated = only(
            driver
                .query(
                    "UPDATE sales.Regions SET Name = Name WHERE RegionId <= 2",
                    None,
                    10,
                )
                .await
                .expect("update"),
        );
        println!("update: affected={:?}", updated.rows_affected);
        assert!(updated.columns.is_empty());
        assert_eq!(updated.rows_affected, Some(2));

        let master = only(
            driver
                .query("SELECT DB_NAME()", Some("master"), 10)
                .await
                .expect("query in master"),
        );
        assert_eq!(cell(&master, 0, 0).as_deref(), Some("master"));
        let app = only(
            driver
                .query("SELECT DB_NAME()", None, 10)
                .await
                .expect("query in the default database"),
        );
        assert_eq!(cell(&app, 0, 0).as_deref(), Some("app"));

        // Batches share a session, so a temp table outlives the batch that made it.
        driver
            .query(
                "CREATE TABLE #quarry (n int); INSERT INTO #quarry VALUES (1), (2)",
                None,
                10,
            )
            .await
            .expect("creating a temp table");
        let temp = only(
            driver
                .query("SELECT COUNT(*) FROM #quarry", None, 10)
                .await
                .expect("reading the temp table"),
        );
        assert_eq!(cell(&temp, 0, 0).as_deref(), Some("2"));
        driver
            .query("DROP TABLE #quarry", None, 10)
            .await
            .expect("dropping the temp table");

        let error = driver
            .query("SELECT * FROM dbo.NoSuchTable", None, 10)
            .await
            .expect_err("querying a missing table");
        println!("error: {error:#}");
        let after = only(
            driver
                .query("SELECT 7", None, 10)
                .await
                .expect("after an error"),
        );
        assert_eq!(cell(&after, 0, 0).as_deref(), Some("7"));
    });
}

#[test]
#[ignore = "needs a live sql server"]
fn sqlserver_preview_and_edit() {
    runtime().block_on(async {
        let driver = sqlserver().await;
        let customers = path(&["app", "sales", "Customers"]);

        let preview = driver.preview(&customers, 5).await.expect("preview");
        println!(
            "preview: {:?} target={:?}",
            preview.columns, preview.edit_target
        );
        assert_eq!(preview.rows.len(), 5);
        assert!(preview.truncated);
        let target = preview.edit_target.clone().expect("Customers is editable");
        assert_eq!(target.path, customers);
        assert_eq!(target.key_columns, vec![0]);
        // CustomerId is an identity column.
        assert_eq!(target.read_only_columns, vec![0]);

        let stock = driver
            .preview(&path(&["app", "inventory", "StockLevels"]), 2)
            .await
            .expect("preview StockLevels");
        assert_eq!(
            stock
                .edit_target
                .expect("StockLevels is editable")
                .key_columns,
            vec![0, 1]
        );

        let view = driver
            .preview(&path(&["app", "sales", "StoreDailyRevenue"]), 2)
            .await
            .expect("preview view");
        assert_eq!(view.rows.len(), 2);
        assert_eq!(view.edit_target, None);

        let tier_column = preview
            .columns
            .iter()
            .position(|column| column.as_ref() == "LoyaltyTier")
            .expect("a LoyaltyTier column");
        let id = cell(&preview, 0, 0).expect("an id");
        let original = cell(&preview, 0, tier_column);
        let edit = |value: Option<String>| CellEdit {
            path: customers.clone(),
            key: vec![("CustomerId".into(), Some(id.clone().into()))],
            column: "LoyaltyTier".into(),
            value,
        };

        let changed = driver
            .update_cell(&edit(Some("Şampiyon'".into())))
            .await
            .expect("updating a cell");
        assert_eq!(changed, 1);
        let read = only(
            driver
                .query(
                    &format!("SELECT LoyaltyTier FROM sales.Customers WHERE CustomerId = {id}"),
                    None,
                    1,
                )
                .await
                .expect("reading back"),
        );
        assert_eq!(cell(&read, 0, 0).as_deref(), Some("Şampiyon'"));

        driver
            .update_cell(&edit(original))
            .await
            .expect("restoring");

        // A datetime2 key compared through its rendered text.
        let stock_preview = driver
            .preview(&path(&["app", "inventory", "StockLevels"]), 1)
            .await
            .expect("preview StockLevels");
        let store = cell(&stock_preview, 0, 0);
        let item = cell(&stock_preview, 0, 1);
        let updated_at = cell(&stock_preview, 0, 3);
        let restored = driver
            .update_cell(&CellEdit {
                path: path(&["app", "inventory", "StockLevels"]),
                key: vec![
                    ("StoreId".into(), store.map(Into::into)),
                    ("ItemId".into(), item.map(Into::into)),
                ],
                column: "UpdatedAt".into(),
                value: updated_at,
            })
            .await
            .expect("rewriting a datetime2 with its own rendering");
        assert_eq!(restored, 1);

        let missing = driver
            .update_cell(&CellEdit {
                key: vec![("CustomerId".into(), Some("-1".into()))],
                ..edit(Some("x".into()))
            })
            .await
            .expect_err("updating a row that does not exist");
        println!("missing row: {missing:#}");
        let too_many = driver
            .update_cell(&CellEdit {
                path: path(&["app", "inventory", "StockLevels"]),
                key: vec![("StoreId".into(), Some("1".into()))],
                column: "Quantity".into(),
                value: Some("0".into()),
            })
            .await
            .expect_err("updating with a partial key");
        println!("partial key: {too_many:#}");
    });
}

#[test]
#[ignore = "needs a live sql server"]
fn sqlserver_definitions_and_catalog() {
    runtime().block_on(async {
        let driver = sqlserver().await;

        let view = driver
            .definition(&path(&["app", "sales", "StoreDailyRevenue"]))
            .await
            .expect("view definition");
        println!("{view}");
        assert!(view.contains("CREATE VIEW sales.StoreDailyRevenue"));
        let procedure = driver
            .definition(&path(&["app", "sales", "TopItems"]))
            .await
            .expect("procedure definition");
        println!("{procedure}");
        assert!(procedure.contains("CREATE PROCEDURE sales.TopItems"));
        assert!(
            driver
                .definition(&path(&["app", "sales", "NoSuchView"]))
                .await
                .is_err()
        );

        let catalog = driver.catalog(Some("app")).await.expect("catalog");
        println!("catalog: {} entries", catalog.len());
        let has = |kind: ObjectKind, label: &str, parent: Option<&str>| {
            catalog.iter().any(|entry| {
                entry.kind == kind
                    && entry.label.as_ref() == label
                    && entry.parent.as_deref() == parent
            })
        };
        assert!(has(ObjectKind::Schema, "sales", None));
        assert!(has(ObjectKind::Table, "Receipts", Some("sales")));
        assert!(has(ObjectKind::View, "StoreDailyRevenue", Some("sales")));
        assert!(has(ObjectKind::Routine, "TopItems", Some("sales")));
        assert!(has(ObjectKind::Column, "SoldAt", Some("Receipts")));

        let master = driver
            .catalog(Some("master"))
            .await
            .expect("master catalog");
        assert!(
            !master
                .iter()
                .any(|entry| entry.label.as_ref() == "Receipts")
        );
    });
}

#[test]
#[ignore = "needs a live sql server"]
fn sqlserver_cancel() {
    runtime().block_on(async {
        let driver = sqlserver().await;

        let (elapsed, outcome) =
            interrupt(driver.clone(), "WAITFOR DELAY '00:00:30'", None, false).await;
        let error = outcome
            .expect("the task finished")
            .expect_err("the cancelled batch succeeded");
        println!("cancelled after {elapsed:?}: {error:#}");
        assert!(elapsed < Duration::from_secs(5));
        let after = only(
            driver
                .query("SELECT 1", None, 10)
                .await
                .expect("after cancel"),
        );
        assert_eq!(cell(&after, 0, 0).as_deref(), Some("1"));

        let (elapsed, _) = interrupt(driver.clone(), "WAITFOR DELAY '00:00:30'", None, true).await;
        println!("aborted after {elapsed:?}");
        let started = std::time::Instant::now();
        let after = only(
            driver
                .query("SELECT 1", None, 10)
                .await
                .expect("after abort"),
        );
        assert_eq!(cell(&after, 0, 0).as_deref(), Some("1"));
        assert!(started.elapsed() < Duration::from_secs(5));

        // The tree has its own session, so it answers while a query is busy.
        let task = tokio::spawn({
            let driver = driver.clone();
            async move { driver.query("WAITFOR DELAY '00:00:05'", None, 10).await }
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        let started = std::time::Instant::now();
        driver.children(&[]).await.expect("tree during a query");
        assert!(started.elapsed() < Duration::from_secs(2));
        task.await
            .expect("joining the query")
            .expect("the slow batch");
    });
}

/// Runs every statement of `script` the way the query editor would split it.
async fn run_script(driver: &dyn Driver, script: &str, engine: Engine, database: Option<&str>) {
    for range in quarry::statements::split(script, engine) {
        let statement = &script[range];
        driver
            .query(statement, database, 10)
            .await
            .unwrap_or_else(|error| panic!("running {statement}: {error:#}"));
    }
}

#[test]
#[ignore = "needs a live postgres"]
fn postgres_create_script_round_trips() {
    runtime().block_on(async {
        let driver = postgres().await;
        let original = driver
            .create_script(&path(&["app", "shop", "orders"]))
            .await
            .expect("create script");
        println!("{original}");
        assert!(original.starts_with("CREATE TABLE \"shop\".\"orders\" (\n"));
        assert!(original.contains("PRIMARY KEY (id)"));
        assert!(original.contains("FOREIGN KEY (customer_id) REFERENCES shop.customers(id)"));
        assert!(original.contains("CREATE INDEX orders_customer_id_idx ON shop.orders"));

        // Recreated in a scratch schema, the copy must describe itself the same way.
        driver
            .query(
                "DROP SCHEMA IF EXISTS quarry_live_ddl CASCADE; CREATE SCHEMA quarry_live_ddl",
                None,
                10,
            )
            .await
            .expect("creating the scratch schema");
        let moved = original
            .replace("\"shop\".\"orders\"", "\"quarry_live_ddl\".\"orders\"")
            .replace("ON shop.orders ", "ON quarry_live_ddl.orders ");
        run_script(driver.as_ref(), &moved, Engine::Postgres, None).await;
        let copy = driver
            .create_script(&path(&["app", "quarry_live_ddl", "orders"]))
            .await
            .expect("create script of the copy");
        driver
            .query("DROP SCHEMA quarry_live_ddl CASCADE", None, 10)
            .await
            .expect("dropping the scratch schema");
        assert_eq!(copy, moved);

        let view = driver
            .create_script(&path(&["app", "shop", "customer_summary"]))
            .await
            .expect("create script of a view");
        assert!(view.starts_with("CREATE OR REPLACE VIEW"));

        assert_eq!(
            driver
                .select_statement(&path(&["app", "shop", "orders"]), 100)
                .as_deref(),
            Some("SELECT * FROM \"shop\".\"orders\" LIMIT 100;")
        );
        let count = only(
            driver
                .query(
                    &driver
                        .count_statement(&path(&["app", "shop", "orders"]))
                        .expect("count statement"),
                    Some("app"),
                    10,
                )
                .await
                .expect("counting"),
        );
        assert!(cell(&count, 0, 0).is_some());
    });
}

#[test]
#[ignore = "needs a live sql server"]
fn sqlserver_create_script_round_trips() {
    runtime().block_on(async {
        let driver = sqlserver().await;
        let original = driver
            .create_script(&path(&["app", "sales", "Stores"]))
            .await
            .expect("create script");
        println!("{original}");
        assert!(original.starts_with("CREATE TABLE [sales].[Stores] (\n"));
        assert!(original.contains("IDENTITY(1, 1) NOT NULL"));
        assert!(original.contains("PRIMARY KEY CLUSTERED ([StoreId])"));
        assert!(original.contains("REFERENCES [sales].[Regions] ([RegionId])"));

        driver
            .query(
                "IF SCHEMA_ID('quarry_live_ddl') IS NULL EXEC('CREATE SCHEMA quarry_live_ddl')",
                Some("app"),
                10,
            )
            .await
            .expect("creating the scratch schema");
        driver
            .query(
                "DROP TABLE IF EXISTS quarry_live_ddl.Stores",
                Some("app"),
                10,
            )
            .await
            .expect("clearing the scratch table");
        // Constraint names are unique per schema, so they can stay as they are.
        let moved = original.replace("[sales].[Stores]", "[quarry_live_ddl].[Stores]");
        run_script(driver.as_ref(), &moved, Engine::SqlServer, Some("app")).await;
        let copy = driver
            .create_script(&path(&["app", "quarry_live_ddl", "Stores"]))
            .await
            .expect("create script of the copy");
        driver
            .query(
                "DROP TABLE quarry_live_ddl.Stores; DROP SCHEMA quarry_live_ddl;",
                Some("app"),
                10,
            )
            .await
            .expect("dropping the scratch schema");
        assert_eq!(copy, moved);

        assert_eq!(
            driver
                .select_statement(&path(&["app", "sales", "Stores"]), 100)
                .as_deref(),
            Some("SELECT TOP (100) * FROM [sales].[Stores];")
        );
    });
}
