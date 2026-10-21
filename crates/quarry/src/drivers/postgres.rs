use crate::driver::{Driver, ObjectKind, ObjectNode, ResultSet};
use anyhow::{Context as _, Result, bail};
use sqlx::{Column as _, Row as _, ValueRef as _, postgres::PgPoolOptions};
use ui::SharedString;

pub struct PostgresDriver {
    pool: sqlx::PgPool,
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
            .connect_with(options)
            .await
            .with_context(|| format!("connecting to postgres at {host}:{port}"))?;
        Ok(Self { pool })
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
    async fn fetch(&self, sql: &str, row_limit: usize) -> Result<ResultSet> {
        // `raw_sql` uses the simple query protocol, which is what makes `cell` above work and
        // also lets the user run several statements separated by semicolons, the way they would
        // in psql. Prepared statements would reject that.
        let rows = sqlx::raw_sql(sql)
            .fetch_all(&self.pool)
            .await
            .with_context(|| format!("running: {}", sql.trim()))?;
        Ok(to_result_set(rows, row_limit))
    }
}

#[async_trait::async_trait]
impl Driver for PostgresDriver {
    async fn children(&self, path: &[SharedString]) -> Result<Vec<ObjectNode>> {
        match path {
            // Top level: the schemas a user actually works in. The catalogs are hidden because
            // they bury the interesting ones under hundreds of system tables.
            [] => {
                let result = self
                    .fetch(
                        "SELECT nspname FROM pg_catalog.pg_namespace \
                         WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema' \
                         ORDER BY nspname",
                        10_000,
                    )
                    .await?;
                Ok(result
                    .rows
                    .into_iter()
                    .filter_map(|row| row.into_iter().next().flatten())
                    .map(|name| ObjectNode {
                        path: vec![name.clone()],
                        name,
                        kind: ObjectKind::Schema,
                        has_children: true,
                        detail: None,
                    })
                    .collect())
            }
            [schema] => {
                let sql = format!(
                    "SELECT c.relname, c.relkind \
                     FROM pg_catalog.pg_class c \
                     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = {} AND c.relkind IN ('r', 'v', 'm', 'p') \
                     ORDER BY c.relname",
                    quote_literal(schema)
                );
                let result = self.fetch(&sql, 10_000).await?;
                Ok(result
                    .rows
                    .into_iter()
                    .filter_map(|row| {
                        let mut values = row.into_iter();
                        let name = values.next().flatten()?;
                        let relkind = values.next().flatten()?;
                        Some(ObjectNode {
                            path: vec![schema.clone(), name.clone()],
                            name,
                            kind: match relkind.as_ref() {
                                "v" | "m" => ObjectKind::View,
                                _ => ObjectKind::Table,
                            },
                            has_children: true,
                            detail: None,
                        })
                    })
                    .collect())
            }
            [schema, table] => {
                let sql = format!(
                    "SELECT column_name, data_type, is_nullable \
                     FROM information_schema.columns \
                     WHERE table_schema = {} AND table_name = {} \
                     ORDER BY ordinal_position",
                    quote_literal(schema),
                    quote_literal(table)
                );
                let result = self.fetch(&sql, 10_000).await?;
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
                            path: vec![schema.clone(), table.clone(), name.clone()],
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
        let [schema, table, ..] = path else {
            bail!("expected a schema-qualified table, got {path:?}");
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
        self.fetch(&sql, row_limit).await
    }
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
