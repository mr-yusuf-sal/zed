use crate::driver::{
    CatalogEntry, CellEdit, Driver, EditTarget, ObjectKind, ObjectNode, ResultSet,
};
use anyhow::{Context as _, Result, bail};
use futures::TryStreamExt as _;
use sqlx::{
    Column as _, Connection as _, Either, Executor as _, Row as _, ValueRef as _,
    postgres::{PgConnectOptions, PgConnection, PgPool, PgPoolOptions, PgRow},
};
use std::{collections::HashMap, sync::Arc};
use ui::SharedString;
use util::ResultExt as _;

/// The most names `catalog` hands to the editor. Completion only needs to be useful, and a
/// database with tens of thousands of columns would otherwise cost a large transfer every time
/// the editor asks.
const CATALOG_LIMIT: usize = 20_000;

/// A plan longer than this is cut; no one reads further down one.
const EXPLAIN_ROW_LIMIT: usize = 10_000;

pub struct PostgresDriver {
    /// Connection settings minus the database, so a pool can be opened for any of them.
    options: PgConnectOptions,
    /// The database named in the connection URL. Queries typed in the editor run here.
    default_database: String,
    /// One pool per database. A Postgres connection is bound to a single database and cannot
    /// query across them, so browsing a server's other databases means opening another pool;
    /// without this the tree could only ever show the one database the URL named.
    pools: tokio::sync::Mutex<HashMap<String, PgPool>>,
    /// The connection editor queries run on, one per database, kept for the life of the
    /// driver. Statements arrive one call at a time, so a pooled connection per call would
    /// split `BEGIN`, the statements after it and `COMMIT` across different sessions, and leave
    /// a `SET search_path` on whichever pooled connection it happened to land on. Query tabs
    /// pointed at the same database share its session and take turns.
    sessions: parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Mutex<Option<Session>>>>>,
    /// Backends currently running a user query, so `cancel` knows whom to interrupt. Queries
    /// in different databases run at the same time, hence a list.
    running: Arc<parking_lot::Mutex<Vec<Backend>>>,
}

struct Session {
    connection: PgConnection,
    backend: Backend,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Backend {
    database: String,
    process_id: i32,
}

impl PostgresDriver {
    pub async fn connect(
        host: &str,
        port: u16,
        database: Option<&str>,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<Self> {
        let mut options = PgConnectOptions::new().host(host).port(port);
        if let Some(database) = database {
            options = options.database(database);
        }
        if let Some(username) = username {
            options = options.username(username);
        }
        if let Some(password) = password {
            options = options.password(password);
        }

        let pool = open_pool(options.clone())
            .await
            .with_context(|| format!("connecting to postgres at {host}:{port}"))?;

        // Whatever the server resolved to, so the tree can mark it and queries can target it.
        let default_database = sqlx::query_scalar::<_, String>("SELECT current_database()")
            .fetch_one(&pool)
            .await
            .context("reading the current database")?;

        let mut pools = HashMap::new();
        pools.insert(default_database.clone(), pool);
        Ok(Self {
            options,
            default_database,
            pools: tokio::sync::Mutex::new(pools),
            sessions: parking_lot::Mutex::default(),
            running: Arc::default(),
        })
    }

    /// Returns the pool for `database`, opening one the first time it is asked for. Pools are
    /// kept because expanding a tree hits the same database repeatedly.
    async fn pool(&self, database: &str) -> Result<PgPool> {
        let mut pools = self.pools.lock().await;
        if let Some(pool) = pools.get(database) {
            return Ok(pool.clone());
        }
        let pool = open_pool(self.options.clone().database(database))
            .await
            .with_context(|| format!("opening {database}"))?;
        pools.insert(database.to_string(), pool.clone());
        Ok(pool)
    }

    fn database_or_default<'a>(&'a self, database: Option<&'a str>) -> &'a str {
        database.unwrap_or(&self.default_database)
    }

    /// Catalog lookups for the tree. These are short and never cancelled, so they go straight
    /// through the pool instead of a query session.
    async fn rows(&self, database: &str, sql: &str) -> Result<Vec<Vec<Option<SharedString>>>> {
        let pool = self.pool(database).await?;
        // `raw_sql` uses the simple query protocol, which answers in text for every type; that
        // is what lets `cell` render values without decoding each Postgres type by hand.
        let rows = sqlx::raw_sql(sql)
            .fetch_all(&pool)
            .await
            .with_context(|| format!("running: {}", sql.trim()))?;
        Ok(rows.iter().map(render_row).collect())
    }

    /// Runs one statement on the query session for `database`, opening it first if this is the
    /// first statement there or the last one was abandoned.
    async fn run(
        &self,
        database: &str,
        statement: &str,
        row_limit: usize,
    ) -> Result<Vec<ResultSet>> {
        let slot = self
            .sessions
            .lock()
            .entry(database.to_string())
            .or_default()
            .clone();
        let mut slot = slot.lock_owned().await;
        if slot.is_none() {
            *slot = Some(self.open_session(database).await?);
        }
        let mut running = RunningQuery::new(slot, self.options.clone(), self.running.clone())?;

        let connection = running.connection()?;
        let outcome = match collect(&mut *connection, statement, row_limit).await {
            Ok(mut sets) => {
                describe_empty_result(connection, statement, &mut sets).await;
                Ok(sets)
            }
            Err(error) => Err(error),
        };
        // A server-side error ends the statement cleanly and leaves the session usable (in an
        // aborted transaction if one was open, exactly as in psql). Anything else, a broken
        // socket or a protocol error, means the session has to be replaced.
        running.finished = match &outcome {
            Ok(_) => true,
            Err(error) => matches!(error, sqlx::Error::Database(_)),
        };
        outcome.with_context(|| format!("running: {}", statement.trim()))
    }

    async fn open_session(&self, database: &str) -> Result<Session> {
        let mut connection = PgConnection::connect_with(&self.options.clone().database(database))
            .await
            .with_context(|| format!("connecting to {database}"))?;
        let process_id = sqlx::query_scalar::<_, i32>("SELECT pg_backend_pid()")
            .fetch_one(&mut connection)
            .await
            .context("reading the backend process id")?;
        Ok(Session {
            connection,
            backend: Backend {
                database: database.to_string(),
                process_id,
            },
        })
    }

    /// Previews come from the pool, not a query session: they must neither wait behind a long
    /// query nor run inside a transaction the user left open.
    async fn pooled(&self, database: &str, sql: &str, row_limit: usize) -> Result<Vec<ResultSet>> {
        let pool = self.pool(database).await?;
        let mut connection = pool
            .acquire()
            .await
            .with_context(|| format!("connecting to {database}"))?;
        let mut sets = collect(&mut connection, sql, row_limit)
            .await
            .with_context(|| format!("running: {}", sql.trim()))?;
        describe_empty_result(&mut connection, sql, &mut sets).await;
        Ok(sets)
    }

    async fn edit_target(
        &self,
        path: &[SharedString],
        columns: &[SharedString],
    ) -> Result<Option<EditTarget>> {
        let [database, schema, table] = path else {
            return Ok(None);
        };
        let sql = format!(
            "SELECT a.attname,
                    COALESCE(a.attnum = ANY(pk.indkey), false),
                    a.attgenerated <> '' OR a.attidentity = 'a'
             FROM pg_catalog.pg_attribute a
             JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
             LEFT JOIN pg_catalog.pg_index pk ON pk.indrelid = a.attrelid AND pk.indisprimary
             WHERE a.attrelid = to_regclass({}) AND a.attnum > 0 AND NOT a.attisdropped
               AND c.relkind IN ('r', 'p')",
            quote_literal(&qualified_name(schema, table))
        );
        let mut key_columns = Vec::new();
        let mut read_only_columns = Vec::new();
        for row in self.rows(database, &sql).await? {
            let mut values = row.into_iter();
            let Some(name) = values.next().flatten() else {
                continue;
            };
            let is_key = values.next().flatten().as_deref() == Some("t");
            let is_read_only = values.next().flatten().as_deref() == Some("t");
            let Some(index) = columns.iter().position(|column| *column == name) else {
                // A key column missing from the result means rows cannot be told apart.
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

async fn open_pool(options: PgConnectOptions) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        // The tree, a preview and an edit each hold a connection, and a browser rarely needs
        // more at once. Editor queries and cancelling open connections of their own.
        .max_connections(4)
        // On a table larger than a quarter of shared_buffers, a sequential scan starts wherever
        // another session's scan of the table has got to, so a preview read during someone
        // else's scan begins mid-table and the page after it can repeat or skip rows. Pooled
        // connections only browse, so they always read from the start. A server that rejects
        // the setting is still browsed, just without that guarantee.
        .after_connect(|connection, _| {
            Box::pin(async move {
                connection
                    .execute("SET synchronize_seqscans = off")
                    .await
                    .log_err();
                Ok(())
            })
        })
        .connect_with(options)
        .await
}

/// A query session lent to one statement. Dropped before `finished` is set (the statement's
/// future was dropped, which is how the UI cancels, or the connection failed) it discards the
/// session: the server may still be producing rows for the abandoned statement and the
/// protocol has no way to skip them. The backend is told to stop and the next statement opens
/// a new session. Whatever the old one held is lost with it, an open transaction included;
/// the server rolls that back when the connection goes away.
struct RunningQuery {
    slot: tokio::sync::OwnedMutexGuard<Option<Session>>,
    backend: Backend,
    options: PgConnectOptions,
    running: Arc<parking_lot::Mutex<Vec<Backend>>>,
    finished: bool,
}

impl RunningQuery {
    fn new(
        slot: tokio::sync::OwnedMutexGuard<Option<Session>>,
        options: PgConnectOptions,
        running: Arc<parking_lot::Mutex<Vec<Backend>>>,
    ) -> Result<Self> {
        let backend = slot
            .as_ref()
            .map(|session| session.backend.clone())
            .context("the query session was closed")?;
        running.lock().push(backend.clone());
        Ok(Self {
            slot,
            backend,
            options,
            running,
            finished: false,
        })
    }

    fn connection(&mut self) -> Result<&mut PgConnection> {
        self.slot
            .as_mut()
            .map(|session| &mut session.connection)
            .context("the query session was closed")
    }
}

impl Drop for RunningQuery {
    fn drop(&mut self) {
        let backend = &self.backend;
        self.running.lock().retain(|running| running != backend);
        if self.finished {
            return;
        }
        *self.slot = None;
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let options = self.options.clone();
                let backend = self.backend.clone();
                handle.spawn(async move { cancel_backend(options, backend).await.log_err() });
            }
            Err(_) => log::warn!(
                "quarry: no runtime to cancel postgres backend {}",
                self.backend.process_id
            ),
        }
    }
}

/// Uses a fresh connection rather than the pool: the pool may be exhausted by exactly the
/// queries that need cancelling.
async fn cancel_backend(options: PgConnectOptions, backend: Backend) -> Result<()> {
    let mut connection = PgConnection::connect_with(&options.database(&backend.database))
        .await
        .with_context(|| format!("connecting to {} to cancel a query", backend.database))?;
    sqlx::query("SELECT pg_cancel_backend($1)")
        .bind(backend.process_id)
        .execute(&mut connection)
        .await
        .context("cancelling the query")?;
    connection
        .close()
        .await
        .context("closing the cancel connection")?;
    Ok(())
}

/// Reads every result the statement produces, keeping at most `row_limit` rows of each.
///
/// Rows past the limit are read and thrown away rather than abandoned. Stopping early would
/// leave the connection mid-response, and the only ways out of that (cancelling the statement
/// or dropping the connection) would also end a transaction the user has open, or lose their
/// session settings. The cost is that a huge result crosses the network in full; cancelling is
/// the way out of that.
async fn collect(
    connection: &mut PgConnection,
    statement: &str,
    row_limit: usize,
) -> Result<Vec<ResultSet>, sqlx::Error> {
    let mut sets = Vec::new();
    let mut current: Option<ResultSet> = None;
    let mut stream = sqlx::raw_sql(statement).fetch_many(connection);
    while let Some(item) = stream.try_next().await? {
        match item {
            // Rows of a result arrive first and its completion last, so a completion with no
            // rows before it is a statement that returned none: a write, DDL, or an empty
            // SELECT that `describe_empty_result` sorts out afterwards.
            Either::Left(done) => sets.push(current.take().unwrap_or_else(|| ResultSet {
                rows_affected: Some(done.rows_affected()),
                ..ResultSet::default()
            })),
            Either::Right(row) => {
                let set = current.get_or_insert_with(|| ResultSet {
                    columns: row
                        .columns()
                        .iter()
                        .map(|column| SharedString::from(column.name().to_string()))
                        .collect(),
                    ..ResultSet::default()
                });
                if set.rows.len() < row_limit {
                    set.rows.push(render_row(&row));
                } else {
                    set.truncated = true;
                }
            }
        }
    }
    sets.extend(current.take());
    Ok(sets)
}

/// An empty result still has a shape. Rows are the only place the simple query protocol
/// carries column names, so a SELECT that matched nothing looks exactly like an UPDATE that
/// touched nothing. `describe` asks the server for the statement's result columns directly;
/// it only parses, so the statement is not run a second time.
async fn describe_empty_result(
    connection: &mut PgConnection,
    statement: &str,
    sets: &mut [ResultSet],
) {
    let [set] = sets else {
        return;
    };
    if !set.columns.is_empty() || set.rows_affected != Some(0) {
        return;
    }
    match connection.describe(statement).await {
        Ok(described) if !described.columns().is_empty() => {
            set.columns = described
                .columns()
                .iter()
                .map(|column| SharedString::from(column.name().to_string()))
                .collect();
            set.rows_affected = None;
        }
        Ok(_) => {}
        // describe prepares the statement, which some utility statements refuse. Nothing is
        // lost: the result was empty either way.
        Err(error) => log::debug!("quarry: could not describe {statement:?}: {error}"),
    }
}

/// Values arrive already rendered by the server because every query here goes through the
/// simple query protocol, which answers in text format for every type. That avoids decoding
/// each Postgres type by hand — and avoids depending on uuid/chrono/decimal crates just to show
/// a value — at the cost of not knowing the type beyond its name.
fn cell(row: &PgRow, index: usize) -> Option<SharedString> {
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

fn render_row(row: &PgRow) -> Vec<Option<SharedString>> {
    (0..row.columns().len())
        .map(|index| cell(row, index))
        .collect()
}

fn first_column(rows: Vec<Vec<Option<SharedString>>>) -> Vec<SharedString> {
    rows.into_iter()
        .filter_map(|row| row.into_iter().next().flatten())
        .collect()
}

/// User schemas only; the catalogs are hidden because they bury the interesting ones under
/// hundreds of system tables.
const USER_SCHEMA: &str = r"n.nspname NOT LIKE 'pg\_%' AND n.nspname <> 'information_schema'";

/// How a routine is named in its tree path: its name plus its argument types, because
/// Postgres allows overloading and the name alone would not say which one to open. The same
/// expression is used to find it again in `definition`.
const ROUTINE_SIGNATURE: &str =
    "p.proname || '(' || pg_catalog.oidvectortypes(p.proargtypes) || ')'";

#[async_trait::async_trait]
impl Driver for PostgresDriver {
    async fn children(&self, path: &[SharedString]) -> Result<Vec<ObjectNode>> {
        match path {
            // Top level is the server's databases, not the connected one's schemas. A Postgres
            // connection is bound to one database, so starting at schemas made every other
            // database on the server unreachable without adding a second connection.
            [] => Ok(self
                .databases()
                .await?
                .into_iter()
                .map(|name| ObjectNode {
                    detail: (name == self.default_database).then(|| "connected".into()),
                    path: vec![name.clone()],
                    name,
                    kind: ObjectKind::Database,
                    has_children: true,
                })
                .collect()),
            [database] => {
                let sql = format!(
                    "SELECT n.nspname FROM pg_catalog.pg_namespace n
                     WHERE {USER_SCHEMA} ORDER BY n.nspname"
                );
                Ok(first_column(self.rows(database, &sql).await?)
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
                let schema_literal = quote_literal(schema);
                let sql = format!(
                    "SELECT kind, name, segment, detail FROM (
                        SELECT CASE c.relkind WHEN 'v' THEN 1 WHEN 'm' THEN 2 ELSE 0 END AS sort,
                               CASE WHEN c.relkind IN ('v', 'm') THEN 'view' ELSE 'table' END AS kind,
                               c.relname AS name, c.relname AS segment,
                               CASE WHEN c.relkind IN ('r', 'p')
                                    THEN '~' || COALESCE(NULLIF(c.reltuples, -1)::bigint, 0) || ' rows'
                                    WHEN c.relkind = 'm' THEN 'materialized'
                               END AS detail
                        FROM pg_catalog.pg_class c
                        JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                        WHERE n.nspname = {schema_literal} AND c.relkind IN ('r', 'p', 'v', 'm')
                        UNION ALL
                        SELECT 3, 'routine', p.proname, {ROUTINE_SIGNATURE},
                               CASE p.prokind WHEN 'p' THEN 'procedure (' ELSE '(' END
                                 || pg_catalog.pg_get_function_identity_arguments(p.oid) || ')'
                                 || CASE WHEN p.prokind = 'p' THEN ''
                                         ELSE ' returns ' || pg_catalog.pg_get_function_result(p.oid) END
                        FROM pg_catalog.pg_proc p
                        JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
                        WHERE n.nspname = {schema_literal} AND p.prokind IN ('f', 'p')
                          AND NOT EXISTS (
                              SELECT 1 FROM pg_catalog.pg_depend d
                              WHERE d.classid = 'pg_catalog.pg_proc'::regclass
                                AND d.objid = p.oid AND d.deptype = 'e')
                        UNION ALL
                        SELECT 4, 'sequence', c.relname, c.relname, NULL
                        FROM pg_catalog.pg_class c
                        JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                        WHERE n.nspname = {schema_literal} AND c.relkind = 'S'
                     ) objects ORDER BY sort, name, segment"
                );
                Ok(self
                    .rows(database, &sql)
                    .await?
                    .into_iter()
                    .filter_map(|row| {
                        let mut values = row.into_iter();
                        let kind = values.next().flatten()?;
                        let name = values.next().flatten()?;
                        let segment = values.next().flatten()?;
                        // reltuples is the planner's estimate, not a count: showing it costs
                        // nothing, while COUNT(*) on every table would make the tree crawl.
                        let detail = values.next().flatten();
                        let kind = match kind.as_ref() {
                            "table" => ObjectKind::Table,
                            "view" => ObjectKind::View,
                            "sequence" => ObjectKind::Sequence,
                            _ => ObjectKind::Routine,
                        };
                        Some(ObjectNode {
                            path: vec![database.clone(), schema.clone(), segment],
                            name,
                            has_children: matches!(kind, ObjectKind::Table | ObjectKind::View),
                            kind,
                            detail,
                        })
                    })
                    .collect())
            }
            [database, schema, table] => {
                let relation = quote_literal(&qualified_name(schema, table));
                let sql = format!(
                    "SELECT 0 AS sort, a.attnum AS position, a.attname AS name,
                            pg_catalog.format_type(a.atttypid, a.atttypmod)
                              || CASE WHEN a.attnotnull THEN ' not null' ELSE '' END AS detail
                     FROM pg_catalog.pg_attribute a
                     WHERE a.attrelid = to_regclass({relation}) AND a.attnum > 0 AND NOT a.attisdropped
                     UNION ALL
                     SELECT 1, CASE WHEN ix.indisprimary THEN 0 ELSE 1 END, i.relname,
                            CASE WHEN ix.indisprimary THEN 'primary key '
                                 WHEN ix.indisunique THEN 'unique '
                                 ELSE '' END
                              || substring(pg_catalog.pg_get_indexdef(ix.indexrelid) from ' USING (.*)$')
                     FROM pg_catalog.pg_index ix
                     JOIN pg_catalog.pg_class i ON i.oid = ix.indexrelid
                     WHERE ix.indrelid = to_regclass({relation})
                     ORDER BY sort, position, name"
                );
                Ok(self
                    .rows(database, &sql)
                    .await?
                    .into_iter()
                    .filter_map(|row| {
                        let mut values = row.into_iter();
                        let sort = values.next().flatten()?;
                        // Skips the position, which exists only to order the rows.
                        let name = values.nth(1).flatten()?;
                        let detail = values.next().flatten();
                        Some(ObjectNode {
                            path: vec![
                                database.clone(),
                                schema.clone(),
                                table.clone(),
                                name.clone(),
                            ],
                            name,
                            kind: if sort.as_ref() == "0" {
                                ObjectKind::Column
                            } else {
                                ObjectKind::Index
                            },
                            has_children: false,
                            detail,
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
        self.run(self.database_or_default(database), statement, row_limit)
            .await
    }

    async fn explain(
        &self,
        statement: &str,
        database: Option<&str>,
        analyze: bool,
    ) -> Result<Vec<ResultSet>> {
        let options = if analyze { "(ANALYZE, BUFFERS) " } else { "" };
        let statement = statement.trim().trim_end_matches(';');
        // On the query session, so the plan sees the session's search_path and open
        // transaction just as running the statement there would.
        self.run(
            self.database_or_default(database),
            &format!("EXPLAIN {options}{statement}"),
            EXPLAIN_ROW_LIMIT,
        )
        .await
    }

    async fn preview_page(
        &self,
        path: &[SharedString],
        row_limit: usize,
        offset: usize,
    ) -> Result<ResultSet> {
        let [database, schema, table] = path else {
            bail!("expected database, schema and table, got {path:?}");
        };
        // Identifiers are quoted rather than interpolated so mixed-case and reserved names work;
        // they come from the catalog rather than from the user, but quoting keeps it honest.
        let sql = format!(
            "SELECT * FROM {} LIMIT {} OFFSET {offset}",
            qualified_name(schema, table),
            // One extra row is what tells the view the result was cut short.
            row_limit.saturating_add(1)
        );
        let mut result = self
            .pooled(database, &sql, row_limit)
            .await?
            .into_iter()
            .next()
            .unwrap_or_default();
        result.rows_affected = None;
        result.edit_target = self.edit_target(path, &result.columns).await?;
        Ok(result)
    }

    async fn cancel(&self) -> Result<()> {
        let backends = self.running.lock().clone();
        for backend in backends {
            cancel_backend(self.options.clone(), backend).await?;
        }
        Ok(())
    }

    async fn databases(&self) -> Result<Vec<SharedString>> {
        Ok(first_column(
            self.rows(
                &self.default_database,
                "SELECT datname FROM pg_catalog.pg_database
                 WHERE NOT datistemplate AND datallowconn ORDER BY datname",
            )
            .await?,
        ))
    }

    fn default_database(&self) -> Option<SharedString> {
        Some(SharedString::from(self.default_database.clone()))
    }

    async fn catalog(&self, database: Option<&str>) -> Result<Vec<CatalogEntry>> {
        // Columns go last so that, on a catalog big enough to hit the cap, it is columns that
        // get cut rather than whole tables.
        let sql = format!(
            "SELECT kind, label, parent, detail FROM (
                SELECT 0 AS sort, 'schema' AS kind, n.nspname AS label, NULL AS parent, NULL AS detail
                FROM pg_catalog.pg_namespace n WHERE {USER_SCHEMA}
                UNION ALL
                SELECT 1, CASE WHEN c.relkind IN ('v', 'm') THEN 'view' ELSE 'table' END,
                       c.relname, n.nspname, NULL
                FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                WHERE {USER_SCHEMA} AND c.relkind IN ('r', 'p', 'v', 'm', 'f')
                UNION ALL
                SELECT DISTINCT 2, 'routine', p.proname, n.nspname, NULL
                FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
                WHERE {USER_SCHEMA} AND p.prokind IN ('f', 'p')
                UNION ALL
                SELECT 3, 'column', a.attname, c.relname, pg_catalog.format_type(a.atttypid, a.atttypmod)
                FROM pg_catalog.pg_attribute a
                JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
                JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                WHERE {USER_SCHEMA} AND c.relkind IN ('r', 'p', 'v', 'm', 'f')
                  AND a.attnum > 0 AND NOT a.attisdropped
             ) entries ORDER BY sort LIMIT {CATALOG_LIMIT}"
        );
        Ok(self
            .rows(self.database_or_default(database), &sql)
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
        let qualified = qualified_name(schema, name);
        let sql = format!(
            "SELECT CASE c.relkind WHEN 'm' THEN 'CREATE MATERIALIZED VIEW '
                                   ELSE 'CREATE OR REPLACE VIEW ' END
                    || {qualified_literal} || ' AS' || chr(10)
                    || ltrim(pg_catalog.pg_get_viewdef(c.oid, true))
             FROM pg_catalog.pg_class c
             WHERE c.oid = to_regclass({qualified_literal}) AND c.relkind IN ('v', 'm')
             UNION ALL
             SELECT pg_catalog.pg_get_functiondef(p.oid)
             FROM pg_catalog.pg_proc p
             JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
             WHERE n.nspname = {schema_literal} AND {ROUTINE_SIGNATURE} = {name_literal}
               AND p.prokind IN ('f', 'p')",
            qualified_literal = quote_literal(&qualified),
            schema_literal = quote_literal(schema),
            name_literal = quote_literal(name),
        );
        first_column(self.rows(database, &sql).await?)
            .into_iter()
            .next()
            .map(|definition| definition.to_string())
            .with_context(|| format!("{qualified} is not a view or routine"))
    }

    async fn create_script(&self, path: &[SharedString]) -> Result<String> {
        let [database, schema, table] = path else {
            bail!("expected database, schema and table, got {path:?}");
        };
        let qualified = qualified_name(schema, table);
        let relation = quote_literal(&qualified);
        let kind = first_column(
            self.rows(
                database,
                &format!(
                    "SELECT c.relkind::text FROM pg_catalog.pg_class c
                     WHERE c.oid = to_regclass({relation})"
                ),
            )
            .await?,
        )
        .into_iter()
        .next()
        .with_context(|| format!("{qualified} was not found"))?;
        if matches!(kind.as_ref(), "v" | "m") {
            return self.definition(path).await;
        }

        // Postgres has no function that prints a table's DDL (pg_dump builds it client-side),
        // so the statement is assembled from the catalog the same way.
        let columns = self
            .rows(
                database,
                &format!(
                    "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod),
                            a.attnotnull, pg_catalog.pg_get_expr(d.adbin, d.adrelid),
                            a.attidentity::text, a.attgenerated::text
                     FROM pg_catalog.pg_attribute a
                     LEFT JOIN pg_catalog.pg_attrdef d
                       ON d.adrelid = a.attrelid AND d.adnum = a.attnum
                     WHERE a.attrelid = to_regclass({relation}) AND a.attnum > 0
                       AND NOT a.attisdropped
                     ORDER BY a.attnum"
                ),
            )
            .await?;
        // NOT NULL constraints are listed in pg_constraint from Postgres 18 on; they are already
        // written on their columns.
        let constraints = self
            .rows(
                database,
                &format!(
                    "SELECT conname, pg_catalog.pg_get_constraintdef(oid, true)
                     FROM pg_catalog.pg_constraint
                     WHERE conrelid = to_regclass({relation}) AND contype <> 'n'
                     ORDER BY CASE contype WHEN 'p' THEN 0 WHEN 'u' THEN 1 WHEN 'c' THEN 2
                              ELSE 3 END, conname"
                ),
            )
            .await?;
        let indexes = first_column(
            self.rows(
                database,
                &format!(
                    "SELECT pg_catalog.pg_get_indexdef(ix.indexrelid)
                     FROM pg_catalog.pg_index ix
                     JOIN pg_catalog.pg_class i ON i.oid = ix.indexrelid
                     WHERE ix.indrelid = to_regclass({relation})
                       AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_constraint c
                                       WHERE c.conindid = ix.indexrelid)
                     ORDER BY i.relname"
                ),
            )
            .await?,
        );

        let mut lines: Vec<String> = columns
            .into_iter()
            .filter_map(|row| {
                let mut values = row.into_iter();
                let name = values.next().flatten()?;
                let data_type = values.next().flatten()?;
                let not_null = values.next().flatten().as_deref() == Some("t");
                let default = values.next().flatten();
                let identity = values.next().flatten();
                let generated = values.next().flatten();
                let mut line = format!("    {} {data_type}", quote_ident(&name));
                match (identity.as_deref(), generated.as_deref(), default) {
                    (Some("a"), _, _) => line.push_str(" GENERATED ALWAYS AS IDENTITY"),
                    (Some("d"), _, _) => line.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
                    (_, Some("s"), Some(expression)) => {
                        line.push_str(&format!(" GENERATED ALWAYS AS ({expression}) STORED"))
                    }
                    (_, _, Some(default)) => line.push_str(&format!(" DEFAULT {default}")),
                    _ => {}
                }
                if not_null {
                    line.push_str(" NOT NULL");
                }
                Some(line)
            })
            .collect();
        lines.extend(constraints.into_iter().filter_map(|row| {
            let mut values = row.into_iter();
            let name = values.next().flatten()?;
            let definition = values.next().flatten()?;
            Some(format!(
                "    CONSTRAINT {} {definition}",
                quote_ident(&name)
            ))
        }));

        let mut script = format!("CREATE TABLE {qualified} (\n{}\n);\n", lines.join(",\n"));
        for index in indexes {
            script.push_str(&format!("\n{index};\n"));
        }
        Ok(script)
    }

    fn qualified_name(&self, path: &[SharedString]) -> Option<String> {
        match path {
            [_, schema, table] => Some(qualified_name(schema, table)),
            [_, schema] => Some(quote_ident(schema)),
            [database] => Some(quote_ident(database)),
            _ => None,
        }
    }

    fn select_statement(&self, path: &[SharedString], limit: usize) -> Option<String> {
        let [_, schema, table] = path else {
            return None;
        };
        Some(format!(
            "SELECT * FROM {} LIMIT {limit};",
            qualified_name(schema, table)
        ))
    }

    fn count_statement(&self, path: &[SharedString]) -> Option<String> {
        let [_, schema, table] = path else {
            return None;
        };
        Some(format!(
            "SELECT count(*) FROM {};",
            qualified_name(schema, table)
        ))
    }

    async fn update_cell(&self, edit: &CellEdit) -> Result<u64> {
        let [database, schema, table] = edit.path.as_slice() else {
            bail!("expected database, schema and table, got {:?}", edit.path);
        };
        if edit.key.is_empty() {
            bail!("refusing to update {table} without a key");
        }
        // Values are written as untyped string literals, which Postgres coerces to the
        // column's type; binding them as parameters would instead fix them as text and fail
        // on every non-text column.
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
            "UPDATE {} SET {} = {} WHERE {condition}",
            qualified_name(schema, table),
            quote_ident(&edit.column),
            edit.value
                .as_deref()
                .map_or_else(|| "NULL".to_string(), quote_literal),
        );

        let pool = self.pool(database).await?;
        let mut transaction = pool.begin().await.context("starting a transaction")?;
        // Through `Executor::execute`, which returns a boxed future, rather than
        // `RawSql::execute`: the latter's lifetime-generic signature cannot be proven Send
        // inside `async_trait`'s future.
        let affected = (&mut *transaction)
            .execute(sqlx::raw_sql(&sql))
            .await
            .with_context(|| format!("running: {sql}"))?
            .rows_affected();
        if affected != 1 {
            transaction
                .rollback()
                .await
                .context("rolling back the update")?;
            bail!("the key matched {affected} rows instead of one; nothing was changed");
        }
        transaction
            .commit()
            .await
            .context("committing the update")?;
        Ok(affected)
    }
}

fn qualified_name(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
