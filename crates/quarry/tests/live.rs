//! Drivers exercised against real servers.
//!
//! These are `#[ignore]`d: they need something listening, so they must never run in a normal
//! `cargo test`. Run one explicitly when a driver misbehaves, e.g.
//!
//! ```text
//! cargo test -p quarry --test live postgres -- --ignored --nocapture
//! ```
//!
//! They exist because the alternative is guessing from the UI whether a blank tree means a
//! broken driver or a broken panel, and that guess is expensive to get wrong.

use quarry::driver::Driver;
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
        .map(|s| SharedString::from(s.to_string()))
        .collect()
}

async fn walk(driver: &dyn Driver, at: Vec<SharedString>, depth: usize) {
    match driver.children(&at).await {
        Ok(children) => {
            if children.is_empty() {
                println!("{:indent$}(no children)", "", indent = depth * 2);
            }
            for node in children {
                println!(
                    "{:indent$}{} [{:?}]{}",
                    "",
                    node.name,
                    node.kind,
                    node.detail
                        .as_ref()
                        .map(|d| format!(" — {d}"))
                        .unwrap_or_default(),
                    indent = depth * 2
                );
                // Two levels is enough to tell a working tree from a broken one without
                // dumping every column of every table.
                if node.has_children && depth < 2 {
                    Box::pin(walk(driver, node.path.clone(), depth + 1)).await;
                }
            }
        }
        Err(error) => println!("{:indent$}ERROR: {error:#}", "", indent = depth * 2),
    }
}

#[test]
#[ignore = "needs a live postgres"]
fn postgres() {
    runtime().block_on(async {
        let driver = quarry::drivers::postgres::PostgresDriver::connect(
            "localhost",
            54322,
            Some("app"),
            // Overridable so the same test can be run as a user with rights to every database,
            // which is the only way the preview path gets covered on a real server.
            Some(
                std::env::var("QUARRY_PG_USER")
                    .unwrap_or_else(|_| "app".into())
                    .as_str(),
            ),
            std::env::var("QUARRY_PG_PASSWORD").ok().as_deref(),
        )
        .await
        .expect("connecting");
        println!("--- tree ---");
        walk(&driver, Vec::new(), 0).await;
        println!("--- query ---");
        match driver
            .query("SELECT 1 AS one, NULL::text AS nothing", 10)
            .await
        {
            Ok(result) => println!("{:?} {:?}", result.columns, result.rows),
            Err(error) => println!("ERROR: {error:#}"),
        }
    });
}

#[test]
#[ignore = "needs a live redis"]
fn redis() {
    runtime().block_on(async {
        let driver =
            quarry::drivers::redis::RedisDriver::connect("localhost", 6379, None, None, None)
                .await
                .expect("connecting");
        println!("--- tree ---");
        walk(&driver, Vec::new(), 0).await;
        println!("--- query ---");
        match driver.query("INFO server", 10).await {
            Ok(result) => println!("{} rows", result.rows.len()),
            Err(error) => println!("ERROR: {error:#}"),
        }
    });
}

#[test]
#[ignore = "needs a live clickhouse"]
fn clickhouse() {
    runtime().block_on(async {
        let driver = quarry::drivers::clickhouse::ClickHouseDriver::connect(
            "localhost",
            8123,
            None,
            None,
            None,
        )
        .await
        .expect("connecting");
        println!("--- tree ---");
        walk(&driver, Vec::new(), 0).await;
        println!("--- query ---");
        match driver.query("SELECT 1 AS one", 10).await {
            Ok(result) => println!("{:?} {:?}", result.columns, result.rows),
            Err(error) => println!("ERROR: {error:#}"),
        }
    });
}

#[test]
#[ignore = "needs a live postgres"]
fn postgres_preview() {
    runtime().block_on(async {
        let driver = quarry::drivers::postgres::PostgresDriver::connect(
            "localhost",
            54322,
            Some("app"),
            // Overridable so the same test can be run as a user with rights to every database,
            // which is the only way the preview path gets covered on a real server.
            Some(
                std::env::var("QUARRY_PG_USER")
                    .unwrap_or_else(|_| "app".into())
                    .as_str(),
            ),
            std::env::var("QUARRY_PG_PASSWORD").ok().as_deref(),
        )
        .await
        .expect("connecting");
        // Try tables until one can actually be read. Browsing a server usually means seeing
        // databases the connected user has no rights in, so a permission denial is a fact about
        // the environment, not a defect in the driver, and must not be reported as a failure.
        let mut attempted = 0usize;
        let mut denied = 0usize;
        'search: for database in driver.children(&[]).await.expect("databases") {
            for schema in driver.children(&database.path).await.unwrap_or_default() {
                for table in driver.children(&schema.path).await.unwrap_or_default() {
                    attempted += 1;
                    match driver.preview(&table.path, 5).await {
                        Ok(result) => {
                            println!(
                                "{} -> {} columns, {} rows",
                                table.path.join("."),
                                result.columns.len(),
                                result.rows.len()
                            );
                            assert!(
                                !result.columns.is_empty(),
                                "preview returned no columns for {}",
                                table.path.join(".")
                            );
                            return;
                        }
                        Err(error) if format!("{error:#}").contains("permission denied") => {
                            denied += 1;
                        }
                        Err(error) => panic!("preview failed on {}: {error:#}", table.path.join(".")),
                    }
                    if attempted >= 25 {
                        break 'search;
                    }
                }
            }
        }

        println!(
            "preview not exercised: {attempted} tables tried, {denied} denied to this user.              Connect as a user with rights (the cluster's superuser) to cover this path."
        );
    });
}
