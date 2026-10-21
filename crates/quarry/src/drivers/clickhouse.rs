use crate::driver::{Driver, ObjectKind, ObjectNode, ResultSet};
use anyhow::{Context as _, Result, bail};
use ui::SharedString;

/// ClickHouse is driven over its HTTP interface rather than through the `clickhouse` crate.
///
/// That crate is built around deserialising rows into a known struct, which is the opposite of
/// what a browser needs: here the shape of a result is whatever the user just typed. The HTTP
/// interface answers any query in any format, and `TabSeparatedWithNames` hands back a header
/// row plus already-rendered values — the same reason the Postgres driver uses the simple query
/// protocol.
pub struct ClickHouseDriver {
    endpoint: String,
    database: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

impl ClickHouseDriver {
    pub async fn connect(
        host: &str,
        port: u16,
        database: Option<&str>,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<Self> {
        let driver = Self {
            endpoint: format!("http://{host}:{port}/"),
            database: database.map(str::to_string),
            username: username.map(str::to_string),
            password: password.map(str::to_string),
        };
        // Fail during connect rather than on the first expansion of the tree, so a bad host or
        // password is reported next to the connection instead of inside an empty folder.
        driver
            .run("SELECT 1", 1)
            .await
            .context("checking the ClickHouse connection")?;
        Ok(driver)
    }

    async fn run(&self, sql: &str, row_limit: usize) -> Result<ResultSet> {
        let client = reqwest::Client::new();
        let mut request = client
            .post(&self.endpoint)
            .query(&[("default_format", "TabSeparatedWithNames")])
            .body(format!("{sql}\nFORMAT TabSeparatedWithNames"));
        if let Some(database) = &self.database {
            request = request.query(&[("database", database.as_str())]);
        }
        if let Some(username) = &self.username {
            request = request.header("X-ClickHouse-User", username);
        }
        if let Some(password) = &self.password {
            request = request.header("X-ClickHouse-Key", password);
        }

        let response = request.send().await.context("sending the request")?;
        let status = response.status();
        let body = response.text().await.context("reading the response")?;
        if !status.is_success() {
            // ClickHouse puts a readable message in the body; the status alone says nothing.
            bail!("{}", body.trim());
        }
        Ok(parse_tsv(&body, row_limit))
    }
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
        rows_affected: None,
        notices: Vec::new(),
        truncated,
    }
}

fn unescape(value: &str) -> String {
    if !value.contains('\\') {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
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

#[async_trait::async_trait]
impl Driver for ClickHouseDriver {
    async fn children(&self, path: &[SharedString]) -> Result<Vec<ObjectNode>> {
        match path {
            [] => {
                let result = self
                    .run(
                        "SELECT name FROM system.databases \
                         WHERE name NOT IN ('system', 'INFORMATION_SCHEMA', 'information_schema') \
                         ORDER BY name",
                        10_000,
                    )
                    .await?;
                Ok(single_column(result, ObjectKind::Database, &[], true))
            }
            [database] => {
                let result = self
                    .run(
                        &format!(
                            "SELECT name FROM system.tables WHERE database = {} ORDER BY name",
                            quote_literal(database)
                        ),
                        10_000,
                    )
                    .await?;
                Ok(single_column(
                    result,
                    ObjectKind::Table,
                    &[database.clone()],
                    true,
                ))
            }
            [database, table] => {
                let result = self
                    .run(
                        &format!(
                            "SELECT name, type FROM system.columns \
                             WHERE database = {} AND table = {} ORDER BY position",
                            quote_literal(database),
                            quote_literal(table)
                        ),
                        10_000,
                    )
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

    async fn query(&self, text: &str, row_limit: usize) -> Result<ResultSet> {
        self.run(text, row_limit).await
    }

    async fn preview(&self, path: &[SharedString], row_limit: usize) -> Result<ResultSet> {
        let [database, table, ..] = path else {
            bail!("expected a database-qualified table, got {path:?}");
        };
        self.run(
            &format!(
                "SELECT * FROM {}.{} LIMIT {}",
                quote_ident(database),
                quote_ident(table),
                row_limit + 1
            ),
            row_limit,
        )
        .await
    }
}

fn single_column(
    result: ResultSet,
    kind: ObjectKind,
    prefix: &[SharedString],
    has_children: bool,
) -> Vec<ObjectNode> {
    result
        .rows
        .into_iter()
        .filter_map(|row| row.into_iter().next().flatten())
        .map(|name| {
            let mut path = prefix.to_vec();
            path.push(name.clone());
            ObjectNode {
                path,
                name,
                kind,
                has_children,
                detail: None,
            }
        })
        .collect()
}

fn quote_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "\\`"))
}

fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "\\'"))
}
