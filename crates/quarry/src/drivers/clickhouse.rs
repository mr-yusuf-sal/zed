use crate::driver::{CatalogEntry, Driver, ObjectKind, ObjectNode, ResultSet};
use anyhow::{Context as _, Result, bail};
use parking_lot::Mutex;
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use ui::SharedString;

/// ClickHouse is driven over its HTTP interface rather than through the `clickhouse` crate.
///
/// That crate is built around deserialising rows into a known struct, which is the opposite of
/// what a browser needs: here the shape of a result is whatever the user just typed. The HTTP
/// interface answers any query in any format, and `TabSeparatedWithNames` hands back a header
/// row plus already-rendered values — the same reason the Postgres driver uses the simple query
/// protocol.
pub struct ClickHouseDriver {
    client: reqwest::Client,
    endpoint: String,
    database: String,
    username: Option<String>,
    password: Option<String>,
    /// Ids of user queries that may still be running on the server. An id is only removed once
    /// its response has been read in full, so a query whose future was dropped stays listed
    /// until `cancel` has had the chance to kill it.
    running_queries: Mutex<Vec<String>>,
}

/// Generous enough for any tree level or catalog, small enough that a server with an absurd
/// number of tables cannot stall the panel.
const METADATA_ROW_LIMIT: usize = 10_000;

impl ClickHouseDriver {
    pub async fn connect(
        host: &str,
        port: u16,
        database: Option<&str>,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<Self> {
        let mut driver = Self {
            client: reqwest::Client::new(),
            endpoint: format!("http://{host}:{port}/"),
            database: database.unwrap_or_default().to_string(),
            username: username.map(str::to_string),
            password: password.map(str::to_string),
            running_queries: Mutex::new(Vec::new()),
        };
        // Fail during connect rather than on the first expansion of the tree, so a bad host or
        // password is reported next to the connection instead of inside an empty folder. Asking
        // for the current database also settles what "no database" means for this user, which
        // is a per-user server setting rather than always `default`.
        let result = driver
            .execute("SELECT currentDatabase()", None, None, None)
            .await
            .context("checking the ClickHouse connection")?;
        if driver.database.is_empty() {
            driver.database = first_value(&result)
                .map(|name| name.to_string())
                .unwrap_or_else(|| "default".to_string());
        }
        Ok(driver)
    }

    /// Sends one statement. `row_limit` is `Some` only for statements whose result is shown to
    /// the user; it is then also enforced by the server so that a bare `SELECT *` over a huge
    /// table does not stream the whole table to be thrown away here.
    async fn execute(
        &self,
        statement: &str,
        database: Option<&str>,
        row_limit: Option<usize>,
        query_id: Option<&str>,
    ) -> Result<ResultSet> {
        let statement = strip_trailing_semicolons(statement);
        let database = database
            .filter(|database| !database.is_empty())
            .unwrap_or(self.database.as_str());

        let mut parameters: Vec<(&str, String)> = vec![
            ("default_format", "TabSeparatedWithNames".to_string()),
            // Without this a read keeps running on the server after the request is abandoned,
            // which is what dropping the future of a cancelled query does.
            (
                "cancel_http_readonly_queries_on_client_close",
                "1".to_string(),
            ),
        ];
        if !database.is_empty() {
            parameters.push(("database", database.to_string()));
        }
        if let Some(query_id) = query_id {
            parameters.push(("query_id", query_id.to_string()));
        }
        // Only for reads: the result limits are also checked inside subqueries, so applied to an
        // INSERT ... SELECT they could cut the inserted data short without any error.
        if let Some(row_limit) = row_limit
            && is_read(statement)
        {
            parameters.push(("max_result_rows", (row_limit + 1).to_string()));
            parameters.push(("result_overflow_mode", "break".to_string()));
        }

        let mut request = self
            .client
            .post(&self.endpoint)
            .query(&parameters)
            .body(statement.to_string());
        if let Some(username) = &self.username {
            request = request.header("X-ClickHouse-User", username);
        }
        if let Some(password) = &self.password {
            request = request.header("X-ClickHouse-Key", password);
        }

        let mut response = request.send().await.context("sending the request")?;
        let status = response.status();
        let written_rows = response
            .headers()
            .get("X-ClickHouse-Summary")
            .and_then(|summary| summary.to_str().ok())
            .and_then(written_rows);

        // `break` overflow mode stops at a block boundary, so the server can still send tens of
        // thousands of rows past the limit. Reading stops as soon as enough lines have arrived.
        let line_limit = row_limit.map(|row_limit| row_limit + 2);
        let mut body = Vec::new();
        let mut line_count = 0;
        while let Some(chunk) = response.chunk().await.context("reading the response")? {
            body.extend_from_slice(&chunk);
            line_count += chunk.iter().filter(|byte| **byte == b'\n').count();
            if let Some(line_limit) = line_limit
                && status.is_success()
                && line_count >= line_limit
            {
                break;
            }
        }
        let body = String::from_utf8_lossy(&body);
        if !status.is_success() {
            // ClickHouse puts a readable message in the body; the status alone says nothing.
            bail!("{}", body.trim());
        }

        if body.trim().is_empty() {
            return Ok(ResultSet {
                rows_affected: Some(written_rows.unwrap_or(0)),
                ..ResultSet::default()
            });
        }
        Ok(parse_tsv(&body, row_limit.unwrap_or(usize::MAX)))
    }

    async fn metadata(&self, sql: &str) -> Result<ResultSet> {
        self.execute(sql, None, None, None).await
    }
}

fn first_value(result: &ResultSet) -> Option<&SharedString> {
    result.rows.first()?.first()?.as_ref()
}

/// `X-ClickHouse-Summary` is a JSON object whose counters are quoted strings, e.g.
/// `{"read_rows":"0","written_rows":"3",...}`.
fn written_rows(summary: &str) -> Option<u64> {
    let summary: serde_json::Value = serde_json::from_str(summary).ok()?;
    match summary.get("written_rows")? {
        serde_json::Value::String(count) => count.parse().ok(),
        serde_json::Value::Number(count) => count.as_u64(),
        _ => None,
    }
}

/// The HTTP interface accepts exactly one statement and rejects a trailing `;` followed by
/// nothing as an empty second statement on some versions.
fn strip_trailing_semicolons(statement: &str) -> &str {
    let mut statement = statement.trim();
    while let Some(stripped) = statement.strip_suffix(';') {
        statement = stripped.trim_end();
    }
    statement
}

/// Whether the statement only returns rows. Leading comments and parentheses are skipped so
/// that a commented or parenthesised SELECT is still recognised.
fn is_read(statement: &str) -> bool {
    let mut rest = statement;
    loop {
        rest = rest
            .trim_start_matches(|character: char| character.is_whitespace() || character == '(');
        if let Some(comment) = rest.strip_prefix("--") {
            rest = comment.split_once('\n').map_or("", |(_, after)| after);
        } else if let Some(comment) = rest.strip_prefix("/*") {
            rest = comment.split_once("*/").map_or("", |(_, after)| after);
        } else {
            break;
        }
    }
    let keyword: String = rest
        .chars()
        .take_while(|character| character.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    matches!(
        keyword.as_str(),
        "SELECT" | "WITH" | "SHOW" | "DESC" | "DESCRIBE" | "EXPLAIN" | "EXISTS"
    )
}

/// Unique enough to address one query on one server: time plus a process-wide counter, so two
/// queries started in the same nanosecond still differ.
fn next_query_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("quarry-{}-{nanos}-{sequence}", std::process::id())
}

/// `TabSeparatedWithNames` is one header line then one line per row, tab separated, with the
/// usual backslash escapes. Values are already strings, so nothing is decoded beyond unescaping.
fn parse_tsv(body: &str, row_limit: usize) -> ResultSet {
    let mut lines = body.lines();
    let Some(header) = lines.next() else {
        return ResultSet::default();
    };
    if header.is_empty() {
        return ResultSet::default();
    }

    let columns: Vec<SharedString> = header
        .split('\t')
        .map(|name| SharedString::from(unescape(name)))
        .collect();

    let mut rows = Vec::new();
    let mut truncated = false;
    for line in lines {
        if rows.len() >= row_limit {
            truncated = true;
            break;
        }
        rows.push(
            line.split('\t')
                .map(|value| match value {
                    // ClickHouse writes a NULL as \N in this format.
                    "\\N" => None,
                    value => Some(SharedString::from(unescape(value))),
                })
                .collect(),
        );
    }

    ResultSet {
        columns,
        rows,
        truncated,
        ..ResultSet::default()
    }
}

fn unescape(value: &str) -> String {
    if !value.contains('\\') {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(character) = chars.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('0') => out.push('\0'),
            Some('\\') => out.push('\\'),
            Some('\'') => out.push('\''),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn table_kind(engine: Option<&str>) -> ObjectKind {
    match engine {
        Some("View" | "MaterializedView") => ObjectKind::View,
        _ => ObjectKind::Table,
    }
}

#[async_trait::async_trait]
impl Driver for ClickHouseDriver {
    async fn children(&self, path: &[SharedString]) -> Result<Vec<ObjectNode>> {
        match path {
            [] => {
                let result = self
                    .metadata(
                        "SELECT name FROM system.databases \
                         WHERE name NOT IN ('system', 'INFORMATION_SCHEMA', 'information_schema') \
                         ORDER BY name",
                    )
                    .await?;
                Ok(result
                    .rows
                    .into_iter()
                    .filter_map(|row| row.into_iter().next().flatten())
                    .map(|name| ObjectNode {
                        path: vec![name.clone()],
                        name,
                        kind: ObjectKind::Database,
                        has_children: true,
                        detail: None,
                    })
                    .collect())
            }
            [database] => {
                let result = self
                    .metadata(&format!(
                        "SELECT name, engine, total_rows FROM system.tables \
                         WHERE database = {} ORDER BY name LIMIT {METADATA_ROW_LIMIT}",
                        quote_literal(database)
                    ))
                    .await?;
                Ok(result
                    .rows
                    .into_iter()
                    .filter_map(|row| {
                        let mut values = row.into_iter();
                        let name = values.next().flatten()?;
                        let engine = values.next().flatten();
                        let total_rows = values.next().flatten();
                        Some(ObjectNode {
                            path: vec![database.clone(), name.clone()],
                            name,
                            kind: table_kind(engine.as_deref()),
                            has_children: true,
                            detail: total_rows
                                .map(|count| SharedString::from(format!("~{count} rows"))),
                        })
                    })
                    .collect())
            }
            [database, table] => {
                let result = self
                    .metadata(&format!(
                        "SELECT name, type FROM system.columns \
                         WHERE database = {} AND table = {} ORDER BY position",
                        quote_literal(database),
                        quote_literal(table)
                    ))
                    .await?;
                Ok(result
                    .rows
                    .into_iter()
                    .filter_map(|row| {
                        let mut values = row.into_iter();
                        let name = values.next().flatten()?;
                        let data_type = values.next().flatten();
                        Some(ObjectNode {
                            path: vec![database.clone(), table.clone(), name.clone()],
                            name,
                            kind: ObjectKind::Column,
                            has_children: false,
                            detail: data_type,
                        })
                    })
                    .collect())
            }
            _ => Ok(Vec::new()),
        }
    }

    async fn query(
        &self,
        statement: &str,
        database: Option<&str>,
        row_limit: usize,
    ) -> Result<Vec<ResultSet>> {
        let query_id = next_query_id();
        self.running_queries.lock().push(query_id.clone());
        let result = self
            .execute(statement, database, Some(row_limit), Some(&query_id))
            .await;
        self.running_queries.lock().retain(|id| *id != query_id);
        Ok(vec![result?])
    }

    async fn preview(&self, path: &[SharedString], row_limit: usize) -> Result<ResultSet> {
        let [database, table, ..] = path else {
            bail!("expected a database-qualified table, got {path:?}");
        };
        // No edit target: ClickHouse tables have no row identity to address an update by, and
        // an ALTER TABLE ... UPDATE mutation is an asynchronous rewrite of whole parts, not an
        // in-place edit of one cell.
        self.execute(
            &format!(
                "SELECT * FROM {}.{} LIMIT {}",
                quote_ident(database),
                quote_ident(table),
                row_limit + 1
            ),
            None,
            Some(row_limit),
            None,
        )
        .await
    }

    async fn cancel(&self) -> Result<()> {
        let query_ids = std::mem::take(&mut *self.running_queries.lock());
        if query_ids.is_empty() {
            return Ok(());
        }
        let list = query_ids
            .iter()
            .map(|id| quote_literal(id))
            .collect::<Vec<_>>()
            .join(", ");
        self.metadata(&format!("KILL QUERY WHERE query_id IN ({list}) ASYNC"))
            .await
            .context("killing the running query")?;
        Ok(())
    }

    async fn databases(&self) -> Result<Vec<SharedString>> {
        Ok(self
            .children(&[])
            .await?
            .into_iter()
            .map(|node| node.name)
            .collect())
    }

    fn default_database(&self) -> Option<SharedString> {
        Some(SharedString::from(self.database.clone()))
    }

    async fn catalog(&self, database: Option<&str>) -> Result<Vec<CatalogEntry>> {
        let database = database.unwrap_or(self.database.as_str());
        let database_name = SharedString::from(database.to_string());
        let mut entries: Vec<CatalogEntry> = self
            .databases()
            .await?
            .into_iter()
            .map(|name| CatalogEntry {
                label: name,
                kind: ObjectKind::Database,
                parent: None,
                detail: None,
            })
            .collect();

        let tables = self
            .metadata(&format!(
                "SELECT name, engine FROM system.tables WHERE database = {} \
                 ORDER BY name LIMIT {METADATA_ROW_LIMIT}",
                quote_literal(database)
            ))
            .await?;
        entries.extend(tables.rows.into_iter().filter_map(|row| {
            let mut values = row.into_iter();
            let name = values.next().flatten()?;
            let engine = values.next().flatten();
            Some(CatalogEntry {
                label: name,
                kind: table_kind(engine.as_deref()),
                parent: Some(database_name.clone()),
                detail: engine,
            })
        }));

        let columns = self
            .metadata(&format!(
                "SELECT table, name, type FROM system.columns WHERE database = {} \
                 ORDER BY table, position LIMIT {METADATA_ROW_LIMIT}",
                quote_literal(database)
            ))
            .await?;
        entries.extend(columns.rows.into_iter().filter_map(|row| {
            let mut values = row.into_iter();
            let table = values.next().flatten()?;
            let name = values.next().flatten()?;
            let data_type = values.next().flatten();
            Some(CatalogEntry {
                label: name,
                kind: ObjectKind::Column,
                parent: Some(table),
                detail: data_type,
            })
        }));
        Ok(entries)
    }

    async fn definition(&self, path: &[SharedString]) -> Result<String> {
        let [database, table, ..] = path else {
            bail!("expected a database-qualified view, got {path:?}");
        };
        let result = self
            .metadata(&format!(
                "SELECT create_table_query FROM system.tables WHERE database = {} AND name = {}",
                quote_literal(database),
                quote_literal(table)
            ))
            .await?;
        first_value(&result)
            .map(|definition| definition.to_string())
            .with_context(|| format!("{database}.{table} was not found"))
    }

    /// ClickHouse keeps the statement every table was created with, so the definition is the
    /// CREATE script for tables and views alike.
    async fn create_script(&self, path: &[SharedString]) -> Result<String> {
        let definition = self.definition(path).await?;
        Ok(format!("{definition};\n"))
    }

    fn qualified_name(&self, path: &[SharedString]) -> Option<String> {
        match path {
            [database, table, ..] => {
                Some(format!("{}.{}", quote_ident(database), quote_ident(table)))
            }
            [database] => Some(quote_ident(database)),
            [] => None,
        }
    }

    fn select_statement(&self, path: &[SharedString], limit: usize) -> Option<String> {
        let table = self.qualified_name(path).filter(|_| path.len() == 2)?;
        Some(format!("SELECT * FROM {table} LIMIT {limit}"))
    }

    fn count_statement(&self, path: &[SharedString]) -> Option<String> {
        let table = self.qualified_name(path).filter(|_| path.len() == 2)?;
        Some(format!("SELECT count() FROM {table}"))
    }
}

fn quote_ident(name: &str) -> String {
    format!("`{}`", name.replace('\\', "\\\\").replace('`', "\\`"))
}

fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
}
