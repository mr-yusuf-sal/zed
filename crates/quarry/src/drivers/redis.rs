use crate::driver::{Driver, ObjectKind, ObjectNode, ResultSet};
use anyhow::{Context as _, Result, bail};
use redis::{AsyncCommands as _, Value, aio::MultiplexedConnection};
use tokio::sync::Mutex;
use ui::SharedString;

/// Redis has no tables and no query language, so it is the one engine that does not map onto
/// rows and columns. The tree shows the keyspace instead, grouped by the `:` convention almost
/// every codebase uses for namespacing, and the editor runs commands rather than queries.
pub struct RedisDriver {
    // One multiplexed connection is enough: it pipelines concurrent commands over a single
    // socket, and a browser never needs more than that.
    connection: Mutex<MultiplexedConnection>,
}

impl RedisDriver {
    pub async fn connect(
        host: &str,
        port: u16,
        database: Option<&str>,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<Self> {
        let mut url = String::from("redis://");
        if let Some(username) = username {
            url.push_str(username);
            if password.is_some() {
                url.push(':');
            }
        }
        if let Some(password) = password {
            if username.is_none() {
                url.push(':');
            }
            url.push_str(password);
        }
        if username.is_some() || password.is_some() {
            url.push('@');
        }
        url.push_str(&format!("{host}:{port}"));
        // Redis numbers its databases; the "database" part of the URL is that number.
        if let Some(database) = database {
            url.push('/');
            url.push_str(database);
        }

        let client = redis::Client::open(url).context("building the Redis client")?;
        let connection = client
            .get_multiplexed_async_connection()
            .await
            .with_context(|| format!("connecting to redis at {host}:{port}"))?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Walks the keyspace with SCAN rather than KEYS. KEYS blocks the server for the whole
    /// sweep, which on a production instance is a real outage; SCAN is incremental and is what
    /// Redis itself tells you to use.
    async fn scan(&self, pattern: &str, limit: usize) -> Result<Vec<String>> {
        let mut connection = self.connection.lock().await;
        let mut cursor: u64 = 0;
        let mut keys = Vec::new();
        loop {
            let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(pattern)
                .arg("COUNT")
                .arg(500)
                .query_async(&mut *connection)
                .await
                .context("scanning the keyspace")?;
            keys.extend(batch);
            cursor = next;
            if cursor == 0 || keys.len() >= limit {
                break;
            }
        }
        keys.truncate(limit);
        keys.sort();
        keys.dedup();
        Ok(keys)
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

#[async_trait::async_trait]
impl Driver for RedisDriver {
    async fn children(&self, path: &[SharedString]) -> Result<Vec<ObjectNode>> {
        // Keys are grouped by ':' one level at a time, so `user:1:name` appears as `user` then
        // `1` then `name` instead of as one flat list of thousands of entries.
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{}:", path.join(":"))
        };
        let keys = self.scan(&format!("{prefix}*"), 10_000).await?;

        let mut groups: Vec<(SharedString, bool)> = Vec::new();
        for key in keys {
            let Some(rest) = key.strip_prefix(&prefix) else {
                continue;
            };
            let (segment, is_leaf) = match rest.split_once(':') {
                Some((segment, _)) => (segment, false),
                None => (rest, true),
            };
            let segment = SharedString::from(segment.to_string());
            match groups.iter_mut().find(|(name, _)| *name == segment) {
                // A prefix that is also a key of its own stays expandable.
                Some((_, leaf)) => *leaf &= is_leaf,
                None => groups.push((segment, is_leaf)),
            }
        }

        let mut connection = self.connection.lock().await;
        let mut nodes = Vec::with_capacity(groups.len());
        for (name, is_leaf) in groups {
            let full = format!("{prefix}{name}");
            let detail = if is_leaf {
                let kind: String = redis::cmd("TYPE")
                    .arg(&full)
                    .query_async(&mut *connection)
                    .await
                    .unwrap_or_else(|_| String::from("unknown"));
                Some(SharedString::from(kind))
            } else {
                None
            };
            let mut node_path = path.to_vec();
            node_path.push(name.clone());
            nodes.push(ObjectNode {
                path: node_path,
                name,
                kind: ObjectKind::Key,
                has_children: !is_leaf,
                detail,
            });
        }
        Ok(nodes)
    }

    async fn query(&self, text: &str, _row_limit: usize) -> Result<ResultSet> {
        let mut parts = text.split_whitespace();
        let Some(name) = parts.next() else {
            return Ok(ResultSet::default());
        };
        let mut command = redis::cmd(name);
        for argument in parts {
            command.arg(argument);
        }

        let mut connection = self.connection.lock().await;
        let value: Value = command
            .query_async(&mut *connection)
            .await
            .with_context(|| format!("running: {}", text.trim()))?;

        let mut lines = Vec::new();
        render(&value, &mut lines, 0);
        Ok(ResultSet {
            columns: vec![SharedString::from("reply")],
            rows: lines.into_iter().map(|line| vec![line]).collect(),
            rows_affected: None,
            notices: Vec::new(),
            truncated: false,
        })
    }

    async fn preview(&self, path: &[SharedString], row_limit: usize) -> Result<ResultSet> {
        if path.is_empty() {
            bail!("expected a key");
        }
        let key = path.join(":");
        let mut connection = self.connection.lock().await;

        let kind: String = redis::cmd("TYPE")
            .arg(&key)
            .query_async(&mut *connection)
            .await
            .context("reading the key's type")?;

        // Each type needs its own read command; there is no generic "get whatever this is".
        let value: Value = match kind.as_str() {
            "string" => connection.get(&key).await,
            "list" => connection.lrange(&key, 0, row_limit as isize - 1).await,
            "set" => connection.smembers(&key).await,
            "zset" => connection.zrange(&key, 0, row_limit as isize - 1).await,
            "hash" => connection.hgetall(&key).await,
            other => bail!("{key} is a {other}, which Quarry cannot display yet"),
        }
        .with_context(|| format!("reading {key}"))?;

        let mut lines = Vec::new();
        render(&value, &mut lines, 0);
        Ok(ResultSet {
            columns: vec![SharedString::from(format!("{key} ({kind})"))],
            rows: lines.into_iter().map(|line| vec![line]).collect(),
            rows_affected: None,
            notices: Vec::new(),
            truncated: false,
        })
    }
}
