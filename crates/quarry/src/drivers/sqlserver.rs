use crate::driver::{Driver, ObjectKind, ObjectNode, ResultSet};
use anyhow::{Context as _, Result, bail};
use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::{net::TcpStream, sync::Mutex};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt as _};
use ui::SharedString;

/// SQL Server is the one engine sqlx cannot reach — MSSQL support was dropped after 0.6 — so it
/// uses tiberius directly. tiberius speaks futures-io rather than tokio-io, which is why the
/// socket is wrapped in tokio-util's compat adapter.
///
/// The client holds one connection and is behind a mutex because tiberius needs `&mut self` to
/// run anything: a TDS connection is a single conversation and cannot interleave queries.
pub struct SqlServerDriver {
    client: Mutex<Client<Compat<TcpStream>>>,
}

impl SqlServerDriver {
    pub async fn connect(
        host: &str,
        port: u16,
        database: Option<&str>,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<Self> {
        let mut config = Config::new();
        config.host(host);
        config.port(port);
        if let Some(database) = database {
            config.database(database);
        }
        config.authentication(match (username, password) {
            (Some(user), password) => AuthMethod::sql_server(user, password.unwrap_or_default()),
            // No username means Windows integrated auth, the common case for a local developer
            // instance. It only exists on Windows; elsewhere a username is genuinely required,
            // and saying so is more use than failing later inside the TDS handshake.
            #[cfg(windows)]
            (None, _) => AuthMethod::Integrated,
            #[cfg(not(windows))]
            (None, _) => bail!(
                "SQL Server needs a username in the connection URL on this platform; integrated authentication is Windows-only"
            ),
        });
        // Developer and container instances almost always present a self-signed certificate,
        // and refusing it would make the driver unusable against exactly the servers it is
        // most often pointed at. The connection is still encrypted.
        config.trust_cert();
        config.encryption(EncryptionLevel::Required);

        let tcp = TcpStream::connect(config.get_addr())
            .await
            .with_context(|| format!("connecting to sql server at {host}:{port}"))?;
        tcp.set_nodelay(true).ok();

        let client = Client::connect(config, tcp.compat_write())
            .await
            .context("starting the TDS session")?;
        Ok(Self {
            client: Mutex::new(client),
        })
    }

    async fn fetch(&self, sql: &str, row_limit: usize) -> Result<ResultSet> {
        let mut client = self.client.lock().await;
        let stream = client
            .simple_query(sql.to_string())
            .await
            .with_context(|| format!("running: {}", sql.trim()))?;
        let rows = stream
            .into_first_result()
            .await
            .context("reading the result")?;

        let Some(first) = rows.first() else {
            return Ok(ResultSet::default());
        };
        let columns: Vec<SharedString> = first
            .columns()
            .iter()
            .map(|column| SharedString::from(column.name().to_string()))
            .collect();

        let truncated = rows.len() > row_limit;
        // Rows are consumed rather than borrowed: `ColumnData` owns its payload and rendering
        // it by value avoids copying every string in the result a second time.
        let cells = rows
            .into_iter()
            .take(row_limit)
            .map(|row| row.into_iter().map(render).collect::<Vec<_>>())
            .collect();

        Ok(ResultSet {
            columns,
            rows: cells,
            rows_affected: None,
            notices: Vec::new(),
            truncated,
        })
    }
}

/// tiberius hands back a typed `ColumnData`, so unlike the Postgres and ClickHouse drivers this
/// one has to render each type itself. Anything without a dedicated arm falls through to its
/// Debug form rather than being dropped, so an unusual column still shows something.
fn render(value: tiberius::ColumnData<'static>) -> Option<SharedString> {
    use tiberius::ColumnData;
    let text = match value {
        ColumnData::U8(v) => v.map(|v| v.to_string()),
        ColumnData::I16(v) => v.map(|v| v.to_string()),
        ColumnData::I32(v) => v.map(|v| v.to_string()),
        ColumnData::I64(v) => v.map(|v| v.to_string()),
        ColumnData::F32(v) => v.map(|v| v.to_string()),
        ColumnData::F64(v) => v.map(|v| v.to_string()),
        ColumnData::Bit(v) => v.map(|v| v.to_string()),
        ColumnData::String(v) => v.as_ref().map(|v| v.to_string()),
        ColumnData::Guid(v) => v.map(|v| v.to_string()),
        ColumnData::Binary(v) => v.as_ref().map(|v| format!("<{} bytes>", v.len())),
        ColumnData::Numeric(v) => v.map(|v| v.to_string()),
        ColumnData::Xml(v) => v.as_ref().map(|v| v.to_string()),
        other => Some(format!("{other:?}")),
    };
    text.map(SharedString::from)
}

#[async_trait::async_trait]
impl Driver for SqlServerDriver {
    async fn children(&self, path: &[SharedString]) -> Result<Vec<ObjectNode>> {
        match path {
            [] => {
                let result = self
                    .fetch(
                        "SELECT name FROM sys.schemas \
                         WHERE name NOT IN ('sys', 'INFORMATION_SCHEMA', 'guest', \
                           'db_owner', 'db_accessadmin', 'db_securityadmin', 'db_ddladmin', \
                           'db_backupoperator', 'db_datareader', 'db_datawriter', \
                           'db_denydatareader', 'db_denydatawriter') \
                         ORDER BY name",
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
                    "SELECT t.name, t.type_desc FROM sys.objects t \
                     JOIN sys.schemas s ON s.schema_id = t.schema_id \
                     WHERE s.name = {} AND t.type IN ('U', 'V') ORDER BY t.name",
                    quote_literal(schema)
                );
                let result = self.fetch(&sql, 10_000).await?;
                Ok(result
                    .rows
                    .into_iter()
                    .filter_map(|row| {
                        let mut values = row.into_iter();
                        let name = values.next().flatten()?;
                        let type_desc = values.next().flatten();
                        Some(ObjectNode {
                            path: vec![schema.clone(), name.clone()],
                            name,
                            kind: match type_desc.as_deref() {
                                Some("VIEW") => ObjectKind::View,
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
        // TOP rather than LIMIT: SQL Server has no LIMIT clause.
        let sql = format!(
            "SELECT TOP {} * FROM {}.{}",
            row_limit + 1,
            quote_ident(schema),
            quote_ident(table)
        );
        self.fetch(&sql, row_limit).await
    }
}

fn quote_ident(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
