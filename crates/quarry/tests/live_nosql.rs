//! ClickHouse, Redis and MongoDB drivers exercised against the servers in
//! `tools/quarry/docker-compose.yml`.
//!
//! These are `#[ignore]`d: they need something listening, so they must never run in a normal
//! `cargo test`. They also change data and put it back, so run them one at a time:
//!
//! ```text
//! cargo test -p quarry --test live_nosql -- --ignored --nocapture --test-threads=1
//! ```

use quarry::driver::{CellEdit, Driver, ObjectKind, ObjectNode, ResultSet};
use quarry::drivers::{clickhouse::ClickHouseDriver, mongodb::MongoDriver, redis::RedisDriver};
use std::time::{Duration, Instant};
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

fn describe(node: &ObjectNode) -> String {
    format!(
        "{} [{:?}]{}{}",
        node.name,
        node.kind,
        node.detail
            .as_ref()
            .map(|detail| format!(" — {detail}"))
            .unwrap_or_default(),
        if node.has_children { " +" } else { "" }
    )
}

async fn walk(driver: &dyn Driver, at: Vec<SharedString>, depth: usize, max_depth: usize) {
    let children = driver
        .children(&at)
        .await
        .unwrap_or_else(|error| panic!("listing {at:?}: {error:#}"));
    for node in children.iter().take(12) {
        println!("{:indent$}{}", "", describe(node), indent = depth * 2);
        if node.has_children && depth < max_depth {
            Box::pin(walk(driver, node.path.clone(), depth + 1, max_depth)).await;
        }
    }
    if children.len() > 12 {
        println!(
            "{:indent$}... {} more",
            "",
            children.len() - 12,
            indent = depth * 2
        );
    }
}

fn cell<'a>(result: &'a ResultSet, row: usize, column: &str) -> Option<&'a str> {
    let index = result
        .columns
        .iter()
        .position(|name| name.as_ref() == column)?;
    result.rows.get(row)?.get(index)?.as_deref()
}

fn single(mut results: Vec<ResultSet>) -> ResultSet {
    assert_eq!(results.len(), 1, "expected exactly one result set");
    results.remove(0)
}

fn edit(
    path: &[SharedString],
    key: &[(&str, Option<&str>)],
    column: &str,
    value: Option<&str>,
) -> CellEdit {
    CellEdit {
        path: path.to_vec(),
        key: key
            .iter()
            .map(|(name, value)| {
                (
                    SharedString::from(name.to_string()),
                    value.map(|value| SharedString::from(value.to_string())),
                )
            })
            .collect(),
        column: SharedString::from(column.to_string()),
        value: value.map(str::to_string),
    }
}

async fn clickhouse_driver() -> ClickHouseDriver {
    ClickHouseDriver::connect("localhost", 18123, Some("app"), Some("app"), Some("app"))
        .await
        .expect("connecting to clickhouse")
}

#[test]
#[ignore = "needs the quarry clickhouse on :18123"]
fn clickhouse_tree_and_catalog() {
    runtime().block_on(async {
        let driver = clickhouse_driver().await;
        println!("--- tree ---");
        walk(&driver, Vec::new(), 0, 1).await;

        let tables = driver.children(&path(&["app"])).await.expect("tables");
        let page_views = tables
            .iter()
            .find(|node| node.name.as_ref() == "page_views")
            .expect("page_views is listed");
        assert_eq!(page_views.kind, ObjectKind::Table);
        assert_eq!(page_views.detail.as_deref(), Some("~1000000 rows"));
        let view = tables
            .iter()
            .find(|node| node.name.as_ref() == "daily_page_stats_mv")
            .expect("the materialized view is listed");
        assert_eq!(view.kind, ObjectKind::View);

        let columns = driver.children(&page_views.path).await.expect("columns");
        assert!(
            columns
                .iter()
                .all(|column| column.kind == ObjectKind::Column)
        );
        assert!(columns.iter().all(|column| column.detail.is_some()));
        println!("page_views columns: {}", columns.len());

        let definition = driver.definition(&view.path).await.expect("definition");
        println!("--- definition ---\n{definition}");
        assert!(definition.contains("MATERIALIZED VIEW"));

        let databases = driver.databases().await.expect("databases");
        println!(
            "databases: {databases:?}, default {:?}",
            driver.default_database()
        );
        assert!(databases.iter().any(|name| name.as_ref() == "app"));
        assert_eq!(driver.default_database().as_deref(), Some("app"));

        let catalog = driver.catalog(None).await.expect("catalog");
        println!("catalog: {} entries", catalog.len());
        assert!(
            catalog
                .iter()
                .any(|entry| entry.label.as_ref() == "page_views"
                    && entry.kind == ObjectKind::Table
                    && entry.parent.as_deref() == Some("app"))
        );
        assert!(catalog.iter().any(|entry| entry.kind == ObjectKind::Column
            && entry.parent.as_deref() == Some("page_views")));
    });
}

#[test]
#[ignore = "needs the quarry clickhouse on :18123"]
fn clickhouse_query() {
    runtime().block_on(async {
        let driver = clickhouse_driver().await;

        let result = single(
            driver
                .query("SELECT 1 AS one, NULL AS nothing, 'a\tb' AS tab;", None, 10)
                .await
                .expect("select"),
        );
        println!("{:?} {:?}", result.columns, result.rows);
        assert_eq!(cell(&result, 0, "one"), Some("1"));
        assert_eq!(cell(&result, 0, "nothing"), None);
        assert_eq!(cell(&result, 0, "tab"), Some("a\tb"));

        let started = Instant::now();
        let result = single(
            driver
                .query("SELECT * FROM page_views", None, 100)
                .await
                .expect("select from page_views"),
        );
        println!(
            "page_views: {} rows, truncated {}, {:?}",
            result.rows.len(),
            result.truncated,
            started.elapsed()
        );
        assert_eq!(result.rows.len(), 100);
        assert!(result.truncated);

        let result = single(
            driver
                .query("SELECT count() FROM numbers(10)", Some("system"), 100)
                .await
                .expect("select in another database"),
        );
        assert_eq!(result.rows.len(), 1);

        driver
            .query("DROP TABLE IF EXISTS quarry_live_probe", None, 100)
            .await
            .expect("drop");
        let created = single(
            driver
                .query(
                    "CREATE TABLE quarry_live_probe (n UInt64) ENGINE = MergeTree ORDER BY n;",
                    None,
                    100,
                )
                .await
                .expect("create"),
        );
        println!("create: {:?}", created.message());
        assert!(created.columns.is_empty());
        assert_eq!(created.rows_affected, Some(0));

        let inserted = single(
            driver
                .query(
                    "INSERT INTO quarry_live_probe SELECT number FROM numbers(5000)",
                    None,
                    10,
                )
                .await
                .expect("insert"),
        );
        println!("insert ... select: {:?}", inserted.message());
        assert_eq!(inserted.rows_affected, Some(5000));

        let inserted = single(
            driver
                .query(
                    "INSERT INTO quarry_live_probe VALUES (1), (2), (3);",
                    None,
                    10,
                )
                .await
                .expect("insert values"),
        );
        assert_eq!(inserted.rows_affected, Some(3));

        let count = single(
            driver
                .query("SELECT count() AS rows FROM quarry_live_probe", None, 10)
                .await
                .expect("count"),
        );
        assert_eq!(cell(&count, 0, "rows"), Some("5003"));

        driver
            .query("DROP TABLE quarry_live_probe", None, 100)
            .await
            .expect("drop");

        let error = driver
            .query("SELECT * FROM no_such_table", None, 10)
            .await
            .expect_err("a missing table is an error");
        println!("error: {error:#}");

        let preview = driver
            .preview(&path(&["app", "countries"]), 5)
            .await
            .expect("preview");
        println!("preview countries: {:?}", preview.columns);
        assert!(preview.rows.len() <= 5);
        assert!(preview.edit_target.is_none());
    });
}

#[test]
#[ignore = "needs the quarry clickhouse on :18123"]
fn clickhouse_cancel() {
    let runtime = runtime();
    runtime.block_on(async {
        let driver = std::sync::Arc::new(clickhouse_driver().await);
        let running = tokio::spawn({
            let driver = driver.clone();
            async move {
                driver
                    .query(
                        "SELECT count() FROM numbers(1000000000000) WHERE sipHash64(number) = 0",
                        None,
                        10,
                    )
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(800)).await;

        let started = Instant::now();
        driver.cancel().await.expect("cancel");
        let outcome = tokio::time::timeout(Duration::from_secs(10), running)
            .await
            .expect("the query stopped after KILL QUERY")
            .expect("the task did not panic");
        println!("cancelled after {:?}: {outcome:?}", started.elapsed());
        assert!(outcome.is_err(), "a killed query reports an error");

        // The other half of cancelling: the caller drops the future. The driver must still work.
        let abandoned = tokio::spawn({
            let driver = driver.clone();
            async move {
                driver
                    .query(
                        "SELECT count() FROM numbers(1000000000000) WHERE sipHash64(number) = 1",
                        None,
                        10,
                    )
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        abandoned.abort();
        driver.cancel().await.expect("cancel after abort");

        let result = single(
            driver
                .query("SELECT 42 AS answer", None, 10)
                .await
                .expect("the driver still works"),
        );
        assert_eq!(cell(&result, 0, "answer"), Some("42"));

        tokio::time::sleep(Duration::from_millis(500)).await;
        let still_running = single(
            driver
                .query(
                    "SELECT groupArray(query) AS running FROM system.processes WHERE query_id LIKE 'quarry-%' \
                     AND query NOT LIKE '%system.processes%'",
                    None,
                    10,
                )
                .await
                .expect("processes"),
        );
        println!("quarry queries still running: {:?}", cell(&still_running, 0, "running"));
        assert_eq!(cell(&still_running, 0, "running"), Some("[]"));
    });
}

async fn redis_driver() -> RedisDriver {
    RedisDriver::connect("localhost", 6380, None, None, None)
        .await
        .expect("connecting to redis")
}

#[test]
#[ignore = "needs the quarry redis on :6380"]
fn redis_tree_and_catalog() {
    runtime().block_on(async {
        let driver = redis_driver().await;
        println!("--- tree ---");
        walk(&driver, Vec::new(), 0, 2).await;

        let databases = driver.children(&[]).await.expect("databases");
        let db0 = databases
            .iter()
            .find(|node| node.name.as_ref() == "db0")
            .expect("db0 is listed");
        assert_eq!(db0.kind, ObjectKind::Database);
        assert!(
            db0.detail
                .as_deref()
                .is_some_and(|detail| detail.ends_with(" keys"))
        );
        assert!(databases.iter().any(|node| node.name.as_ref() == "db1"));

        let top = driver.children(&db0.path).await.expect("top level of db0");
        let user = top
            .iter()
            .find(|node| node.name.as_ref() == "user")
            .expect("user: prefix");
        assert_eq!(user.kind, ObjectKind::Folder);
        assert!(user.has_children);
        let users = driver.children(&user.path).await.expect("user:*");
        let first_user = users.first().expect("at least one user");
        println!("first user: {}", describe(first_user));
        assert_eq!(
            first_user.path.first().map(|name| name.as_ref()),
            Some("db0")
        );
        assert!(users.iter().any(|node| node.kind == ObjectKind::Key));
        assert!(
            top.iter()
                .chain(users.iter())
                .all(|node| node.kind != ObjectKind::Key || node.detail.is_some())
        );
        let with_ttl: Vec<String> = top
            .iter()
            .filter(|node| {
                node.detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains("ttl"))
            })
            .map(describe)
            .collect();
        println!("top-level keys with a TTL: {with_ttl:?}");

        let names = driver.databases().await.expect("databases()");
        println!(
            "databases: {names:?}, default {:?}",
            driver.default_database()
        );
        assert_eq!(driver.default_database().as_deref(), Some("db0"));
        assert!(names.iter().any(|name| name.as_ref() == "db0"));

        let catalog = driver.catalog(Some("db1")).await.expect("catalog of db1");
        println!("db1 catalog: {} keys", catalog.len());
        assert!(!catalog.is_empty());
        assert!(
            catalog
                .iter()
                .all(|entry| entry.kind == ObjectKind::Key && entry.detail.is_some())
        );
    });
}

/// Finds the first key of `kind` anywhere in db0, walking prefixes breadth first.
async fn find_key(driver: &RedisDriver, kind: &str) -> Option<ObjectNode> {
    let mut pending = vec![path(&["db0"])];
    while let Some(at) = pending.pop() {
        for node in driver.children(&at).await.ok()? {
            if node.kind == ObjectKind::Key
                && node
                    .detail
                    .as_deref()
                    .is_some_and(|detail| detail.split(" · ").next() == Some(kind))
            {
                return Some(node);
            }
            if node.has_children {
                pending.insert(0, node.path.clone());
            }
        }
    }
    None
}

#[test]
#[ignore = "needs the quarry redis on :6380"]
fn redis_preview_every_type() {
    runtime().block_on(async {
        let driver = redis_driver().await;
        for (kind, columns) in [
            ("string", vec!["value"]),
            ("list", vec!["index", "value"]),
            ("set", vec!["member"]),
            ("zset", vec!["member", "score"]),
            ("hash", vec!["field", "value"]),
            ("stream", vec!["id", "fields"]),
        ] {
            let node = find_key(&driver, kind)
                .await
                .unwrap_or_else(|| panic!("no {kind} key in db0"));
            let result = driver.preview(&node.path, 5).await.expect("preview");
            println!(
                "{kind} {} -> {:?} {} rows truncated={} notices={:?} editable={}",
                node.path.join(":"),
                result.columns,
                result.rows.len(),
                result.truncated,
                result.notices,
                result.edit_target.is_some()
            );
            for row in result.rows.iter().take(2) {
                println!("    {row:?}");
            }
            let names: Vec<&str> = result.columns.iter().map(|name| name.as_ref()).collect();
            assert_eq!(names, columns);
            assert!(result.rows.len() <= 5);
            assert_eq!(
                result.edit_target.is_some(),
                !matches!(kind, "set" | "stream"),
                "{kind} editability"
            );
        }

        let hyperloglog = driver
            .preview(&path(&["db0", "visitors", "today"]), 5)
            .await
            .expect("preview the HyperLogLog");
        println!("HyperLogLog: {:?}", hyperloglog.rows);
        assert!(
            cell(&hyperloglog, 0, "value")
                .is_some_and(|value| value.starts_with("<HyperLogLog, PFCOUNT="))
        );
        assert!(hyperloglog.edit_target.is_none());

        let geo = driver
            .preview(&path(&["db0", "stores", "geo"]), 5)
            .await
            .expect("preview the geo set");
        assert_eq!(geo.columns.len(), 2);
    });
}

#[test]
#[ignore = "needs the quarry redis on :6380"]
fn redis_query_and_edit() {
    runtime().block_on(async {
        let driver = redis_driver().await;
        let setup = [
            r#"SET "quarry:live:quoted key" "it's \"quoted\"\ttab" EX 600"#,
            "DEL quarry:live:hash quarry:live:list quarry:live:zset",
            "HSET quarry:live:hash name 'Ayşe Yılmaz' city İzmir",
            "RPUSH quarry:live:list a b c",
            "ZADD quarry:live:zset 1 bir 2 iki",
        ];
        for statement in setup {
            let result = single(driver.query(statement, None, 100).await.expect(statement));
            println!("{statement} -> {:?}", result.rows);
        }

        let value = single(
            driver
                .query(r#"GET "quarry:live:quoted key""#, Some("db0"), 10)
                .await
                .expect("get"),
        );
        assert_eq!(cell(&value, 0, "reply"), Some("it's \"quoted\"\ttab"));

        let string_path = path(&["db0", "quarry", "live", "quoted key"]);
        let preview = driver.preview(&string_path, 10).await.expect("preview string");
        println!("string notices: {:?}", preview.notices);
        assert!(preview.notices.iter().any(|notice| notice.starts_with("ttl ")));
        driver
            .update_cell(&edit(&string_path, &[], "value", Some("changed")))
            .await
            .expect("edit string");
        let ttl = single(
            driver
                .query(r#"TTL "quarry:live:quoted key""#, None, 10)
                .await
                .expect("ttl"),
        );
        println!("TTL after edit: {:?}", ttl.rows);
        assert_ne!(cell(&ttl, 0, "reply"), Some("-1"), "KEEPTTL kept the expiry");

        let hash_path = path(&["db0", "quarry", "live", "hash"]);
        driver
            .update_cell(&edit(&hash_path, &[("field", Some("city"))], "value", Some("Ankara")))
            .await
            .expect("edit hash");
        let city = single(driver.query("HGET quarry:live:hash city", None, 10).await.expect("hget"));
        assert_eq!(cell(&city, 0, "reply"), Some("Ankara"));
        assert!(driver
            .update_cell(&edit(&hash_path, &[("field", Some("missing"))], "value", Some("x")))
            .await
            .is_err());

        let list_path = path(&["db0", "quarry", "live", "list"]);
        driver
            .update_cell(&edit(&list_path, &[("index", Some("1"))], "value", Some("B")))
            .await
            .expect("edit list");
        let item = single(driver.query("LINDEX quarry:live:list 1", None, 10).await.expect("lindex"));
        assert_eq!(cell(&item, 0, "reply"), Some("B"));

        let zset_path = path(&["db0", "quarry", "live", "zset"]);
        driver
            .update_cell(&edit(&zset_path, &[("member", Some("iki"))], "score", Some("2.5")))
            .await
            .expect("edit zset score");
        let score = single(driver.query("ZSCORE quarry:live:zset iki", None, 10).await.expect("zscore"));
        assert_eq!(cell(&score, 0, "reply"), Some("2.5"));
        assert!(driver
            .update_cell(&edit(&zset_path, &[("member", Some("üç"))], "score", Some("3")))
            .await
            .is_err());

        // A real seeded key, changed and put back.
        let user = find_key(&driver, "hash").await.expect("a hash in db0");
        let preview = driver.preview(&user.path, 50).await.expect("preview hash");
        let field = cell(&preview, 0, "field").expect("a field").to_string();
        let original = cell(&preview, 0, "value").map(str::to_string);
        driver
            .update_cell(&edit(&user.path, &[("field", Some(&field))], "value", Some("quarry-test")))
            .await
            .expect("edit seeded hash");
        let changed = driver.preview(&user.path, 50).await.expect("preview again");
        let row = changed
            .rows
            .iter()
            .position(|row| row.first().and_then(|value| value.as_deref()) == Some(field.as_str()))
            .expect("field still there");
        assert_eq!(cell(&changed, row, "value"), Some("quarry-test"));
        driver
            .update_cell(&edit(&user.path, &[("field", Some(&field))], "value", original.as_deref()))
            .await
            .expect("restore seeded hash");

        let cleanup = single(
            driver
                .query(
                    r#"DEL "quarry:live:quoted key" quarry:live:hash quarry:live:list quarry:live:zset"#,
                    None,
                    10,
                )
                .await
                .expect("cleanup"),
        );
        assert_eq!(cell(&cleanup, 0, "reply"), Some("4"));

        let db1 = single(driver.query("DBSIZE", Some("db1"), 10).await.expect("dbsize db1"));
        let db0 = single(driver.query("DBSIZE", Some("db0"), 10).await.expect("dbsize db0"));
        println!("DBSIZE db0 {:?}, db1 {:?}", db0.rows, db1.rows);
        assert_ne!(db0.rows, db1.rows);

        let log = single(
            driver
                .query("LRANGE log:app 0 -1", None, 5)
                .await
                .expect("lrange"),
        );
        assert_eq!(log.rows.len(), 5);
        assert!(log.truncated);

        assert!(driver.query(r#"GET "unterminated"#, None, 10).await.is_err());
        assert!(driver.query("select 1", None, 10).await.is_err());

        // A blocking command abandoned midway must not leave db0's shared connection stuck.
        let runtime_handle = tokio::runtime::Handle::current();
        let driver = std::sync::Arc::new(driver);
        let blocked = runtime_handle.spawn({
            let driver = driver.clone();
            async move { driver.query("BLPOP quarry:live:never 0", None, 10).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        blocked.abort();
        let pong = tokio::time::timeout(Duration::from_secs(5), driver.query("PING", None, 10))
            .await
            .expect("PING answered after an abandoned BLPOP")
            .expect("ping");
        assert_eq!(cell(&single(pong), 0, "reply"), Some("PONG"));
    });
}

async fn mongo_driver() -> MongoDriver {
    MongoDriver::connect("localhost", 27018, Some("app"), Some("app"), Some("app"))
        .await
        .expect("connecting to mongodb")
}

#[test]
#[ignore = "needs the quarry mongodb on :27018"]
fn mongodb_tree_and_catalog() {
    runtime().block_on(async {
        let driver = mongo_driver().await;
        println!("--- tree ---");
        walk(&driver, Vec::new(), 0, 2).await;

        let collections = driver.children(&path(&["app"])).await.expect("collections");
        let orders = collections
            .iter()
            .find(|node| node.name.as_ref() == "orders")
            .expect("orders is listed");
        assert_eq!(orders.kind, ObjectKind::Collection);
        assert_eq!(orders.detail.as_deref(), Some("~2000 docs"));
        assert!(orders.has_children);

        let fields = driver.children(&orders.path).await.expect("fields");
        let items = fields
            .iter()
            .find(|node| node.name.as_ref() == "items")
            .expect("items field");
        assert_eq!(items.detail.as_deref(), Some("array"));
        assert!(
            fields
                .iter()
                .any(|node| node.name.as_ref() == "_id"
                    && node.detail.as_deref() == Some("objectId"))
        );

        let databases = driver.databases().await.expect("databases");
        println!("databases: {databases:?}");
        assert!(databases.iter().any(|name| name.as_ref() == "app"));
        assert_eq!(driver.default_database().as_deref(), Some("app"));

        let catalog = driver.catalog(None).await.expect("catalog");
        println!("catalog: {} entries", catalog.len());
        assert!(
            catalog.iter().any(
                |entry| entry.label.as_ref() == "users" && entry.kind == ObjectKind::Collection
            )
        );
        assert!(catalog.iter().any(|entry| entry.label.as_ref() == "email"
            && entry.parent.as_deref() == Some("users")
            && entry.detail.as_deref() == Some("string")));
    });
}

#[test]
#[ignore = "needs the quarry mongodb on :27018"]
fn mongodb_query_and_edit() {
    runtime().block_on(async {
        let driver = mongo_driver().await;

        // More than one batch: the default first batch is 101 documents.
        let events = single(
            driver
                .query(r#"{"find": "events", "filter": {}, "batchSize": 50}"#, None, 300)
                .await
                .expect("find events"),
        );
        println!("events: {} rows, truncated {}", events.rows.len(), events.truncated);
        assert_eq!(events.rows.len(), 300);
        assert!(events.truncated);

        let aggregate = single(
            driver
                .query(
                    r#"{"aggregate": "orders", "pipeline": [{"$group": {"_id": "$status", "count": {"$sum": 1}}}, {"$sort": {"_id": 1}}], "cursor": {}}"#,
                    Some("app"),
                    100,
                )
                .await
                .expect("aggregate"),
        );
        println!("orders by status: {:?}", aggregate.rows);
        assert!(!aggregate.truncated);

        let other_database = single(
            driver
                .query(r#"{"listCollections": 1, "$db": "admin"}"#, Some("app"), 100)
                .await
                .expect("$db wins over the picker"),
        );
        println!("admin collections: {}", other_database.rows.len());

        let inserted = single(
            driver
                .query(
                    r#"{"insert": "quarry_live", "documents": [{"_id": 1, "name": "Çiçek"}, {"_id": 2, "name": "Ilgaz"}]}"#,
                    None,
                    10,
                )
                .await
                .expect("insert"),
        );
        assert_eq!(inserted.rows_affected, Some(2));
        let updated = single(
            driver
                .query(
                    r#"{"update": "quarry_live", "updates": [{"q": {}, "u": {"$set": {"seen": true}}, "multi": true}]}"#,
                    None,
                    10,
                )
                .await
                .expect("update"),
        );
        println!("update: {:?}", updated.message());
        assert_eq!(updated.rows_affected, Some(2));
        assert!(updated.columns.is_empty());
        let duplicate = driver
            .query(
                r#"{"insert": "quarry_live", "documents": [{"_id": 1}]}"#,
                None,
                10,
            )
            .await;
        println!("duplicate insert: {duplicate:?}");
        assert!(duplicate.is_err());

        // Edit a numeric _id document.
        let live_path = path(&["app", "quarry_live"]);
        let preview = driver.preview(&live_path, 10).await.expect("preview quarry_live");
        assert!(preview.edit_target.is_some());
        driver
            .update_cell(&edit(&live_path, &[("_id", Some("2"))], "name", Some("Uludağ")))
            .await
            .expect("edit by numeric _id");
        let renamed = single(
            driver
                .query(r#"{"find": "quarry_live", "filter": {"_id": 2}}"#, None, 10)
                .await
                .expect("find renamed"),
        );
        assert_eq!(cell(&renamed, 0, "name"), Some("Uludağ"));
        let deleted = single(
            driver
                .query(r#"{"delete": "quarry_live", "deletes": [{"q": {}, "limit": 0}]}"#, None, 10)
                .await
                .expect("delete"),
        );
        assert_eq!(deleted.rows_affected, Some(2));
        driver
            .query(r#"{"drop": "quarry_live"}"#, None, 10)
            .await
            .expect("drop");

        // Edit a seeded user (ObjectId _id) and put it back, including a type that must survive.
        let users_path = path(&["app", "users"]);
        let preview = driver.preview(&users_path, 5).await.expect("preview users");
        let target = preview.edit_target.clone().expect("users are editable");
        let id_column = target.key_columns.first().copied().expect("a key column");
        assert_eq!(preview.columns.get(id_column).map(|name| name.as_ref()), Some("_id"));
        let id = cell(&preview, 0, "_id").expect("an _id").to_string();
        let original_points = cell(&preview, 0, "loyaltyPoints").map(str::to_string);
        let original_created = cell(&preview, 0, "createdAt").map(str::to_string);
        println!("editing user {id}: loyaltyPoints {original_points:?}, createdAt {original_created:?}");

        driver
            .update_cell(&edit(&users_path, &[("_id", Some(&id))], "loyaltyPoints", Some("12345")))
            .await
            .expect("edit loyaltyPoints");
        driver
            .update_cell(&edit(&users_path, &[("_id", Some(&id))], "createdAt", original_created.as_deref()))
            .await
            .expect("rewrite createdAt");
        let check = single(
            driver
                .query(
                    &format!(
                        r#"{{"find": "users", "filter": {{"_id": {{"$oid": "{id}"}}, "loyaltyPoints": 12345, "createdAt": {{"$type": "date"}}}}}}"#
                    ),
                    None,
                    10,
                )
                .await
                .expect("verify edit"),
        );
        assert_eq!(check.rows.len(), 1, "number stayed a number and date stayed a date");
        driver
            .update_cell(&edit(&users_path, &[("_id", Some(&id))], "loyaltyPoints", original_points.as_deref()))
            .await
            .expect("restore loyaltyPoints");

        assert!(driver
            .update_cell(&edit(&users_path, &[("_id", Some("000000000000000000000000"))], "name", Some("x")))
            .await
            .is_err());
        assert!(driver
            .update_cell(&edit(&users_path, &[("_id", Some(&id))], "_id", Some("x")))
            .await
            .is_err());
    });
}

#[test]
#[ignore = "needs the quarry mongodb on :27018"]
fn mongodb_create_script_round_trips() {
    runtime().block_on(async {
        let driver = mongo_driver().await;
        let original = driver
            .create_script(&path(&["app", "orders"]))
            .await
            .expect("create script");
        println!("{original}");
        assert!(original.contains("\"createIndexes\": \"orders\""));
        assert!(!original.contains("\"_id_\""));

        // Run in a scratch database, the commands recreate the same collection and indexes.
        let scratch = Some("quarry_live_ddl");
        driver
            .query("{\"dropDatabase\": 1}", scratch, 10)
            .await
            .expect("clearing the scratch database");
        for range in quarry::statements::split(&original, quarry::driver::Engine::MongoDb) {
            driver
                .query(&original[range], scratch, 10)
                .await
                .expect("running the script");
        }
        let copy = driver
            .create_script(&path(&["quarry_live_ddl", "orders"]))
            .await
            .expect("create script of the copy");
        driver
            .query("{\"dropDatabase\": 1}", scratch, 10)
            .await
            .expect("dropping the scratch database");
        assert_eq!(copy, original);

        let count = single(
            driver
                .query(
                    &driver
                        .count_statement(&path(&["app", "orders"]))
                        .expect("count statement"),
                    Some("app"),
                    10,
                )
                .await
                .expect("counting"),
        );
        assert!(cell(&count, 0, "n").is_some());
    });
}

#[test]
#[ignore = "needs the quarry clickhouse on :18123"]
fn clickhouse_create_script() {
    runtime().block_on(async {
        let driver = clickhouse_driver().await;
        let script = driver
            .create_script(&path(&["app", "orders"]))
            .await
            .expect("create script");
        println!("{script}");
        assert!(script.starts_with("CREATE TABLE app.orders"));
        let select = driver
            .select_statement(&path(&["app", "orders"]), 5)
            .expect("select statement");
        let rows = single(driver.query(&select, None, 10).await.expect("selecting"));
        assert_eq!(rows.rows.len(), 5);
    });
}

#[test]
#[ignore = "needs the quarry containers running under docker"]
fn docker_discovery_finds_the_quarry_containers() {
    let servers =
        futures::executor::block_on(quarry::docker::discover()).expect("asking docker");
    for server in &servers {
        println!(
            "{} → {} {}:{} user={:?} db={:?} password={}",
            server.container,
            server.config.engine.label(),
            server.config.host,
            server.config.port,
            server.config.username,
            server.config.database,
            server.password.is_some()
        );
    }
    let mongo = servers
        .iter()
        .find(|server| server.container == "quarry-mongodb")
        .expect("the mongodb container");
    assert_eq!(mongo.config.port, 27018);
    assert_eq!(mongo.config.username.as_deref(), Some("app"));
    assert_eq!(mongo.password.as_deref(), Some("app"));
    let sqlserver = servers
        .iter()
        .find(|server| server.container == "quarry-sqlserver")
        .expect("the sql server container");
    assert_eq!(sqlserver.password.as_deref(), Some("Quarry_Pass123!"));
    assert_eq!(servers.len(), 5);
}
