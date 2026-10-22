use crate::driver::{
    CatalogEntry, CellEdit, Driver, EditTarget, ObjectKind, ObjectNode, ResultSet,
};
use anyhow::{Context as _, Result, bail};
use futures::TryStreamExt as _;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use tiberius::{
    AuthMethod, Client, ColumnData, Config, EncryptionLevel, FromSql as _, QueryItem,
    time::chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime},
};
use tokio::{net::TcpStream, sync::Mutex};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt as _};
use ui::SharedString;

/// The most names `catalog` hands to the editor, for the same reason as in the Postgres driver.
const CATALOG_LIMIT: usize = 20_000;

type TdsClient = Client<Compat<TcpStream>>;

/// SQL Server is the one engine sqlx cannot reach — MSSQL support was dropped after 0.6 — so it
/// uses tiberius directly. tiberius speaks futures-io rather than tokio-io, which is why the
/// socket is wrapped in tokio-util's compat adapter.
///
/// A TDS connection is a single conversation that cannot interleave requests, and tiberius
/// cannot interrupt a batch once it is sent. So the driver keeps two sessions: one for the
/// tree, previews and edits, and one for queries typed in the editor, so a long query does not
/// freeze the tree. Queries from every editor tab share that one session and run one after the
/// other; in exchange, a batch sees the temp tables and SET options of the batches before it,
/// the way it would in SSMS.
pub struct SqlServerDriver {
    /// Kept so either session can be re-established after it is abandoned mid-request.
    config: Config,
    default_database: String,
    browse: Session,
    query: Session,
}

#[derive(Default)]
struct Session {
    connection: Mutex<Option<Connection>>,
    /// Readable without the lock, which a running query holds: `cancel` uses it to find the
    /// server session to kill.
    process_id: AtomicI32,
    running: AtomicBool,
}

struct Connection {
    client: TdsClient,
    /// The session's current database, so a query only issues `USE` when it has to.
    database: String,
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

        let browse = Session::default();
        let connection = open(&config, &browse)
            .await
            .with_context(|| format!("connecting to sql server at {host}:{port}"))?;
        let default_database = connection.database.clone();
        *browse.connection.lock().await = Some(connection);

        Ok(Self {
            config,
            default_database,
            browse,
            query: Session::default(),
        })
    }

    /// Takes a session for one request, reconnecting it if the last request on it was
    /// abandoned, and switching it to `database` when one is given.
    async fn session<'a>(
        &'a self,
        session: &'a Session,
        database: Option<&str>,
    ) -> Result<SessionGuard<'a>> {
        let mut slot = session.connection.lock().await;
        if slot.is_none() {
            *slot = Some(
                open(&self.config, session)
                    .await
                    .context("reconnecting to sql server")?,
            );
        }
        let mut guard = SessionGuard {
            slot,
            session,
            finished: false,
        };
        session.running.store(true, Ordering::SeqCst);
        if let Some(database) = database {
            if guard.connection()?.database != database {
                let switched = simple_command(
                    &mut guard.connection()?.client,
                    format!("USE {}", quote_ident(database)),
                )
                .await;
                guard.settle(&switched);
                switched.with_context(|| format!("switching to {database}"))?;
                guard.connection()?.database = database.to_string();
                guard.finished = false;
            }
        }
        Ok(guard)
    }

    /// One result set from the browse session, for catalog lookups.
    async fn rows(&self, sql: &str) -> Result<Vec<Vec<Option<SharedString>>>> {
        let mut guard = self.session(&self.browse, None).await?;
        let outcome = run_batch(&mut guard.connection()?.client, sql, usize::MAX).await;
        guard.settle(&outcome);
        let batch = outcome.with_context(|| format!("running: {}", sql.trim()))?;
        Ok(batch
            .sets
            .into_iter()
            .next()
            .map(|set| set.rows)
            .unwrap_or_default())
    }

    async fn edit_target(
        &self,
        path: &[SharedString],
        columns: &[SharedString],
    ) -> Result<Option<EditTarget>> {
        let [database, schema, table] = path else {
            return Ok(None);
        };
        let database_ident = quote_ident(database);
        let object = quote_literal(&three_part_name(database, schema, table));
        let sql = format!(
            "SELECT c.name,
                    CASE WHEN EXISTS (
                        SELECT 1 FROM {database_ident}.sys.indexes i
                        JOIN {database_ident}.sys.index_columns ic
                          ON ic.object_id = i.object_id AND ic.index_id = i.index_id
                        WHERE i.object_id = c.object_id AND i.is_primary_key = 1
                          AND ic.column_id = c.column_id) THEN 1 ELSE 0 END,
                    CASE WHEN c.is_computed = 1 OR c.is_identity = 1 OR c.system_type_id = 189
                         THEN 1 ELSE 0 END
             FROM {database_ident}.sys.columns c
             JOIN {database_ident}.sys.objects o ON o.object_id = c.object_id
             WHERE c.object_id = OBJECT_ID({object}) AND o.type = 'U'"
        );
        let mut key_columns = Vec::new();
        let mut read_only_columns = Vec::new();
        for row in self.rows(&sql).await? {
            let mut values = row.into_iter();
            let Some(name) = values.next().flatten() else {
                continue;
            };
            let is_key = values.next().flatten().as_deref() == Some("1");
            let is_read_only = values.next().flatten().as_deref() == Some("1");
            let Some(index) = columns.iter().position(|column| *column == name) else {
                if is_key {
                    return Ok(None);
                }
                continue;
            };
            if is_key {
                key_columns.push(index);
            }
            if is_read_only {
                read_only_columns.push(index);
            }
        }
        if key_columns.is_empty() {
            return Ok(None);
        }
        Ok(Some(EditTarget {
            path: path.to_vec(),
            key_columns,
            read_only_columns,
        }))
    }
}

async fn open(config: &Config, session: &Session) -> Result<Connection> {
    let tcp = TcpStream::connect(config.get_addr())
        .await
        .context("opening the socket")?;
    tcp.set_nodelay(true).ok();
    let mut client = Client::connect(config.clone(), tcp.compat_write())
        .await
        .context("starting the TDS session")?;

    let row = client
        .simple_query("SELECT DB_NAME(), @@SPID")
        .await
        .context("reading the session")?
        .into_row()
        .await
        .context("reading the session")?
        .context("the server did not describe the session")?;
    let database = row
        .try_get::<&str, _>(0)
        .context("reading the current database")?
        .unwrap_or("master")
        .to_string();
    let process_id = row
        .try_get::<i16, _>(1)
        .context("reading the session id")?
        .unwrap_or_default();
    session
        .process_id
        .store(i32::from(process_id), Ordering::SeqCst);
    Ok(Connection { client, database })
}

/// Exclusive use of a session for one request. Dropped without being settled successfully —
/// the request's future was dropped, or the connection failed — it discards the connection:
/// the server may still be sending the abandoned response, and there is no way to tell it to
/// stop on that connection, so the next request starts on a fresh one.
struct SessionGuard<'a> {
    slot: tokio::sync::MutexGuard<'a, Option<Connection>>,
    session: &'a Session,
    finished: bool,
}

impl SessionGuard<'_> {
    fn connection(&mut self) -> Result<&mut Connection> {
        self.slot
            .as_mut()
            .context("the SQL Server session was closed")
    }

    /// Records whether the conversation is still in a known state after a request. A server
    /// error (bad syntax, a constraint violation) arrives as a token in an otherwise intact
    /// response, and tiberius drains whatever is left of it before the next request; any
    /// other error means the socket or the protocol broke. So do server errors of severity
    /// 20 and up: SQL Server closes the connection after those, as it does after `KILL`.
    fn settle<T>(&mut self, outcome: &Result<T, tiberius::error::Error>) {
        self.finished = match outcome {
            Ok(_) => true,
            Err(tiberius::error::Error::Server(error)) => error.class() < 20,
            Err(_) => false,
        };
    }
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        self.session.running.store(false, Ordering::SeqCst);
        if !self.finished {
            *self.slot = None;
        }
    }
}

/// Runs a batch whose results, if any, are of no interest.
async fn simple_command(client: &mut TdsClient, sql: String) -> Result<(), tiberius::error::Error> {
    client.simple_query(sql).await?.into_results().await?;
    Ok(())
}

struct Batch {
    sets: Vec<ResultSet>,
    /// `@@ROWCOUNT` after the batch: the rows touched by its last statement.
    last_row_count: Option<u64>,
    database: Option<String>,
}

/// Runs one batch and reads every result set it produces.
///
/// tiberius's `simple_query` reports result sets but drops the DONE tokens that carry row
/// counts, and `execute`, which keeps the counts, throws the rows away; nothing exposes both
/// for one request. Running the batch twice is out of the question, so the count comes from a
/// second, separate batch reading `@@ROWCOUNT`, which survives across batches on a session.
/// The limitation that buys: it is the count of the batch's last statement only, so
/// `UPDATE a ...; UPDATE b ...` reports b's count, and a batch that ends in something other
/// than a DML statement (PRINT, an IF) reports zero.
///
/// Rows past `row_limit` are read and discarded rather than abandoned, since stopping early
/// would leave the session mid-response and lose the result sets after the long one.
async fn run_batch(
    client: &mut TdsClient,
    sql: &str,
    row_limit: usize,
) -> Result<Batch, tiberius::error::Error> {
    let mut sets: Vec<ResultSet> = Vec::new();
    let mut stream = client.simple_query(sql).await?;
    while let Some(item) = stream.try_next().await? {
        match item {
            QueryItem::Metadata(metadata) => sets.push(ResultSet {
                columns: metadata
                    .columns()
                    .iter()
                    .map(|column| SharedString::from(column.name().to_string()))
                    .collect(),
                ..ResultSet::default()
            }),
            QueryItem::Row(row) => {
                if let Some(set) = sets.last_mut() {
                    if set.rows.len() < row_limit {
                        set.rows.push(row.into_iter().map(render).collect());
                    } else {
                        set.truncated = true;
                    }
                }
            }
        }
    }
    drop(stream);

    let row = client
        .simple_query("SELECT CAST(@@ROWCOUNT AS bigint), DB_NAME()")
        .await?
        .into_row()
        .await?;
    let (last_row_count, database) = match &row {
        Some(row) => (
            row.try_get::<i64, _>(0)?
                .and_then(|count| u64::try_from(count).ok()),
            row.try_get::<&str, _>(1)?.map(str::to_string),
        ),
        None => (None, None),
    };
    Ok(Batch {
        sets,
        last_row_count,
        database,
    })
}

/// tiberius hands back a typed `ColumnData`, so unlike the Postgres and ClickHouse drivers this
/// one renders each type itself, in the form SQL Server would accept back as a literal — which
/// is what lets a displayed key value be used to address a row in `update_cell`.
fn render(value: ColumnData<'static>) -> Option<SharedString> {
    let text = match &value {
        ColumnData::U8(value) => value.map(|value| value.to_string()),
        ColumnData::I16(value) => value.map(|value| value.to_string()),
        ColumnData::I32(value) => value.map(|value| value.to_string()),
        ColumnData::I64(value) => value.map(|value| value.to_string()),
        ColumnData::F32(value) => value.map(|value| value.to_string()),
        // money and smallmoney arrive as F64 too.
        ColumnData::F64(value) => value.map(|value| value.to_string()),
        ColumnData::Bit(value) => value.map(|value| if value { "1" } else { "0" }.to_string()),
        ColumnData::String(value) => value.as_ref().map(|value| value.to_string()),
        ColumnData::Guid(value) => value.map(|value| value.to_string().to_uppercase()),
        ColumnData::Binary(value) => value.as_ref().map(|bytes| render_binary(bytes)),
        ColumnData::Numeric(value) => value.map(|value| value.to_string()),
        ColumnData::Xml(value) => value.as_ref().map(|value| value.to_string()),
        ColumnData::DateTime(value) => value.and_then(|value| {
            // datetime counts 1/300ths of a second; chrono turns that into nanoseconds like
            // .996666666, while SQL Server itself shows (and parses) it rounded to .997.
            let whole_seconds = NaiveDateTime::from_sql(&ColumnData::DateTime(Some(value)))
                .ok()
                .flatten()?
                .to_string();
            let milliseconds = (value.seconds_fragments() % 300 * 1000 + 150) / 300;
            Some(format!("{}.{milliseconds:03}", whole_seconds.get(..19)?))
        }),
        ColumnData::SmallDateTime(_) | ColumnData::DateTime2(_) => NaiveDateTime::from_sql(&value)
            .ok()
            .flatten()
            .map(|value| value.to_string()),
        ColumnData::Date(_) => NaiveDate::from_sql(&value)
            .ok()
            .flatten()
            .map(|value| value.to_string()),
        ColumnData::Time(_) => NaiveTime::from_sql(&value)
            .ok()
            .flatten()
            .map(|value| value.to_string()),
        ColumnData::DateTimeOffset(_) => DateTime::<FixedOffset>::from_sql(&value)
            .ok()
            .flatten()
            .map(|value| value.to_string()),
    };
    text.map(SharedString::from)
}

/// Hex as SQL Server writes binary literals. Large blobs are summarised instead: the grid is no
/// place for a megabyte of hex.
fn render_binary(bytes: &[u8]) -> String {
    const SHOWN_BYTES: usize = 256;
    if bytes.len() > SHOWN_BYTES {
        return format!("<{} bytes>", bytes.len());
    }
    let mut text = String::with_capacity(2 + bytes.len() * 2);
    text.push_str("0x");
    for byte in bytes {
        text.push_str(&format!("{byte:02X}"));
    }
    text
}

fn first_column(rows: Vec<Vec<Option<SharedString>>>) -> Vec<SharedString> {
    rows.into_iter()
        .filter_map(|row| row.into_iter().next().flatten())
        .collect()
}

/// Routine object types: procedures (SQL and CLR) and scalar, inline and table-valued
/// functions (SQL and CLR).
const ROUTINE_TYPES: &str = "'P', 'PC', 'FN', 'IF', 'TF', 'FS', 'FT'";

#[async_trait::async_trait]
impl Driver for SqlServerDriver {
    async fn children(&self, path: &[SharedString]) -> Result<Vec<ObjectNode>> {
        match path {
            [] => {
                let sql = "SELECT name, CASE WHEN database_id <= 4 THEN 1 ELSE 0 END
                           FROM sys.databases
                           WHERE HAS_DBACCESS(name) = 1 AND state_desc = 'ONLINE'
                           ORDER BY 2, name";
                Ok(self
                    .rows(sql)
                    .await?
                    .into_iter()
                    .filter_map(|row| {
                        let mut values = row.into_iter();
                        let name = values.next().flatten()?;
                        let is_system = values.next().flatten().as_deref() == Some("1");
                        Some(ObjectNode {
                            detail: if name == self.default_database {
                                Some("connected".into())
                            } else if is_system {
                                Some("system".into())
                            } else {
                                None
                            },
                            path: vec![name.clone()],
                            name,
                            kind: ObjectKind::Database,
                            has_children: true,
                        })
                    })
                    .collect())
            }
            [database] => {
                // Schemas from 16384 up are the fixed database roles' (db_owner and friends),
                // which never hold user objects.
                let sql = format!(
                    "SELECT name FROM {}.sys.schemas
                     WHERE schema_id < 16384 AND name NOT IN ('sys', 'INFORMATION_SCHEMA', 'guest')
                     ORDER BY name",
                    quote_ident(database)
                );
                Ok(first_column(self.rows(&sql).await?)
                    .into_iter()
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
                let database_ident = quote_ident(database);
                let sql = format!(
                    "SELECT o.name, RTRIM(o.type),
                            CASE WHEN o.type = 'U' THEN (
                                SELECT SUM(p.rows) FROM {database_ident}.sys.partitions p
                                WHERE p.object_id = o.object_id AND p.index_id IN (0, 1))
                            END
                     FROM {database_ident}.sys.objects o
                     JOIN {database_ident}.sys.schemas s ON s.schema_id = o.schema_id
                     WHERE s.name = {} AND o.is_ms_shipped = 0
                       AND o.type IN ('U', 'V', 'SO', {ROUTINE_TYPES})
                     ORDER BY CASE RTRIM(o.type) WHEN 'U' THEN 0 WHEN 'V' THEN 1 WHEN 'SO' THEN 3
                              ELSE 2 END, o.name",
                    quote_literal(schema)
                );
                Ok(self
                    .rows(&sql)
                    .await?
                    .into_iter()
                    .filter_map(|row| {
                        let mut values = row.into_iter();
                        let name = values.next().flatten()?;
                        let object_type = values.next().flatten()?;
                        let row_count = values.next().flatten();
                        let (kind, detail) = match object_type.as_ref() {
                            "U" => (
                                ObjectKind::Table,
                                row_count.map(|rows| format!("~{rows} rows")),
                            ),
                            "V" => (ObjectKind::View, None),
                            "SO" => (ObjectKind::Sequence, None),
                            "P" | "PC" => (ObjectKind::Routine, Some("procedure".to_string())),
                            _ => (ObjectKind::Routine, Some("function".to_string())),
                        };
                        Some(ObjectNode {
                            path: vec![database.clone(), schema.clone(), name.clone()],
                            name,
                            has_children: matches!(kind, ObjectKind::Table | ObjectKind::View),
                            kind,
                            detail: detail.map(SharedString::from),
                        })
                    })
                    .collect())
            }
            [database, schema, table] => {
                let database_ident = quote_ident(database);
                let object = quote_literal(&three_part_name(database, schema, table));
                // The catalog's names carry the server's collation while the literals carry the
                // database's, and SQL Server refuses to concatenate or union across the two.
                let sql = format!(
                    "SELECT 0, c.column_id, c.name, t.name COLLATE DATABASE_DEFAULT, c.max_length, c.precision, c.scale,
                            c.is_nullable
                     FROM {database_ident}.sys.columns c
                     JOIN {database_ident}.sys.types t ON t.user_type_id = c.user_type_id
                     WHERE c.object_id = OBJECT_ID({object})
                     UNION ALL
                     SELECT 1, CASE WHEN i.is_primary_key = 1 THEN 0 ELSE 1 END, i.name,
                            CASE WHEN i.is_primary_key = 1 THEN 'primary key'
                                 WHEN i.is_unique = 1 THEN 'unique ' + LOWER(i.type_desc) COLLATE DATABASE_DEFAULT
                                 ELSE LOWER(i.type_desc) COLLATE DATABASE_DEFAULT END
                              + ' (' + (
                                SELECT STRING_AGG(c.name COLLATE DATABASE_DEFAULT, ', ') WITHIN GROUP (ORDER BY ic.key_ordinal)
                                FROM {database_ident}.sys.index_columns ic
                                JOIN {database_ident}.sys.columns c
                                  ON c.object_id = ic.object_id AND c.column_id = ic.column_id
                                WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id
                                  AND ic.is_included_column = 0) + ')',
                            NULL, NULL, NULL, NULL
                     FROM {database_ident}.sys.indexes i
                     WHERE i.object_id = OBJECT_ID({object}) AND i.type > 0
                     ORDER BY 1, 2, 3"
                );
                Ok(self
                    .rows(&sql)
                    .await?
                    .into_iter()
                    .filter_map(|row| {
                        let mut values = row.into_iter();
                        let is_index = values.next().flatten()?.as_ref() == "1";
                        // Skips the position, which exists only to order the rows.
                        let name = values.nth(1).flatten()?;
                        let type_name = values.next().flatten();
                        let detail = if is_index {
                            type_name.map(|detail| detail.to_string())
                        } else {
                            let numbers: Vec<Option<i64>> = values
                                .by_ref()
                                .take(3)
                                .map(|value| value.and_then(|value| value.parse().ok()))
                                .collect();
                            let nullable = values.next().flatten();
                            type_name.map(|type_name| {
                                let data_type = column_type(&type_name, &numbers);
                                match nullable.as_deref() {
                                    Some("0") => format!("{data_type} not null"),
                                    _ => data_type,
                                }
                            })
                        };
                        Some(ObjectNode {
                            path: vec![
                                database.clone(),
                                schema.clone(),
                                table.clone(),
                                name.clone(),
                            ],
                            name,
                            kind: if is_index {
                                ObjectKind::Index
                            } else {
                                ObjectKind::Column
                            },
                            has_children: false,
                            detail: detail.map(SharedString::from),
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
        let database = database.unwrap_or(&self.default_database);
        let mut guard = self.session(&self.query, Some(database)).await?;
        let outcome = run_batch(&mut guard.connection()?.client, statement, row_limit).await;
        guard.settle(&outcome);
        let batch = outcome.with_context(|| format!("running: {}", statement.trim()))?;
        // The batch may have switched databases itself; remembering where it ended up keeps
        // the next query from skipping a `USE` it needs.
        if let Some(current) = batch.database {
            guard.connection()?.database = current;
        }
        if batch.sets.is_empty() {
            return Ok(vec![ResultSet {
                rows_affected: batch.last_row_count,
                ..ResultSet::default()
            }]);
        }
        Ok(batch.sets)
    }

    async fn preview(&self, path: &[SharedString], row_limit: usize) -> Result<ResultSet> {
        let [database, schema, table] = path else {
            bail!("expected database, schema and table, got {path:?}");
        };
        // TOP rather than LIMIT: SQL Server has no LIMIT clause. One extra row is what tells
        // the view the result was cut short.
        let sql = format!(
            "SELECT TOP ({}) * FROM {}",
            row_limit.saturating_add(1),
            three_part_name(database, schema, table)
        );
        let mut guard = self.session(&self.browse, None).await?;
        let outcome = run_batch(&mut guard.connection()?.client, &sql, row_limit).await;
        guard.settle(&outcome);
        drop(guard);
        let batch = outcome.with_context(|| format!("running: {sql}"))?;
        let mut result = batch.sets.into_iter().next().unwrap_or_default();
        result.edit_target = self.edit_target(path, &result.columns).await?;
        Ok(result)
    }

    /// tiberius has no way to send an attention signal, so the running batch is stopped by
    /// killing its server session from the browse session. That needs the ALTER ANY
    /// CONNECTION permission; without it the caller dropping the query future still frees the
    /// driver (the session is discarded and reopened), but the server finishes the batch on
    /// its own time.
    async fn cancel(&self) -> Result<()> {
        if !self.query.running.load(Ordering::SeqCst) {
            return Ok(());
        }
        let process_id = self.query.process_id.load(Ordering::SeqCst);
        if process_id <= 0 {
            return Ok(());
        }
        let mut guard = self.session(&self.browse, None).await?;
        let outcome = simple_command(
            &mut guard.connection()?.client,
            format!("KILL {process_id}"),
        )
        .await;
        guard.settle(&outcome);
        outcome.with_context(|| format!("killing session {process_id}"))
    }

    async fn databases(&self) -> Result<Vec<SharedString>> {
        Ok(first_column(
            self.rows(
                "SELECT name FROM sys.databases
                 WHERE HAS_DBACCESS(name) = 1 AND state_desc = 'ONLINE'
                 ORDER BY CASE WHEN database_id <= 4 THEN 1 ELSE 0 END, name",
            )
            .await?,
        ))
    }

    fn default_database(&self) -> Option<SharedString> {
        Some(SharedString::from(self.default_database.clone()))
    }

    async fn catalog(&self, database: Option<&str>) -> Result<Vec<CatalogEntry>> {
        let database_ident = quote_ident(database.unwrap_or(&self.default_database));
        // Columns go last so that, on a catalog big enough to hit the cap, it is columns that
        // get cut rather than whole tables.
        let sql = format!(
            "SELECT TOP ({CATALOG_LIMIT}) kind, label, parent, detail FROM (
                SELECT 0 AS sort, 'schema' AS kind, s.name AS label,
                       CAST(NULL AS sysname) AS parent, CAST(NULL AS sysname) AS detail
                FROM {database_ident}.sys.schemas s
                WHERE s.schema_id < 16384 AND s.name NOT IN ('sys', 'INFORMATION_SCHEMA', 'guest')
                UNION ALL
                SELECT 1, CASE o.type WHEN 'V' THEN 'view' WHEN 'U' THEN 'table' ELSE 'routine' END,
                       o.name, s.name, NULL
                FROM {database_ident}.sys.objects o
                JOIN {database_ident}.sys.schemas s ON s.schema_id = o.schema_id
                WHERE o.is_ms_shipped = 0 AND o.type IN ('U', 'V', {ROUTINE_TYPES})
                UNION ALL
                SELECT 2, 'column', c.name, o.name, t.name
                FROM {database_ident}.sys.columns c
                JOIN {database_ident}.sys.objects o ON o.object_id = c.object_id
                JOIN {database_ident}.sys.types t ON t.user_type_id = c.user_type_id
                WHERE o.is_ms_shipped = 0 AND o.type IN ('U', 'V')
             ) entries ORDER BY sort"
        );
        Ok(self
            .rows(&sql)
            .await?
            .into_iter()
            .filter_map(|row| {
                let mut values = row.into_iter();
                let kind = match values.next().flatten()?.as_ref() {
                    "schema" => ObjectKind::Schema,
                    "table" => ObjectKind::Table,
                    "view" => ObjectKind::View,
                    "routine" => ObjectKind::Routine,
                    _ => ObjectKind::Column,
                };
                Some(CatalogEntry {
                    label: values.next().flatten()?,
                    kind,
                    parent: values.next().flatten(),
                    detail: values.next().flatten(),
                })
            })
            .collect())
    }

    async fn definition(&self, path: &[SharedString]) -> Result<String> {
        let [database, schema, name] = path else {
            bail!("{} has no definition to show", path.join("."));
        };
        // sys.sql_modules through a three-part name rather than OBJECT_DEFINITION, which only
        // looks in the session's current database and would need a `USE` first.
        let sql = format!(
            "SELECT m.definition FROM {}.sys.sql_modules m WHERE m.object_id = OBJECT_ID({})",
            quote_ident(database),
            quote_literal(&three_part_name(database, schema, name))
        );
        first_column(self.rows(&sql).await?)
            .into_iter()
            .next()
            .map(|definition| definition.to_string())
            .with_context(|| {
                format!("{database}.{schema}.{name} has no definition (or it is encrypted)")
            })
    }

    async fn create_script(&self, path: &[SharedString]) -> Result<String> {
        let [database, schema, table] = path else {
            bail!("expected database, schema and table, got {path:?}");
        };
        let database_ident = quote_ident(database);
        let qualified = three_part_name(database, schema, table);
        let object = quote_literal(&qualified);
        let kind = first_column(
            self.rows(&format!(
                "SELECT RTRIM(o.type) FROM {database_ident}.sys.objects o
                 WHERE o.object_id = OBJECT_ID({object})"
            ))
            .await?,
        )
        .into_iter()
        .next()
        .with_context(|| format!("{qualified} was not found"))?;
        if kind.as_ref() != "U" {
            return self.definition(path).await;
        }
        // Two-part in the script: it is meant to be run in a database of the user's choosing,
        // which is what the editor's database picker is for.
        let table_name = format!("{}.{}", quote_ident(schema), quote_ident(table));

        let columns = self
            .rows(&format!(
                "SELECT c.name, t.name, c.max_length, c.precision, c.scale, c.is_nullable,
                        c.is_identity, CAST(ic.seed_value AS nvarchar(100)),
                        CAST(ic.increment_value AS nvarchar(100)), cc.definition, cc.is_persisted,
                        dc.definition
                 FROM {database_ident}.sys.columns c
                 JOIN {database_ident}.sys.types t ON t.user_type_id = c.user_type_id
                 LEFT JOIN {database_ident}.sys.identity_columns ic
                   ON ic.object_id = c.object_id AND ic.column_id = c.column_id
                 LEFT JOIN {database_ident}.sys.computed_columns cc
                   ON cc.object_id = c.object_id AND cc.column_id = c.column_id
                 LEFT JOIN {database_ident}.sys.default_constraints dc
                   ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id
                 WHERE c.object_id = OBJECT_ID({object})
                 ORDER BY c.column_id"
            ))
            .await?;
        let keys = self
            .rows(&format!(
                "SELECT kc.name, kc.type, i.type_desc,
                        STRING_AGG(QUOTENAME(c.name)
                                   + CASE WHEN ic.is_descending_key = 1 THEN ' DESC' ELSE '' END,
                                   ', ') WITHIN GROUP (ORDER BY ic.key_ordinal)
                 FROM {database_ident}.sys.key_constraints kc
                 JOIN {database_ident}.sys.indexes i
                   ON i.object_id = kc.parent_object_id AND i.index_id = kc.unique_index_id
                 JOIN {database_ident}.sys.index_columns ic
                   ON ic.object_id = i.object_id AND ic.index_id = i.index_id
                 JOIN {database_ident}.sys.columns c
                   ON c.object_id = ic.object_id AND c.column_id = ic.column_id
                 WHERE kc.parent_object_id = OBJECT_ID({object})
                 GROUP BY kc.name, kc.type, i.type_desc
                 ORDER BY CASE kc.type WHEN 'PK' THEN 0 ELSE 1 END, kc.name"
            ))
            .await?;
        let checks = self
            .rows(&format!(
                "SELECT name, definition FROM {database_ident}.sys.check_constraints
                 WHERE parent_object_id = OBJECT_ID({object}) ORDER BY name"
            ))
            .await?;
        let foreign_keys = self
            .rows(&format!(
                "SELECT fk.name,
                        STRING_AGG(QUOTENAME(pc.name), ', ')
                          WITHIN GROUP (ORDER BY fkc.constraint_column_id),
                        QUOTENAME(rs.name) + '.' + QUOTENAME(rt.name),
                        STRING_AGG(QUOTENAME(rc.name), ', ')
                          WITHIN GROUP (ORDER BY fkc.constraint_column_id),
                        fk.delete_referential_action_desc, fk.update_referential_action_desc
                 FROM {database_ident}.sys.foreign_keys fk
                 JOIN {database_ident}.sys.foreign_key_columns fkc
                   ON fkc.constraint_object_id = fk.object_id
                 JOIN {database_ident}.sys.columns pc
                   ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id
                 JOIN {database_ident}.sys.columns rc
                   ON rc.object_id = fkc.referenced_object_id
                  AND rc.column_id = fkc.referenced_column_id
                 JOIN {database_ident}.sys.tables rt ON rt.object_id = fk.referenced_object_id
                 JOIN {database_ident}.sys.schemas rs ON rs.schema_id = rt.schema_id
                 WHERE fk.parent_object_id = OBJECT_ID({object})
                 GROUP BY fk.name, rs.name, rt.name, fk.delete_referential_action_desc,
                          fk.update_referential_action_desc
                 ORDER BY fk.name"
            ))
            .await?;
        let indexes = self
            .rows(&format!(
                "SELECT i.name, i.is_unique, i.type_desc,
                        STRING_AGG(CASE WHEN ic.is_included_column = 0 THEN QUOTENAME(c.name)
                                   + CASE WHEN ic.is_descending_key = 1 THEN ' DESC' ELSE '' END
                                   END, ', ') WITHIN GROUP (ORDER BY ic.key_ordinal),
                        STRING_AGG(CASE WHEN ic.is_included_column = 1 THEN QUOTENAME(c.name) END,
                                   ', '),
                        i.filter_definition
                 FROM {database_ident}.sys.indexes i
                 JOIN {database_ident}.sys.index_columns ic
                   ON ic.object_id = i.object_id AND ic.index_id = i.index_id
                 JOIN {database_ident}.sys.columns c
                   ON c.object_id = ic.object_id AND c.column_id = ic.column_id
                 WHERE i.object_id = OBJECT_ID({object}) AND i.type > 0
                   AND i.is_primary_key = 0 AND i.is_unique_constraint = 0
                 GROUP BY i.name, i.is_unique, i.type_desc, i.filter_definition
                 ORDER BY i.name"
            ))
            .await?;

        let mut lines: Vec<String> = columns
            .into_iter()
            .filter_map(|row| {
                let mut values = row.into_iter();
                let name = values.next().flatten()?;
                let type_name = values.next().flatten()?;
                let numbers: Vec<Option<i64>> = values
                    .by_ref()
                    .take(3)
                    .map(|value| value.and_then(|value| value.parse().ok()))
                    .collect();
                let nullable = values.next().flatten().as_deref() == Some("1");
                let identity = values.next().flatten().as_deref() == Some("1");
                let seed = values.next().flatten();
                let increment = values.next().flatten();
                let computed = values.next().flatten();
                let persisted = values.next().flatten().as_deref() == Some("1");
                let default = values.next().flatten();
                let mut line = format!("    {}", quote_ident(&name));
                if let Some(computed) = computed {
                    line.push_str(&format!(" AS {computed}"));
                    if persisted {
                        line.push_str(" PERSISTED");
                    }
                    return Some(line);
                }
                line.push_str(&format!(" {}", column_type(&type_name, &numbers)));
                if identity {
                    line.push_str(&format!(
                        " IDENTITY({}, {})",
                        seed.as_deref().unwrap_or("1"),
                        increment.as_deref().unwrap_or("1")
                    ));
                }
                line.push_str(if nullable { " NULL" } else { " NOT NULL" });
                if let Some(default) = default {
                    line.push_str(&format!(" DEFAULT {default}"));
                }
                Some(line)
            })
            .collect();
        lines.extend(keys.into_iter().filter_map(|row| {
            let mut values = row.into_iter();
            let name = values.next().flatten()?;
            let kind = values.next().flatten()?;
            let index_type = values.next().flatten()?;
            let columns = values.next().flatten()?;
            Some(format!(
                "    CONSTRAINT {} {} {index_type} ({columns})",
                quote_ident(&name),
                if kind.as_ref() == "PK" {
                    "PRIMARY KEY"
                } else {
                    "UNIQUE"
                },
            ))
        }));
        lines.extend(checks.into_iter().filter_map(|row| {
            let mut values = row.into_iter();
            let name = values.next().flatten()?;
            let definition = values.next().flatten()?;
            Some(format!(
                "    CONSTRAINT {} CHECK {definition}",
                quote_ident(&name)
            ))
        }));
        lines.extend(foreign_keys.into_iter().filter_map(|row| {
            let mut values = row.into_iter();
            let name = values.next().flatten()?;
            let columns = values.next().flatten()?;
            let referenced_table = values.next().flatten()?;
            let referenced_columns = values.next().flatten()?;
            let on_delete = values.next().flatten();
            let on_update = values.next().flatten();
            let mut line = format!(
                "    CONSTRAINT {} FOREIGN KEY ({columns}) REFERENCES {referenced_table} ({referenced_columns})",
                quote_ident(&name)
            );
            for (clause, action) in [("DELETE", on_delete), ("UPDATE", on_update)] {
                if let Some(action) = action.filter(|action| action.as_ref() != "NO_ACTION") {
                    line.push_str(&format!(" ON {clause} {}", action.replace('_', " ")));
                }
            }
            Some(line)
        }));

        // GO between statements because that is what the editor splits T-SQL batches on.
        let mut script = format!(
            "CREATE TABLE {table_name} (\n{}\n);\nGO\n",
            lines.join(",\n")
        );
        for row in indexes {
            let mut values = row.into_iter();
            let Some(name) = values.next().flatten() else {
                continue;
            };
            let unique = values.next().flatten().as_deref() == Some("1");
            let index_type = values.next().flatten().unwrap_or_default();
            let key_columns = values.next().flatten();
            let included = values.next().flatten();
            let filter = values.next().flatten();
            let name = quote_ident(&name);
            let statement = if index_type.contains("COLUMNSTORE") {
                // A columnstore index lists its columns as included ones and has no key.
                match (index_type.starts_with("NONCLUSTERED"), included) {
                    (true, Some(included)) => {
                        format!("CREATE {index_type} INDEX {name} ON {table_name} ({included})")
                    }
                    _ => format!("CREATE {index_type} INDEX {name} ON {table_name}"),
                }
            } else {
                let mut statement = format!(
                    "CREATE {}{index_type} INDEX {name} ON {table_name} ({})",
                    if unique { "UNIQUE " } else { "" },
                    key_columns.unwrap_or_default()
                );
                if let Some(included) = included {
                    statement.push_str(&format!(" INCLUDE ({included})"));
                }
                if let Some(filter) = filter {
                    statement.push_str(&format!(" WHERE {filter}"));
                }
                statement
            };
            script.push_str(&format!("\n{statement};\nGO\n"));
        }
        Ok(script)
    }

    fn qualified_name(&self, path: &[SharedString]) -> Option<String> {
        match path {
            [_, schema, table] => Some(format!("{}.{}", quote_ident(schema), quote_ident(table))),
            [_, schema] => Some(quote_ident(schema)),
            [database] => Some(quote_ident(database)),
            _ => None,
        }
    }

    fn select_statement(&self, path: &[SharedString], limit: usize) -> Option<String> {
        let table = self.qualified_name(path).filter(|_| path.len() == 3)?;
        Some(format!("SELECT TOP ({limit}) * FROM {table};"))
    }

    fn count_statement(&self, path: &[SharedString]) -> Option<String> {
        let table = self.qualified_name(path).filter(|_| path.len() == 3)?;
        Some(format!("SELECT COUNT_BIG(*) FROM {table};"))
    }

    async fn update_cell(&self, edit: &CellEdit) -> Result<u64> {
        let [database, schema, table] = edit.path.as_slice() else {
            bail!("expected database, schema and table, got {:?}", edit.path);
        };
        if edit.key.is_empty() {
            bail!("refusing to update {table} without a key");
        }
        // Values go in as N'' literals and are converted to the column's type by the server,
        // which accepts the same text `render` produced for every type it shows as text.
        let condition = edit
            .key
            .iter()
            .map(|(column, value)| match value {
                Some(value) => format!("{} = {}", quote_ident(column), quote_literal(value)),
                None => format!("{} IS NULL", quote_ident(column)),
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        let sql = format!(
            "SET XACT_ABORT ON;
             BEGIN TRANSACTION;
             UPDATE {} SET {} = {} WHERE {condition};
             DECLARE @affected bigint = @@ROWCOUNT;
             IF @affected = 1 COMMIT TRANSACTION; ELSE ROLLBACK TRANSACTION;
             SELECT @affected;",
            three_part_name(database, schema, table),
            quote_ident(&edit.column),
            edit.value
                .as_deref()
                .map_or_else(|| "NULL".to_string(), quote_literal),
        );
        let affected = first_column(self.rows(&sql).await?)
            .into_iter()
            .next()
            .and_then(|count| count.parse::<u64>().ok())
            .context("the server did not report how many rows the update touched")?;
        if affected != 1 {
            bail!("the key matched {affected} rows instead of one; nothing was changed");
        }
        Ok(affected)
    }
}

/// A column's type the way it is written in a CREATE TABLE. `numbers` is max_length, precision
/// and scale from sys.columns; max_length is in bytes, so the n-types are halved.
fn column_type(type_name: &str, numbers: &[Option<i64>]) -> String {
    let number = |index: usize| numbers.get(index).copied().flatten();
    match type_name {
        "varchar" | "char" | "varbinary" | "binary" | "nvarchar" | "nchar" => match number(0) {
            Some(-1) => format!("{type_name}(max)"),
            Some(length) if type_name.starts_with('n') => {
                format!("{type_name}({})", length / 2)
            }
            Some(length) => format!("{type_name}({length})"),
            None => type_name.to_string(),
        },
        "decimal" | "numeric" => match (number(1), number(2)) {
            (Some(precision), Some(scale)) => format!("{type_name}({precision}, {scale})"),
            _ => type_name.to_string(),
        },
        "datetime2" | "time" | "datetimeoffset" => match number(2) {
            Some(scale) => format!("{type_name}({scale})"),
            None => type_name.to_string(),
        },
        _ => type_name.to_string(),
    }
}

fn three_part_name(database: &str, schema: &str, name: &str) -> String {
    format!(
        "{}.{}.{}",
        quote_ident(database),
        quote_ident(schema),
        quote_ident(name)
    )
}

fn quote_ident(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// N-prefixed so non-ASCII text survives: a plain '' literal is converted to the database's
/// code page before it reaches an nvarchar column.
fn quote_literal(value: &str) -> String {
    format!("N'{}'", value.replace('\'', "''"))
}
