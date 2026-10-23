use crate::schema_diff::{SchemaSnapshot, TableChange};
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
    /// Driver-defined address of this node, passed back to `children`, `preview` and
    /// `definition`. Opaque to the UI so engines can encode whatever they need.
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
    /// A Redis key that holds a value. A key that is also a prefix of other keys stays a `Key`
    /// and has children.
    Key,
    /// A grouping with no value of its own, such as a Redis key prefix.
    Folder,
    /// A function or stored procedure.
    Routine,
    Sequence,
}

impl ObjectKind {
    /// Whether double-clicking the node opens its data in a tab.
    pub fn is_previewable(self) -> bool {
        matches!(
            self,
            ObjectKind::Table | ObjectKind::View | ObjectKind::Collection | ObjectKind::Key
        )
    }

    /// Whether the node has source text that can be opened in a query tab.
    pub fn has_definition(self) -> bool {
        matches!(self, ObjectKind::Routine | ObjectKind::View)
    }
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
    /// Present when every row came from one table, collection or key and can be addressed for
    /// an update. `None` makes the result read-only.
    pub edit_target: Option<EditTarget>,
    /// For engines whose rows are documents (MongoDB), each row's document as indented JSON,
    /// with its types and nesting intact, for the record view. Empty for every other engine,
    /// and `None` for a row edited since it was read.
    pub documents: Vec<Option<SharedString>>,
}

impl ResultSet {
    pub fn message(&self) -> Option<SharedString> {
        self.rows_affected
            .map(|n| SharedString::from(format!("{n} row(s) affected")))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditTarget {
    /// The tree path of the object the rows came from, exactly as it was passed to `preview`.
    pub path: Vec<SharedString>,
    /// Indexes into `ResultSet::columns` that together identify a row.
    pub key_columns: Vec<usize>,
    /// Indexes into `ResultSet::columns` that can be shown but not changed.
    pub read_only_columns: Vec<usize>,
    /// Indexes into `ResultSet::columns` that a new row is given values for, in the order the
    /// form asks for them. Empty when rows cannot be added this way.
    pub insert_columns: Vec<usize>,
}

/// A row's key, as `CellEdit::key`: each key column's name and its value as displayed.
pub type RowKey = [(SharedString, Option<SharedString>)];

/// One cell change, built by the UI from an [`EditTarget`] and handed back to the driver that
/// produced it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellEdit {
    /// `EditTarget::path` of the result the cell is in.
    pub path: Vec<SharedString>,
    /// The edited row's key: each key column's name and its value as displayed.
    pub key: Vec<(SharedString, Option<SharedString>)>,
    pub column: SharedString,
    /// The new value as typed. `None` sets NULL.
    pub value: Option<String>,
}

/// Something done to a whole key from the tree rather than to a row of its value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyAction {
    /// Expire the key after this many seconds, or never with `None`.
    Expire(Option<u64>),
    /// Give the key this full name, refusing if another key already has it.
    Rename(String),
    Delete,
}

/// A name the query editor offers as a completion. Keywords and commands are not here: they
/// are static per engine and live in `completion.rs`. This is what only the server knows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogEntry {
    pub label: SharedString,
    /// `Database`, `Schema`, `Table`, `View`, `Column`, `Collection`, `Key` or `Routine`.
    pub kind: ObjectKind,
    /// The schema of a table, the table of a column, the database of a collection.
    pub parent: Option<SharedString>,
    /// A column's type, a key's value type.
    pub detail: Option<SharedString>,
}

/// How a driver reaches its server. Passwords are deliberately absent: they live in the OS
/// credential store and are fetched just before connecting, so a config can be serialised to
/// disk without carrying a secret.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConnectionConfig {
    pub name: String,
    pub engine: Engine,
    pub host: String,
    pub port: u16,
    pub database: Option<String>,
    pub username: Option<String>,
    /// Which kind of server this is, so production stands out and asks before a write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<Environment>,
    /// The panel groups connections that share a folder under one heading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// A bastion the connection goes through: `host` and `port` are then as the bastion sees
    /// them, and a local port forwarded over SSH is what the driver actually connects to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshTunnel>,
    #[serde(default, skip_serializing_if = "TlsMode::is_default")]
    pub tls: TlsMode,
}

/// `user@host:port` of an SSH server to tunnel through. Logging in is left to the system's
/// `ssh`, so keys, the agent and `~/.ssh/config` all apply.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SshTunnel {
    pub user: Option<String>,
    pub host: String,
    pub port: u16,
    /// A private key to log in with instead of whatever ssh would pick.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
}

impl SshTunnel {
    /// Reads `user@host`, `host:2222` or `user@host:2222`.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let text = text.trim();
        let (user, host_port) = match text.rsplit_once('@') {
            Some((user, host_port)) => (Some(user.to_string()), host_port),
            None => (None, text),
        };
        let (host, port) = match host_port.rsplit_once(':') {
            Some((host, port)) => (
                host,
                port.parse()
                    .map_err(|_| anyhow::anyhow!("{port:?} is not an SSH port number"))?,
            ),
            None => (host_port, 22),
        };
        if host.is_empty() {
            anyhow::bail!("the SSH tunnel has no host; expected user@bastion:22");
        }
        Ok(Self {
            user: user.filter(|user| !user.is_empty()),
            host: host.to_string(),
            port,
            identity_file: None,
        })
    }
}

impl fmt::Display for SshTunnel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(user) = &self.user {
            write!(f, "{user}@")?;
        }
        write!(f, "{}", self.host)?;
        if self.port != 22 {
            write!(f, ":{}", self.port)?;
        }
        Ok(())
    }
}

/// Whether the connection to the server is encrypted, and whether its certificate is checked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    /// What each driver did before there was a choice: Postgres encrypts when the server offers
    /// it, SQL Server always encrypts and accepts any certificate, the others do not encrypt.
    #[default]
    Default,
    Disable,
    /// Encrypt, but accept any certificate, as a self-signed development server needs.
    Require,
    /// Encrypt and check the certificate against the system's trusted roots.
    Verify,
}

impl TlsMode {
    pub fn is_default(&self) -> bool {
        *self == TlsMode::Default
    }

    pub fn parse(text: &str) -> anyhow::Result<Self> {
        Ok(match text.trim().to_ascii_lowercase().as_str() {
            "" | "default" => TlsMode::Default,
            "disable" | "false" | "off" => TlsMode::Disable,
            "require" | "true" | "on" => TlsMode::Require,
            "verify" | "verify-full" => TlsMode::Verify,
            other => {
                anyhow::bail!("unknown tls mode {other:?}; expected disable, require or verify")
            }
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            TlsMode::Default => "default",
            TlsMode::Disable => "disable",
            TlsMode::Require => "require",
            TlsMode::Verify => "verify",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Environment {
    Development,
    Staging,
    Production,
}

impl Environment {
    pub const ALL: [Environment; 3] = [
        Environment::Development,
        Environment::Staging,
        Environment::Production,
    ];

    pub fn color(self) -> ui::Color {
        match self {
            Environment::Development => ui::Color::Success,
            Environment::Staging => ui::Color::Warning,
            Environment::Production => ui::Color::Error,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Environment::Development => "dev",
            Environment::Staging => "staging",
            Environment::Production => "prod",
        }
    }
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
///
/// A future returned by any of these may be dropped part-way through: that is how a query is
/// cancelled. Implementations must leave the driver usable afterwards, reconnecting on next
/// use if the protocol cannot recover from an abandoned request.
#[async_trait::async_trait]
pub trait Driver: Send + Sync {
    /// Lists the children of `path`. An empty path asks for the top level.
    async fn children(&self, path: &[SharedString]) -> Result<Vec<ObjectNode>>;

    /// Runs one statement — the caller has already split the editor text — against
    /// `database`, or the connection's default when `None`. Returns every result set the
    /// statement produced, in order. A statement that changes data instead of returning it
    /// yields one `ResultSet` with `rows_affected` set and no columns.
    async fn query(
        &self,
        statement: &str,
        database: Option<&str>,
        row_limit: usize,
    ) -> Result<Vec<ResultSet>>;

    /// The first page of a table, collection or key, for double-clicking it in the tree. Sets
    /// `edit_target` when the rows can be updated in place.
    async fn preview(&self, path: &[SharedString], row_limit: usize) -> Result<ResultSet> {
        self.preview_page(path, row_limit, 0).await
    }

    /// Like `preview`, but skipping the first `offset` rows, for loading the page after the
    /// one already shown. The order is the server's natural one, which an unchanged table
    /// keeps from one page to the next.
    async fn preview_page(
        &self,
        path: &[SharedString],
        row_limit: usize,
        offset: usize,
    ) -> Result<ResultSet>;

    /// Asks the server to stop whatever this driver is currently running. Best effort: the
    /// caller also drops the future that is running it.
    async fn cancel(&self) -> Result<()> {
        Ok(())
    }

    /// The databases the query editor can be pointed at, for its database picker. Empty when
    /// the engine has only one namespace per connection.
    async fn databases(&self) -> Result<Vec<SharedString>>;

    /// The database queries run in when the editor has not picked one.
    fn default_database(&self) -> Option<SharedString>;

    /// Names in `database` (or the default) for the editor's completions: schemas, tables,
    /// views, columns, routines, collections, keys. Expected to be capped so a huge catalog
    /// costs a bounded amount.
    async fn catalog(&self, database: Option<&str>) -> Result<Vec<CatalogEntry>>;

    /// Source text of a view or routine, opened in a query tab.
    async fn definition(&self, path: &[SharedString]) -> Result<String> {
        anyhow::bail!("{} has no definition to show", path.join("."))
    }

    /// A script that recreates the table, view or collection at `path`: columns, keys,
    /// constraints and indexes, in the engine's own syntax and split the way the query editor
    /// splits statements, so it can be run as it is.
    async fn create_script(&self, path: &[SharedString]) -> Result<String> {
        anyhow::bail!("{} has no CREATE script to show", path.join("."))
    }

    /// The name of the object at `path` as it is written in a query, quoted where the engine
    /// needs it. `None` for nodes a query cannot name, such as a Redis key prefix.
    fn qualified_name(&self, _path: &[SharedString]) -> Option<String> {
        None
    }

    /// A statement reading the first `limit` rows of the object at `path`, for opening it as an
    /// editable query rather than a bare preview.
    fn select_statement(&self, _path: &[SharedString], _limit: usize) -> Option<String> {
        None
    }

    /// A statement counting the rows of the object at `path` exactly. The tree only shows the
    /// planner's estimate, which can be far off.
    fn count_statement(&self, _path: &[SharedString]) -> Option<String> {
        None
    }

    /// The plan the server would use for `statement`, as rows to read in the grid. With
    /// `analyze` the statement is run to measure the plan, so callers offer that only for
    /// statements that do not write.
    async fn explain(
        &self,
        _statement: &str,
        _database: Option<&str>,
        _analyze: bool,
    ) -> Result<Vec<ResultSet>> {
        anyhow::bail!("this engine has no query plan to show")
    }

    /// Keys at or under `path` whose names match the glob `pattern`, searched on the server
    /// rather than in what the tree happens to have loaded.
    async fn search(&self, path: &[SharedString], _pattern: &str) -> Result<ResultSet> {
        anyhow::bail!("{} cannot be searched by pattern", path.join("."))
    }

    async fn key_action(&self, path: &[SharedString], _action: &KeyAction) -> Result<()> {
        anyhow::bail!("{} is not a key", path.join("."))
    }

    /// Adds one row to the object at `path`. `values` holds the text typed for each column
    /// given one; a column left out gets its default. Returns how many rows were added.
    async fn insert_row(
        &self,
        path: &[SharedString],
        _values: &[(SharedString, String)],
    ) -> Result<u64> {
        anyhow::bail!("adding rows to {} is not supported", path.join("."))
    }

    /// Deletes the one row `key` identifies. Implementations must refuse rather than delete
    /// anything else when the key matches no row or several.
    async fn delete_row(&self, path: &[SharedString], _key: &RowKey) -> Result<u64> {
        anyhow::bail!("deleting rows from {} is not supported", path.join("."))
    }

    /// Whether the node at `path` is a schema (or, where there are no schemas, a database)
    /// whose tables can be compared with another's.
    fn supports_schema_compare(&self, _path: &[SharedString]) -> bool {
        false
    }

    /// The tables of the schema at `path` and their columns, for comparing it with another.
    async fn schema_snapshot(&self, path: &[SharedString]) -> Result<SchemaSnapshot> {
        anyhow::bail!("{} cannot be compared", path.join("."))
    }

    /// How a line comment starts in the engine's query language.
    fn comment_prefix(&self) -> &'static str {
        "--"
    }

    /// The statements making a table of the schema at `schema` match `change`. Any that would
    /// lose data come back commented out.
    fn alter_table(&self, _schema: &[SharedString], _change: &TableChange) -> Vec<String> {
        Vec::new()
    }

    fn drop_table(&self, _schema: &[SharedString], _table: &str) -> String {
        String::new()
    }

    /// `script`, a CREATE script read from the schema at `from`, rewritten to create its
    /// objects in the schema at `to` instead.
    fn retarget_script(
        &self,
        script: &str,
        _from: &[SharedString],
        _to: &[SharedString],
    ) -> String {
        script.to_string()
    }

    /// Applies one cell change and returns how many rows it touched. Implementations must
    /// refuse rather than guess when the key does not identify the row.
    async fn update_cell(&self, edit: &CellEdit) -> Result<u64> {
        anyhow::bail!(
            "editing {} is not supported for this engine",
            edit.path.join(".")
        )
    }
}
