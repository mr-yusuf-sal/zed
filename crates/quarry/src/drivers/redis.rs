use crate::driver::{
    CatalogEntry, CellEdit, Driver, EditTarget, KeyAction, ObjectKind, ObjectNode, ResultSet,
    RowKey,
};
use anyhow::{Context as _, Result, anyhow, bail};
use collections::{BTreeMap, HashMap, HashSet};
use parking_lot::Mutex;
use redis::{
    ConnectionAddr, ConnectionInfo, RedisConnectionInfo, Value, aio::MultiplexedConnection,
};
use ui::SharedString;

/// Redis has no tables and no query language, so it is the one engine that does not map onto
/// rows and columns. The tree shows the keyspace instead, grouped by the `:` convention almost
/// every codebase uses for namespacing, and the editor runs commands rather than queries.
///
/// Logical databases are named `db0`, `db1`, ... everywhere: in the tree, in the database
/// picker and in the `database` argument the editor passes back.
pub struct RedisDriver {
    connection_info: ConnectionInfo,
    default_database: i64,
    /// One multiplexed connection per logical database. A multiplexed connection is shared by
    /// every caller, so running `SELECT` on it to switch databases would switch them for
    /// everyone in the middle of their commands.
    connections: Mutex<HashMap<i64, MultiplexedConnection>>,
}

/// How many keys one tree level or catalog looks at. SCAN still walks the whole keyspace to
/// find matches for a prefix, but this bounds what is held and sent back to the panel.
const TREE_KEY_LIMIT: usize = 10_000;
const CATALOG_KEY_LIMIT: usize = 2_000;

impl RedisDriver {
    pub async fn connect(
        host: &str,
        port: u16,
        database: Option<&str>,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<Self> {
        let default_database = match database.filter(|database| !database.trim().is_empty()) {
            Some(database) => parse_database(database)?,
            None => 0,
        };
        // Built directly rather than as a URL so a password with `@` or `/` in it needs no
        // escaping.
        let connection_info = ConnectionInfo {
            addr: ConnectionAddr::Tcp(host.to_string(), port),
            redis: RedisConnectionInfo {
                db: default_database,
                username: username.map(str::to_string),
                password: password.map(str::to_string),
                ..RedisConnectionInfo::default()
            },
        };
        let driver = Self {
            connection_info,
            default_database,
            connections: Mutex::new(HashMap::default()),
        };
        driver
            .connection(default_database)
            .await
            .with_context(|| format!("connecting to redis at {host}:{port}"))?;
        Ok(driver)
    }

    async fn connection(&self, database: i64) -> Result<MultiplexedConnection> {
        if let Some(connection) = self.connections.lock().get(&database) {
            return Ok(connection.clone());
        }
        let connection = self.open_connection(database).await?;
        Ok(self
            .connections
            .lock()
            .entry(database)
            .or_insert(connection)
            .clone())
    }

    async fn open_connection(&self, database: i64) -> Result<MultiplexedConnection> {
        let mut connection_info = self.connection_info.clone();
        connection_info.redis.db = database;
        let client = redis::Client::open(connection_info).context("building the Redis client")?;
        client
            .get_multiplexed_async_connection()
            .await
            .with_context(|| format!("opening db{database}"))
    }

    fn resolve_database(&self, database: Option<&str>) -> Result<i64> {
        match database {
            Some(database) => parse_database(database),
            None => Ok(self.default_database),
        }
    }

    /// Splits a tree path into its database and the key (or key prefix) it names.
    fn locate(path: &[SharedString]) -> Result<(i64, String)> {
        let [database, segments @ ..] = path else {
            bail!("expected a database, got an empty path");
        };
        Ok((parse_database(database)?, segments.join(":")))
    }

    /// Walks the keyspace with SCAN rather than KEYS. KEYS blocks the server for the whole
    /// sweep, which on a production instance is a real outage; SCAN is incremental and is what
    /// Redis itself tells you to use.
    async fn scan(
        connection: &mut MultiplexedConnection,
        pattern: &str,
        limit: usize,
    ) -> Result<Vec<String>> {
        let mut cursor: u64 = 0;
        let mut keys = HashSet::default();
        loop {
            let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(pattern)
                .arg("COUNT")
                .arg(1000)
                .query_async(connection)
                .await
                .context("scanning the keyspace")?;
            // SCAN may return a key more than once while the keyspace is being rehashed.
            keys.extend(batch);
            cursor = next;
            if cursor == 0 || keys.len() >= limit {
                break;
            }
        }
        let mut keys: Vec<String> = keys.into_iter().collect();
        keys.sort();
        keys.truncate(limit);
        Ok(keys)
    }

    /// Key counts per logical database from `INFO keyspace`, which lists only the databases
    /// that hold at least one key.
    async fn keyspace(&self) -> Result<BTreeMap<i64, u64>> {
        let mut connection = self.connection(self.default_database).await?;
        let info: String = redis::cmd("INFO")
            .arg("keyspace")
            .query_async(&mut connection)
            .await
            .context("reading INFO keyspace")?;
        let mut databases = BTreeMap::default();
        for line in info.lines() {
            let Some((name, fields)) = line.trim().split_once(':') else {
                continue;
            };
            let Some(number) = name
                .strip_prefix("db")
                .and_then(|number| number.parse().ok())
            else {
                continue;
            };
            let keys = fields
                .split(',')
                .find_map(|field| field.strip_prefix("keys="))
                .and_then(|count| count.parse().ok())
                .unwrap_or(0);
            databases.insert(number, keys);
        }
        databases.entry(self.default_database).or_insert(0);
        Ok(databases)
    }
}

fn database_name(number: i64) -> SharedString {
    SharedString::from(format!("db{number}"))
}

/// Accepts `db3` as named in the tree and picker, and a bare `3` as written in a URL.
fn parse_database(name: &str) -> Result<i64> {
    let name = name.trim();
    name.strip_prefix("db")
        .unwrap_or(name)
        .parse()
        .with_context(|| format!("{name:?} is not a Redis database; expected db0, db1, ..."))
}

/// SCAN's MATCH is a glob, so a literal prefix must have its glob characters escaped or a key
/// named `a[1]:x` would match nothing.
fn escape_glob(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(character, '*' | '?' | '[' | ']' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn format_ttl(milliseconds: i64) -> String {
    let seconds = (milliseconds + 999) / 1000;
    let (minutes, seconds) = (seconds / 60, seconds % 60);
    let (hours, minutes) = (minutes / 60, minutes % 60);
    let (days, hours) = (hours / 24, hours % 24);
    if days > 0 {
        format!("ttl {days}d {hours}h")
    } else if hours > 0 {
        format!("ttl {hours}h {minutes}m")
    } else if minutes > 0 {
        format!("ttl {minutes}m {seconds}s")
    } else {
        format!("ttl {seconds}s")
    }
}

/// The type and remaining time to live of each key, fetched in one round trip each rather than
/// one per key: a tree level can hold thousands of keys.
async fn types_and_ttls(
    connection: &mut MultiplexedConnection,
    keys: &[String],
) -> Result<Vec<(String, i64)>> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let mut pipeline = redis::pipe();
    for key in keys {
        pipeline.cmd("TYPE").arg(key);
    }
    let types: Vec<String> = pipeline
        .query_async(connection)
        .await
        .context("reading key types")?;

    let mut pipeline = redis::pipe();
    for key in keys {
        pipeline.cmd("PTTL").arg(key);
    }
    let ttls: Vec<i64> = pipeline
        .query_async(connection)
        .await
        .context("reading key TTLs")?;
    Ok(types.into_iter().zip(ttls).collect())
}

fn key_detail(kind: &str, ttl: i64) -> SharedString {
    if ttl > 0 {
        SharedString::from(format!("{kind} · {}", format_ttl(ttl)))
    } else {
        SharedString::from(kind.to_string())
    }
}

fn text(bytes: &[u8]) -> SharedString {
    SharedString::from(String::from_utf8_lossy(bytes).into_owned())
}

/// HyperLogLogs are stored as strings that begin with this magic. Their bytes are a sketch, not
/// text, so they are summarised by their count instead of being printed.
const HYPERLOGLOG_HEADER: &[u8] = b"HYLL";

/// Splits a command line the way redis-cli does: whitespace separates arguments, double quotes
/// allow backslash escapes (including `\xHH`), single quotes are literal apart from `\'`, and a
/// closing quote must end the argument.
fn split_arguments(line: &str) -> Result<Vec<Vec<u8>>> {
    let mut arguments = Vec::new();
    let mut characters = line.chars().peekable();
    loop {
        while characters
            .next_if(|character| character.is_whitespace())
            .is_some()
        {}
        let Some(&first) = characters.peek() else {
            return Ok(arguments);
        };
        let mut argument = Vec::new();
        match first {
            '"' => {
                characters.next();
                loop {
                    let character = characters
                        .next()
                        .ok_or_else(|| anyhow!("unbalanced double quotes"))?;
                    match character {
                        '"' => break,
                        '\\' => {
                            let escaped = characters
                                .next()
                                .ok_or_else(|| anyhow!("unbalanced double quotes"))?;
                            match escaped {
                                'n' => argument.push(b'\n'),
                                'r' => argument.push(b'\r'),
                                't' => argument.push(b'\t'),
                                'b' => argument.push(0x08),
                                'a' => argument.push(0x07),
                                'x' => {
                                    let high = characters.peek().and_then(|c| c.to_digit(16));
                                    let mut lookahead = characters.clone();
                                    lookahead.next();
                                    let low = lookahead.peek().and_then(|c| c.to_digit(16));
                                    match (high, low) {
                                        (Some(high), Some(low)) => {
                                            characters.next();
                                            characters.next();
                                            argument.push((high * 16 + low) as u8);
                                        }
                                        _ => argument.push(b'x'),
                                    }
                                }
                                other => push_char(&mut argument, other),
                            }
                        }
                        other => push_char(&mut argument, other),
                    }
                }
                ensure_argument_ends(characters.peek())?;
            }
            '\'' => {
                characters.next();
                loop {
                    let character = characters
                        .next()
                        .ok_or_else(|| anyhow!("unbalanced single quotes"))?;
                    match character {
                        '\'' => break,
                        '\\' if characters.peek() == Some(&'\'') => {
                            characters.next();
                            argument.push(b'\'');
                        }
                        other => push_char(&mut argument, other),
                    }
                }
                ensure_argument_ends(characters.peek())?;
            }
            _ => {
                while let Some(character) =
                    characters.next_if(|character| !character.is_whitespace())
                {
                    push_char(&mut argument, character);
                }
            }
        }
        arguments.push(argument);
    }
}

/// Commands that can hold a connection open indefinitely waiting for something to happen.
fn is_blocking(name: &[u8], arguments: &[Vec<u8>]) -> bool {
    const ALWAYS_BLOCKING: [&[u8]; 14] = [
        b"BLPOP",
        b"BRPOP",
        b"BRPOPLPUSH",
        b"BLMOVE",
        b"BLMPOP",
        b"BZPOPMIN",
        b"BZPOPMAX",
        b"BZMPOP",
        b"WAIT",
        b"WAITAOF",
        b"SUBSCRIBE",
        b"PSUBSCRIBE",
        b"SSUBSCRIBE",
        b"MONITOR",
    ];
    if ALWAYS_BLOCKING
        .iter()
        .any(|blocking| name.eq_ignore_ascii_case(blocking))
    {
        return true;
    }
    (name.eq_ignore_ascii_case(b"XREAD") || name.eq_ignore_ascii_case(b"XREADGROUP"))
        && arguments
            .iter()
            .any(|argument| argument.eq_ignore_ascii_case(b"BLOCK"))
}

fn push_char(bytes: &mut Vec<u8>, character: char) {
    let mut buffer = [0; 4];
    bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
}

fn ensure_argument_ends(next: Option<&char>) -> Result<()> {
    match next {
        Some(character) if !character.is_whitespace() => {
            bail!("a closing quote must be followed by a space")
        }
        _ => Ok(()),
    }
}

/// Renders a reply as a single column of lines. Redis replies are trees — nested arrays, maps,
/// sets — and flattening them into a grid would invent a structure that is not there, so they
/// are shown the way redis-cli shows them.
fn render(value: &Value, out: &mut Vec<Option<SharedString>>, depth: usize) {
    let indent = "  ".repeat(depth);
    match value {
        Value::Nil => out.push(None),
        Value::Int(n) => out.push(Some(format!("{indent}{n}").into())),
        Value::BulkString(bytes) => out.push(Some(
            format!("{indent}{}", String::from_utf8_lossy(bytes)).into(),
        )),
        Value::SimpleString(text) => out.push(Some(format!("{indent}{text}").into())),
        Value::Okay => out.push(Some(format!("{indent}OK").into())),
        Value::Double(n) => out.push(Some(format!("{indent}{n}").into())),
        Value::Boolean(b) => out.push(Some(format!("{indent}{b}").into())),
        Value::VerbatimString { text, .. } => {
            for line in text.lines() {
                out.push(Some(format!("{indent}{line}").into()));
            }
        }
        Value::Array(items) | Value::Set(items) => {
            for item in items {
                render(item, out, depth + 1);
            }
        }
        Value::Map(pairs) => {
            for (key, value) in pairs {
                render(key, out, depth + 1);
                render(value, out, depth + 2);
            }
        }
        other => out.push(Some(format!("{indent}{other:?}").into())),
    }
}

/// Reads a SSCAN or HSCAN until at least `limit` items have been seen or the cursor wraps. The
/// scans replace SMEMBERS and HGETALL, which would pull a key of millions of members in one
/// reply just to show the first page.
async fn scan_collection(
    connection: &mut MultiplexedConnection,
    command: &str,
    key: &str,
    limit: usize,
) -> Result<(Vec<Vec<u8>>, bool)> {
    let mut cursor: u64 = 0;
    let mut items = Vec::new();
    loop {
        let (next, batch): (u64, Vec<Vec<u8>>) = redis::cmd(command)
            .arg(key)
            .arg(cursor)
            .arg("COUNT")
            .arg(500)
            .query_async(connection)
            .await
            .with_context(|| format!("reading {key}"))?;
        items.extend(batch);
        cursor = next;
        if cursor == 0 || items.len() >= limit {
            return Ok((items, cursor != 0));
        }
    }
}

fn string_rows(
    rows: impl IntoIterator<Item = Vec<SharedString>>,
) -> Vec<Vec<Option<SharedString>>> {
    rows.into_iter()
        .map(|row| row.into_iter().map(Some).collect())
        .collect()
}

fn columns(names: &[&'static str]) -> Vec<SharedString> {
    names.iter().map(|name| SharedString::from(*name)).collect()
}

fn key_value<'a>(edit: &'a CellEdit, column: &str) -> Result<&'a str> {
    edit.key
        .iter()
        .find(|(name, _)| name.as_ref() == column)
        .and_then(|(_, value)| value.as_ref())
        .map(|value| value.as_ref())
        .with_context(|| format!("the edit does not say which {column} to change"))
}

#[async_trait::async_trait]
impl Driver for RedisDriver {
    async fn children(&self, path: &[SharedString]) -> Result<Vec<ObjectNode>> {
        if path.is_empty() {
            return Ok(self
                .keyspace()
                .await?
                .into_iter()
                .map(|(number, keys)| {
                    let name = database_name(number);
                    ObjectNode {
                        path: vec![name.clone()],
                        name,
                        kind: ObjectKind::Database,
                        has_children: keys > 0,
                        detail: Some(SharedString::from(format!("{keys} keys"))),
                    }
                })
                .collect());
        }

        // Keys are grouped by ':' one level at a time, so `user:1:name` appears as `user` then
        // `1` then `name` instead of as one flat list of thousands of entries.
        let (database, prefix) = Self::locate(path)?;
        let prefix = if path.len() == 1 {
            String::new()
        } else {
            format!("{prefix}:")
        };
        let mut connection = self.connection(database).await?;
        let keys = Self::scan(
            &mut connection,
            &format!("{}*", escape_glob(&prefix)),
            TREE_KEY_LIMIT,
        )
        .await?;

        // For each segment: whether it is a key of its own, and whether longer keys extend it.
        let mut groups: BTreeMap<String, (bool, bool)> = BTreeMap::default();
        for key in &keys {
            let Some(rest) = key.strip_prefix(&prefix) else {
                continue;
            };
            let entry = groups.entry(match rest.split_once(':') {
                Some((segment, _)) => segment.to_string(),
                None => rest.to_string(),
            });
            let (is_key, has_children) = entry.or_insert((false, false));
            if rest.contains(':') {
                *has_children = true;
            } else {
                *is_key = true;
            }
        }

        let value_keys: Vec<String> = groups
            .iter()
            .filter(|(_, (is_key, _))| *is_key)
            .map(|(segment, _)| format!("{prefix}{segment}"))
            .collect();
        let mut details: HashMap<String, SharedString> = value_keys
            .iter()
            .cloned()
            .zip(
                types_and_ttls(&mut connection, &value_keys)
                    .await?
                    .into_iter()
                    .map(|(kind, ttl)| key_detail(&kind, ttl)),
            )
            .collect();

        Ok(groups
            .into_iter()
            .map(|(segment, (is_key, has_children))| {
                let mut node_path = path.to_vec();
                let name = SharedString::from(segment);
                node_path.push(name.clone());
                ObjectNode {
                    detail: if is_key {
                        details.remove(&format!("{prefix}{name}"))
                    } else {
                        None
                    },
                    path: node_path,
                    name,
                    kind: if is_key {
                        ObjectKind::Key
                    } else {
                        ObjectKind::Folder
                    },
                    has_children,
                }
            })
            .collect())
    }

    async fn query(
        &self,
        statement: &str,
        database: Option<&str>,
        row_limit: usize,
    ) -> Result<Vec<ResultSet>> {
        let arguments = split_arguments(statement)?;
        let Some((name, arguments)) = arguments.split_first() else {
            return Ok(Vec::new());
        };
        if name.eq_ignore_ascii_case(b"SELECT") {
            bail!(
                "SELECT would switch the database for every view sharing this connection; pick the database in the editor instead"
            );
        }
        let mut command = redis::cmd(&String::from_utf8_lossy(name));
        for argument in arguments {
            command.arg(argument.as_slice());
        }

        let database = self.resolve_database(database)?;
        // A blocking command (BLPOP 0, XREAD BLOCK) holds its connection until it is answered.
        // On the shared connection that would stall every other command in this database, and
        // an abandoned one would keep stalling them after a cancel, so it gets a connection of
        // its own that is simply dropped with the future.
        let mut connection = if is_blocking(name, arguments) {
            self.open_connection(database).await?
        } else {
            self.connection(database).await?
        };
        let value: Value = command
            .query_async(&mut connection)
            .await
            .with_context(|| format!("running: {}", statement.trim()))?;

        let mut lines = Vec::new();
        render(&value, &mut lines, 0);
        let truncated = lines.len() > row_limit;
        lines.truncate(row_limit);
        Ok(vec![ResultSet {
            columns: columns(&["reply"]),
            rows: lines.into_iter().map(|line| vec![line]).collect(),
            truncated,
            ..ResultSet::default()
        }])
    }

    async fn preview_page(
        &self,
        path: &[SharedString],
        row_limit: usize,
        offset: usize,
    ) -> Result<ResultSet> {
        let (database, key) = Self::locate(path)?;
        if path.len() < 2 {
            bail!("expected a key, got a database");
        }
        let mut connection = self.connection(database).await?;

        let (kind, ttl): (String, i64) = redis::pipe()
            .cmd("TYPE")
            .arg(&key)
            .cmd("PTTL")
            .arg(&key)
            .query_async(&mut connection)
            .await
            .context("reading the key's type")?;
        // The last index of a ranged read, one past the page to learn whether it was cut short.
        let last = (offset + row_limit) as isize;

        let edit_target =
            |key_columns: Vec<usize>, read_only_columns: Vec<usize>, insert_columns: Vec<usize>| {
                Some(EditTarget {
                    path: path.to_vec(),
                    key_columns,
                    read_only_columns,
                    insert_columns,
                })
            };

        // Each type needs its own read command; there is no generic "get whatever this is".
        // Ranged reads ask for one more item than shown to learn whether the page was cut short.
        let mut result = match kind.as_str() {
            // A string is one value, so it has no page after the first.
            "string" if offset > 0 => ResultSet {
                columns: columns(&["value"]),
                ..ResultSet::default()
            },
            "string" => {
                let value: Vec<u8> = redis::cmd("GET")
                    .arg(&key)
                    .query_async(&mut connection)
                    .await
                    .with_context(|| format!("reading {key}"))?;
                if value.starts_with(HYPERLOGLOG_HEADER) {
                    let count: u64 = redis::cmd("PFCOUNT")
                        .arg(&key)
                        .query_async(&mut connection)
                        .await
                        .with_context(|| format!("counting {key}"))?;
                    ResultSet {
                        columns: columns(&["value"]),
                        rows: string_rows([vec![SharedString::from(format!(
                            "<HyperLogLog, PFCOUNT={count}>"
                        ))]]),
                        ..ResultSet::default()
                    }
                } else {
                    // A single value has nothing to key on; the key itself is the row.
                    ResultSet {
                        columns: columns(&["value"]),
                        rows: string_rows([vec![text(&value)]]),
                        edit_target: edit_target(Vec::new(), Vec::new(), Vec::new()),
                        ..ResultSet::default()
                    }
                }
            }
            "list" => {
                let items: Vec<Vec<u8>> = redis::cmd("LRANGE")
                    .arg(&key)
                    .arg(offset)
                    .arg(last)
                    .query_async(&mut connection)
                    .await
                    .with_context(|| format!("reading {key}"))?;
                ResultSet {
                    columns: columns(&["index", "value"]),
                    truncated: items.len() > row_limit,
                    rows: string_rows(items.iter().take(row_limit).enumerate().map(
                        |(index, item)| {
                            vec![SharedString::from((offset + index).to_string()), text(item)]
                        },
                    )),
                    // A new item is pushed onto the end; its index is the list's to give.
                    edit_target: edit_target(vec![0], vec![0], vec![1]),
                    ..ResultSet::default()
                }
            }
            "set" => {
                // SCAN has no offset; the members before the page are read and skipped.
                let (members, more) =
                    scan_collection(&mut connection, "SSCAN", &key, offset + row_limit + 1).await?;
                let mut seen = HashSet::default();
                let members: Vec<Vec<u8>> = members
                    .into_iter()
                    .filter(|member| seen.insert(member.clone()))
                    .skip(offset)
                    .collect();
                // Members are their own identity: changing one is a remove plus an add, not an
                // edit, so a member can be added or deleted but not edited.
                ResultSet {
                    columns: columns(&["member"]),
                    truncated: more || members.len() > row_limit,
                    rows: string_rows(
                        members
                            .iter()
                            .take(row_limit)
                            .map(|member| vec![text(member)]),
                    ),
                    edit_target: edit_target(vec![0], vec![0], vec![0]),
                    ..ResultSet::default()
                }
            }
            "zset" => {
                let items: Vec<(Vec<u8>, String)> = redis::cmd("ZRANGE")
                    .arg(&key)
                    .arg(offset)
                    .arg(last)
                    .arg("WITHSCORES")
                    .query_async(&mut connection)
                    .await
                    .with_context(|| format!("reading {key}"))?;
                ResultSet {
                    columns: columns(&["member", "score"]),
                    truncated: items.len() > row_limit,
                    rows: string_rows(items.iter().take(row_limit).map(|(member, score)| {
                        vec![text(member), SharedString::from(score.clone())]
                    })),
                    edit_target: edit_target(vec![0], vec![0], vec![0, 1]),
                    ..ResultSet::default()
                }
            }
            "hash" => {
                let (flat, more) =
                    scan_collection(&mut connection, "HSCAN", &key, (offset + row_limit + 1) * 2)
                        .await?;
                let mut seen = HashSet::default();
                let pairs: Vec<(&[u8], &[u8])> = flat
                    .chunks_exact(2)
                    .filter_map(|pair| match pair {
                        [field, value] => Some((field.as_slice(), value.as_slice())),
                        _ => None,
                    })
                    .filter(|(field, _)| seen.insert(field.to_vec()))
                    .skip(offset)
                    .collect();
                ResultSet {
                    columns: columns(&["field", "value"]),
                    truncated: more || pairs.len() > row_limit,
                    rows: string_rows(
                        pairs
                            .iter()
                            .take(row_limit)
                            .map(|(field, value)| vec![text(field), text(value)]),
                    ),
                    edit_target: edit_target(vec![0], vec![0], vec![0, 1]),
                    ..ResultSet::default()
                }
            }
            "stream" => {
                let entries: Vec<(String, Vec<Vec<u8>>)> = redis::cmd("XRANGE")
                    .arg(&key)
                    .arg("-")
                    .arg("+")
                    .arg("COUNT")
                    .arg(offset + row_limit + 1)
                    .query_async(&mut connection)
                    .await
                    .with_context(|| format!("reading {key}"))?;
                let entries: Vec<_> = entries.into_iter().skip(offset).collect();
                let rows = entries.iter().take(row_limit).map(|(id, fields)| {
                    let object: serde_json::Map<String, serde_json::Value> = fields
                        .chunks_exact(2)
                        .filter_map(|pair| match pair {
                            [field, value] => Some((
                                String::from_utf8_lossy(field).into_owned(),
                                serde_json::Value::String(
                                    String::from_utf8_lossy(value).into_owned(),
                                ),
                            )),
                            _ => None,
                        })
                        .collect();
                    vec![
                        SharedString::from(id.clone()),
                        SharedString::from(serde_json::Value::Object(object).to_string()),
                    ]
                });
                // Stream entries are append-only by design; there is no command to change one,
                // only to add one (with an id the server picks) or delete one.
                ResultSet {
                    columns: columns(&["id", "fields"]),
                    truncated: entries.len() > row_limit,
                    rows: string_rows(rows),
                    edit_target: edit_target(vec![0], vec![0, 1], vec![1]),
                    ..ResultSet::default()
                }
            }
            "none" => bail!("{key} no longer exists"),
            other => bail!("{key} is a {other}, which Quarry cannot display yet"),
        };
        if ttl > 0 {
            result.notices.push(SharedString::from(format_ttl(ttl)));
        }
        Ok(result)
    }

    async fn databases(&self) -> Result<Vec<SharedString>> {
        Ok(self
            .keyspace()
            .await?
            .into_keys()
            .map(database_name)
            .collect())
    }

    fn default_database(&self) -> Option<SharedString> {
        Some(database_name(self.default_database))
    }

    async fn catalog(&self, database: Option<&str>) -> Result<Vec<CatalogEntry>> {
        let database = self.resolve_database(database)?;
        let mut connection = self.connection(database).await?;
        let keys = Self::scan(&mut connection, "*", CATALOG_KEY_LIMIT).await?;
        let details = types_and_ttls(&mut connection, &keys).await?;
        Ok(keys
            .into_iter()
            .zip(details)
            .map(|(key, (kind, _))| CatalogEntry {
                label: SharedString::from(key),
                kind: ObjectKind::Key,
                parent: None,
                detail: Some(SharedString::from(kind)),
            })
            .collect())
    }

    /// SCANs for `pattern` under the prefix `path` names, so "Find Keys" on `user` with `*:email`
    /// looks for `user:*:email`. The pattern is a glob as MATCH reads it; the prefix is escaped.
    async fn search(&self, path: &[SharedString], pattern: &str) -> Result<ResultSet> {
        let (database, prefix) = Self::locate(path)?;
        let pattern = pattern.trim();
        let pattern = if pattern.is_empty() { "*" } else { pattern };
        let pattern = if path.len() > 1 {
            format!("{}:{pattern}", escape_glob(&prefix))
        } else {
            pattern.to_string()
        };
        let mut connection = self.connection(database).await?;
        let keys = Self::scan(&mut connection, &pattern, TREE_KEY_LIMIT + 1).await?;
        let truncated = keys.len() > TREE_KEY_LIMIT;
        let keys: Vec<String> = keys.into_iter().take(TREE_KEY_LIMIT).collect();
        let details = types_and_ttls(&mut connection, &keys).await?;
        Ok(ResultSet {
            columns: columns(&["key", "type", "ttl"]),
            rows: keys
                .into_iter()
                .zip(details)
                .map(|(key, (kind, ttl))| {
                    vec![
                        Some(SharedString::from(key)),
                        Some(SharedString::from(kind)),
                        (ttl > 0).then(|| SharedString::from(format_ttl(ttl))),
                    ]
                })
                .collect(),
            truncated,
            ..ResultSet::default()
        })
    }

    async fn key_action(&self, path: &[SharedString], action: &KeyAction) -> Result<()> {
        let (database, key) = Self::locate(path)?;
        if path.len() < 2 {
            bail!("expected a key, got a database");
        }
        let mut connection = self.connection(database).await?;
        match action {
            KeyAction::Expire(Some(seconds)) => {
                let set: bool = redis::cmd("EXPIRE")
                    .arg(&key)
                    .arg(*seconds)
                    .query_async(&mut connection)
                    .await
                    .with_context(|| format!("setting the TTL of {key}"))?;
                if !set {
                    bail!("{key} no longer exists");
                }
            }
            KeyAction::Expire(None) => {
                redis::cmd("PERSIST")
                    .arg(&key)
                    .exec_async(&mut connection)
                    .await
                    .with_context(|| format!("removing the TTL of {key}"))?;
            }
            KeyAction::Rename(new_name) => {
                // RENAMENX, so a typo cannot overwrite a key that happens to have that name.
                let renamed: bool = redis::cmd("RENAMENX")
                    .arg(&key)
                    .arg(new_name)
                    .query_async(&mut connection)
                    .await
                    .with_context(|| format!("renaming {key}"))?;
                if !renamed {
                    bail!("a key named {new_name} already exists; nothing was renamed");
                }
            }
            KeyAction::Delete => {
                // UNLINK frees a large value in the background instead of blocking the server.
                let removed: u64 = redis::cmd("UNLINK")
                    .arg(&key)
                    .query_async(&mut connection)
                    .await
                    .with_context(|| format!("deleting {key}"))?;
                if removed == 0 {
                    bail!("{key} no longer exists");
                }
            }
        }
        Ok(())
    }

    async fn insert_row(
        &self,
        path: &[SharedString],
        values: &[(SharedString, String)],
    ) -> Result<u64> {
        let (database, key) = Self::locate(path)?;
        if path.len() < 2 {
            bail!("expected a key, got a database");
        }
        let value = |column: &str| {
            values
                .iter()
                .find(|(name, _)| name.as_ref() == column)
                .map(|(_, value)| value.as_str())
                .with_context(|| format!("a new item needs a {column}"))
        };
        let mut connection = self.connection(database).await?;
        let kind: String = redis::cmd("TYPE")
            .arg(&key)
            .query_async(&mut connection)
            .await
            .context("reading the key's type")?;
        // Each add refuses to replace an existing item: changing one is what editing a cell is
        // for, and a new row that silently overwrote one would lose its old value.
        let added: u64 = match kind.as_str() {
            "list" => {
                redis::cmd("RPUSH")
                    .arg(&key)
                    .arg(value("value")?)
                    .exec_async(&mut connection)
                    .await
                    .with_context(|| format!("pushing onto {key}"))?;
                1
            }
            "hash" => redis::cmd("HSETNX")
                .arg(&key)
                .arg(value("field")?)
                .arg(value("value")?)
                .query_async(&mut connection)
                .await
                .with_context(|| format!("adding a field to {key}"))?,
            "set" => redis::cmd("SADD")
                .arg(&key)
                .arg(value("member")?)
                .query_async(&mut connection)
                .await
                .with_context(|| format!("adding a member to {key}"))?,
            "zset" => {
                let score: f64 = value("score")?
                    .trim()
                    .parse()
                    .context("the score is not a number")?;
                redis::cmd("ZADD")
                    .arg(&key)
                    .arg("NX")
                    .arg(score)
                    .arg(value("member")?)
                    .query_async(&mut connection)
                    .await
                    .with_context(|| format!("adding a member to {key}"))?
            }
            "stream" => {
                let fields: serde_json::Map<String, serde_json::Value> =
                    serde_json::from_str(value("fields")?)
                        .context("the fields must be a JSON object, e.g. {\"name\": \"value\"}")?;
                if fields.is_empty() {
                    bail!("a stream entry needs at least one field");
                }
                let mut command = redis::cmd("XADD");
                command.arg(&key).arg("*");
                for (field, value) in fields {
                    let value = match value {
                        serde_json::Value::String(text) => text,
                        other => other.to_string(),
                    };
                    command.arg(field).arg(value);
                }
                command
                    .exec_async(&mut connection)
                    .await
                    .with_context(|| format!("adding an entry to {key}"))?;
                1
            }
            "none" => bail!("{key} no longer exists"),
            other => bail!("rows cannot be added to a Redis {other}"),
        };
        if added == 0 {
            bail!("{key} already has that item; nothing was added");
        }
        Ok(added)
    }

    async fn delete_row(&self, path: &[SharedString], row_key: &RowKey) -> Result<u64> {
        let (database, key) = Self::locate(path)?;
        if path.len() < 2 {
            bail!("expected a key, got a database");
        }
        let (_, item) = row_key
            .first()
            .context("the row does not say which item it is")?;
        let item = item.as_deref().context("the row's key is empty")?;
        let mut connection = self.connection(database).await?;
        let kind: String = redis::cmd("TYPE")
            .arg(&key)
            .query_async(&mut connection)
            .await
            .context("reading the key's type")?;
        let command = match kind.as_str() {
            "hash" => "HDEL",
            "set" => "SREM",
            "zset" => "ZREM",
            "stream" => "XDEL",
            "list" => {
                // A list item has only its position, and LREM removes by value, so the item is
                // first overwritten with a value nothing else holds and that value removed.
                let index: i64 = item.parse().context("the list index is not a number")?;
                let marker = format!(
                    "\u{0}quarry-deleted-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|elapsed| elapsed.as_nanos())
                        .unwrap_or_default()
                );
                let (_, removed): ((), u64) = redis::pipe()
                    .atomic()
                    .cmd("LSET")
                    .arg(&key)
                    .arg(index)
                    .arg(&marker)
                    .cmd("LREM")
                    .arg(&key)
                    .arg(1)
                    .arg(&marker)
                    .query_async(&mut connection)
                    .await
                    .with_context(|| format!("deleting {key}[{index}]"))?;
                return Ok(removed);
            }
            "none" => bail!("{key} no longer exists"),
            other => bail!("rows cannot be deleted from a Redis {other}"),
        };
        let removed: u64 = redis::cmd(command)
            .arg(&key)
            .arg(item)
            .query_async(&mut connection)
            .await
            .with_context(|| format!("deleting {item} from {key}"))?;
        if removed == 0 {
            bail!("{key} no longer has {item:?}; nothing was deleted");
        }
        Ok(removed)
    }

    async fn update_cell(&self, edit: &CellEdit) -> Result<u64> {
        let (database, key) = Self::locate(&edit.path)?;
        if edit.path.len() < 2 {
            bail!("expected a key, got a database");
        }
        let Some(value) = edit.value.as_deref() else {
            bail!("Redis has no NULL; set an empty string instead");
        };
        let mut connection = self.connection(database).await?;
        let kind: String = redis::cmd("TYPE")
            .arg(&key)
            .query_async(&mut connection)
            .await
            .context("reading the key's type")?;

        match (kind.as_str(), edit.column.as_ref()) {
            ("string", "value") => {
                let current: Vec<u8> = redis::cmd("GETRANGE")
                    .arg(&key)
                    .arg(0)
                    .arg(HYPERLOGLOG_HEADER.len() - 1)
                    .query_async(&mut connection)
                    .await
                    .with_context(|| format!("reading {key}"))?;
                if current == HYPERLOGLOG_HEADER {
                    bail!("{key} is a HyperLogLog and cannot be edited as text");
                }
                redis::cmd("SET")
                    .arg(&key)
                    .arg(value)
                    .arg("KEEPTTL")
                    .exec_async(&mut connection)
                    .await
                    .with_context(|| format!("setting {key}"))?;
                Ok(1)
            }
            ("hash", "value") => {
                let field = key_value(edit, "field")?;
                // HSET would quietly recreate a field someone else deleted since the preview.
                let exists: bool = redis::cmd("HEXISTS")
                    .arg(&key)
                    .arg(field)
                    .query_async(&mut connection)
                    .await
                    .with_context(|| format!("reading {key}"))?;
                if !exists {
                    bail!("{key} no longer has a field {field:?}");
                }
                redis::cmd("HSET")
                    .arg(&key)
                    .arg(field)
                    .arg(value)
                    .exec_async(&mut connection)
                    .await
                    .with_context(|| format!("setting {key} {field}"))?;
                Ok(1)
            }
            ("list", "value") => {
                let index: i64 = key_value(edit, "index")?
                    .parse()
                    .context("the list index is not a number")?;
                redis::cmd("LSET")
                    .arg(&key)
                    .arg(index)
                    .arg(value)
                    .exec_async(&mut connection)
                    .await
                    .with_context(|| format!("setting {key}[{index}]"))?;
                Ok(1)
            }
            ("zset", "score") => {
                let member = key_value(edit, "member")?;
                let score: f64 = value
                    .trim()
                    .parse()
                    .with_context(|| format!("{value:?} is not a number"))?;
                // XX only updates: a member removed since the preview is not silently re-added.
                // Its reply counts additions, which XX makes always zero, so existence is
                // checked separately.
                let existing: Option<f64> = redis::cmd("ZSCORE")
                    .arg(&key)
                    .arg(member)
                    .query_async(&mut connection)
                    .await
                    .with_context(|| format!("reading {key}"))?;
                if existing.is_none() {
                    bail!("{key} no longer has a member {member:?}");
                }
                redis::cmd("ZADD")
                    .arg(&key)
                    .arg("XX")
                    .arg(score)
                    .arg(member)
                    .exec_async(&mut connection)
                    .await
                    .with_context(|| format!("setting the score of {member} in {key}"))?;
                Ok(1)
            }
            ("none", _) => bail!("{key} no longer exists"),
            (kind, column) => bail!("the {column} of a Redis {kind} cannot be edited"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(line: &str) -> Vec<String> {
        split_arguments(line)
            .unwrap()
            .into_iter()
            .map(|argument| String::from_utf8(argument).unwrap())
            .collect()
    }

    #[test]
    fn splits_like_redis_cli() {
        assert_eq!(split("  GET   key "), ["GET", "key"]);
        assert_eq!(split(r#"SET "a b" 'c \'d\' e'"#), ["SET", "a b", "c 'd' e"]);
        assert_eq!(split(r#"SET k "x\ny\"z\x41""#), ["SET", "k", "x\ny\"zA"]);
        assert!(split_arguments(r#"SET "unterminated"#).is_err());
        assert!(split_arguments(r#"SET "a"b"#).is_err());
        assert!(split_arguments("").unwrap().is_empty());
    }

    #[test]
    fn parses_database_names() {
        assert_eq!(parse_database("db3").unwrap(), 3);
        assert_eq!(parse_database("7").unwrap(), 7);
        assert!(parse_database("users").is_err());
    }
}
