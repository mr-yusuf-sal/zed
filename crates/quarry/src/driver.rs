use anyhow::Result;
use std::fmt;
use ui::SharedString;

/// The engines Quarry can talk to. Redis is the odd one out: it has no tables and no query
/// language in the SQL sense, so it reports its keyspace through the same object tree and
/// answers commands rather than queries. ClickHouse and the two SQL servers map cleanly;
/// MongoDB returns documents that are flattened into columns only for display.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    Postgres,
    SqlServer,
    MongoDb,
    Redis,
    ClickHouse,
}

impl Engine {
    pub fn label(self) -> &'static str {
        match self {
            Engine::Postgres => "PostgreSQL",
            Engine::SqlServer => "SQL Server",
            Engine::MongoDb => "MongoDB",
            Engine::Redis => "Redis",
            Engine::ClickHouse => "ClickHouse",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Engine::Postgres => 5432,
            Engine::SqlServer => 1433,
            Engine::MongoDb => 27017,
            Engine::Redis => 6379,
            Engine::ClickHouse => 8123,
        }
    }

    /// What the query editor is actually sending. Used for the editor's language and for the
    /// wording of the run action, so Redis is not told it is running "SQL".
    pub fn query_language(self) -> &'static str {
        match self {
            Engine::Postgres | Engine::SqlServer | Engine::ClickHouse => "SQL",
            Engine::MongoDb => "JSON",
            Engine::Redis => "Redis",
        }
    }
}

/// A node in the left-hand tree. Engines disagree about what lives where — schemas exist in
/// Postgres and SQL Server, MongoDB has databases and collections, Redis has only a keyspace —
/// so the tree is built from whatever each driver reports rather than a fixed hierarchy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectNode {
    pub name: SharedString,
    pub kind: ObjectKind,
    /// Driver-defined address of this node, passed back to `children` and `preview`. Opaque to
    /// the UI so engines can encode whatever they need (schema-qualified names, key prefixes).
    pub path: Vec<SharedString>,
    pub has_children: bool,
    /// Shown dimmed after the name: a row count, a column type, a Redis value type.
    pub detail: Option<SharedString>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjectKind {
    Database,
    Schema,
    Table,
    View,
    Column,
    Index,
    Collection,
    Key,
}

/// A grid of already-stringified values. Drivers do the type conversion because only they know
/// their own type system; the UI never sees an engine-specific value.
///
/// `None` is a genuine NULL and is rendered differently from an empty string, which is why the
/// cells are `Option` rather than plain strings.
#[derive(Clone, Debug, Default)]
pub struct ResultSet {
    pub columns: Vec<SharedString>,
    pub rows: Vec<Vec<Option<SharedString>>>,
    /// Set for statements that change data instead of returning it.
    pub rows_affected: Option<u64>,
    /// Server-side notices worth surfacing (Postgres NOTICE, ClickHouse warnings).
    pub notices: Vec<SharedString>,
    pub truncated: bool,
}

impl ResultSet {
    pub fn message(&self) -> Option<SharedString> {
        self.rows_affected
            .map(|n| SharedString::from(format!("{n} row(s) affected")))
    }
}

/// How a driver reaches its server. Passwords are deliberately absent: they live in the OS
/// credential store and are fetched just before connecting, so a config can be serialised to
/// disk without carrying a secret.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ConnectionConfig {
    pub name: String,
    pub engine: Engine,
    pub host: String,
    pub port: u16,
    pub database: Option<String>,
    pub username: Option<String>,
}

impl ConnectionConfig {
    /// Stable identity for the credential store. Including the engine keeps a Postgres and a
    /// ClickHouse on the same host and port from sharing one password entry.
    pub fn credential_url(&self) -> String {
        format!(
            "quarry://{}@{}:{}",
            match self.engine {
                Engine::Postgres => "postgres",
                Engine::SqlServer => "sqlserver",
                Engine::MongoDb => "mongodb",
                Engine::Redis => "redis",
                Engine::ClickHouse => "clickhouse",
            },
            self.host,
            self.port
        )
    }
}

impl fmt::Display for ConnectionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

/// Implemented once per engine. Every method runs on the Quarry tokio runtime, never on a gpui
/// executor, so implementations are free to use driver crates that demand tokio.
#[async_trait::async_trait]
pub trait Driver: Send + Sync {
    /// Lists the children of `path`. An empty path asks for the top level, which is databases
    /// for MongoDB and ClickHouse, schemas for the SQL servers, and key prefixes for Redis.
    async fn children(&self, path: &[SharedString]) -> Result<Vec<ObjectNode>>;

    /// Runs whatever the user typed. The driver decides what that means for its engine.
    async fn query(&self, text: &str, row_limit: usize) -> Result<ResultSet>;

    /// The first page of a table or collection, for double-clicking an object in the tree.
    async fn preview(&self, path: &[SharedString], row_limit: usize) -> Result<ResultSet>;
}
