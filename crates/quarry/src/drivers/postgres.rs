use crate::driver::{Driver, ObjectKind, ObjectNode, ResultSet};
use anyhow::{Context as _, Result, bail};
use sqlx::{Column as _, Row as _, ValueRef as _, postgres::PgPoolOptions};
use ui::SharedString;

pub struct PostgresDriver {
    /// Connection settings minus the database, so a pool can be opened for any of them.
    options: sqlx::postgres::PgConnectOptions,
    /// The database named in the connection URL. Queries typed in the editor run here.
    default_database: String,
    /// One pool per database. A Postgres connection is bound to a single database and cannot
    /// query across them, so browsing a server's other databases means opening another pool;
    /// without this the tree could only ever show the one database the URL named.
    pools: tokio::sync::Mutex<std::collections::HashMap<String, sqlx::PgPool>>,
}

impl PostgresDriver {
    pub async fn connect(
        host: &str,
        port: u16,
        database: Option<&str>,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<Self> {
        let mut options = sqlx::postgres::PgConnectOptions::new()
            .host(host)
            .port(port);
        if let Some(database) = database {
            options = options.database(database);
        }
        if let Some(username) = username {
            options = options.username(username);
        }
        if let Some(password) = password {
            options = options.password(password);
        }

        let pool = PgPoolOptions::new()
            // A browser issues one query at a time per view; a couple of connections is enough
            // to keep the tree responsive while a long query runs.
            .max_connections(4)
            .connect_with(options.clone())
            .await
            .with_context(|| format!("connecting to postgres at {host}:{port}"))?;

        // Whatever the server resolved to, so the tree can mark it and queries can target it.
        let default_database: String = sqlx::raw_sql("SELECT current_database()")
            .fetch_one(&pool)
            .await
            .context("reading the current database")?
            .try_get_raw(0)
            .ok()
            .and_then(|raw| raw.as_str().ok().map(str::to_string))
            .unwrap_or_else(|| database.unwrap_or("postgres").to_string());

        let mut pools = std::collections::HashMap::new();
        pools.insert(default_database.clone(), pool);
        Ok(Self {
            options,
            default_database,
            pools: tokio::sync::Mutex::new(pools),
        })
    }

    /// Returns the pool for `database`, opening one the first time it is asked for. Pools are
    /// kept because expanding a tree hits the same database repeatedly.
    async fn pool(&self, database: &str) -> Result<sqlx::PgPool> {
        let mut pools = self.pools.lock().await;
        if let Some(pool) = pools.get(database) {
            return Ok(pool.clone());
        }
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(self.options.clone().database(database))
            .await
            .with_context(|| format!("opening {database}"))?;
        pools.insert(database.to_string(), pool.clone());
        Ok(pool)
    }
}

/// Values arrive already rendered by the server because `fetch_all` below goes through the
/// simple query protocol, which answers in text format for every type. That avoids decoding
/// each Postgres type by hand — and avoids depending on uuid/chrono/decimal crates just to show
/// a value — at the cost of not knowing the type beyond its name.
fn cell(row: &sqlx::postgres::PgRow, index: usize) -> Option<SharedString> {
    let raw = row.try_get_raw(index).ok()?;
    if raw.is_null() {
        return None;
    }
    match raw.as_str() {
        Ok(text) => Some(SharedString::from(text.to_string())),
        // bytea and anything else that is not valid UTF-8 in text format.
        Err(_) => Some(SharedString::from("<binary>")),
    }
}

fn to_result_set(rows: Vec<sqlx::postgres::PgRow>, row_limit: usize) -> ResultSet {
    let Some(first) = rows.first() else {
        return ResultSet::default();
    };

    let columns: Vec<SharedString> = first
        .columns()
        .iter()
        .map(|column| SharedString::from(column.name().to_string()))
        .collect();

    let truncated = rows.len() > row_limit;
    let cells = rows
        .iter()
        .take(row_limit)
        .map(|row| (0..columns.len()).map(|i| cell(row, i)).collect())
        .collect();

    ResultSet {
        columns,
        rows: cells,
        rows_affected: None,
        notices: Vec::new(),
        truncated,
    }
}

impl PostgresDriver {
    async fn fetch_in(&self, database: &str, sql: &str, row_limit: usize) -> Result<ResultSet> {
        let pool = self.pool(database).await?;
        // `raw_sql` uses the simple query protocol, which is what makes `cell` above work and
        // also lets the user run several statements separated by semicolons, the way they would
        // in psql. Prepared statements would reject that.
        let rows = sqlx::raw_sql(sql)
            .fetch_all(&pool)
            .await
            .with_context(|| format!("running: {}", sql.trim()))?;
        Ok(to_result_set(rows, row_limit))
    }

    async fn fetch(&self, sql: &str, row_limit: usize) -> Result<ResultSet> {
        self.fetch_in(&self.default_database, sql, row_limit).await
    }
}

#[async_trait::async_trait]
impl Driver for PostgresDriver {
    async fn children(&self, path: &[SharedString]) -> Result<Vec<ObjectNode>> {
        match path {
            // Top level is the server's databases, not the connected one's schemas. A Postgres
            // connection is bound to one database, so starting at schemas made every other
            // database on the server unreachable without adding a second connection.
            [] => {
                let result = self
                    .fetch(
                        "SELECT datname FROM pg_database                          WHERE NOT datistemplate AND datallowconn ORDER BY datname",
                        10_000,
                    )
                    .await?;
                Ok(result
                    .rows
                    .into_iter()
                    .filter_map(|row| row.into_iter().next().flatten())
                    .map(|name| ObjectNode {
                        detail: (name == self.default_database).then(|| "connected".into()),
                        path: vec![name.clone()],
                        name,
                        kind: ObjectKind::Database,
                        has_children: true,
                    })
                    .collect())
            }
            // Schemas the user actually works in; the catalogs are hidden because they bury the
            // interesting ones under hundreds of system tables.
            [database] => {
                let result = self
                    .fetch_in(
                        database,
                        r"SELECT nspname FROM pg_catalog.pg_namespace
                           WHERE nspname NOT LIKE 'pg\_%' AND nspname <> 'information_schema'
                           ORDER BY nspname",
                        10_000,
                    )
                    .await?;
                Ok(result
                    .rows
                    .into_iter()
                    .filter_map(|row| row.into_iter().next().flatten())
                    .map(|name| ObjectNode {
                        path: vec![database.clone(), name.clone()],
                        name,
                        kind: ObjectKind::Schema,
                        has_children: true,
                        detail: None,
                    })
                    .collect())
            }
            [database, schema] => {
                let sql = format!(
                    "SELECT c.relname, c.relkind,                             COALESCE(NULLIF(c.reltuples, -1)::bigint, 0) AS rows                      FROM pg_catalog.pg_class c                      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace                      WHERE n.nspname = {} AND c.relkind IN ('r', 'v', 'm', 'p')                      ORDER BY c.relname",
                    quote_literal(schema)
                );
                let result = self.fetch_in(database, &sql, 10_000).await?;
                Ok(result
                    .rows
                    .into_iter()
                    .filter_map(|row| {
                        let mut values = row.into_iter();
                        let name = values.next().flatten()?;
                        let relkind = values.next().flatten()?;
                        let rows = values.next().flatten();
                        let is_view = matches!(relkind.as_ref(), "v" | "m");
                        Some(ObjectNode {
                            path: vec![database.clone(), schema.clone(), name.clone()],
                            name,
                            kind: if is_view {
                                ObjectKind::View
                            } else {
                                ObjectKind::Table
                            },
                            has_children: true,
                            // reltuples is the planner's estimate, not a count: showing it costs
                            // nothing, while COUNT(*) on every table would make the tree crawl.
                            detail: (!is_view)
                                .then_some(rows)
                                .flatten()
                                .map(|rows| SharedString::from(format!("~{rows} rows"))),
                        })
                    })
                    .collect())
            }
            [database, schema, table] => {
                let sql = format!(
                    "SELECT column_name, data_type, is_nullable                      FROM information_schema.columns                      WHERE table_schema = {} AND table_name = {}                      ORDER BY ordinal_position",
                    quote_literal(schema),
                    quote_literal(table)
                );
                let result = self.fetch_in(database, &sql, 10_000).await?;
                Ok(result
                    .rows
                    .into_iter()
                    .filter_map(|row| {
                        let mut values = row.into_iter();
                        let name = values.next().flatten()?;
                        let data_type = values.next().flatten()?;
                        let nullable = values.next().flatten();
                        let detail = match nullable.as_deref() {
                            Some("NO") => format!("{data_type} not null"),
                            _ => data_type.to_string(),
                        };
                        Some(ObjectNode {
                            path: vec![
                                database.clone(),
                                schema.clone(),
                                table.clone(),
                                name.clone(),
                            ],
                            name,
                            kind: ObjectKind::Column,
                            has_children: false,
                            detail: Some(SharedString::from(detail)),
                        })
                    })
                    .collect())
            }
            _ => Ok(Vec::new()),
        }
    }

    async fn query(&self, text: &str, row_limit: usize) -> Result<ResultSet> {
        self.fetch(text, row_limit).await
    }

    async fn preview(&self, path: &[SharedString], row_limit: usize) -> Result<ResultSet> {
        let [database, schema, table, ..] = path else {
            bail!("expected database, schema and table, got {path:?}");
        };
        // Identifiers are quoted rather than interpolated so mixed-case and reserved names work;
        // they come from the catalog rather than from the user, but quoting keeps it honest.
        let sql = format!(
            "SELECT * FROM {}.{} LIMIT {}",
            quote_ident(schema),
            quote_ident(table),
            // One extra row is what tells the view the result was cut short.
            row_limit + 1
        );
        self.fetch_in(database, &sql, row_limit).await
    }
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
