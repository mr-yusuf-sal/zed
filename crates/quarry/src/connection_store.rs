use crate::driver::{ConnectionConfig, Engine};
use anyhow::{Result, anyhow, bail};
use db::kvp::KeyValueStore;
use gpui::{App, AppContext as _, Context, Entity, Global};
use util::ResultExt as _;

/// Saved connections live in the key-value store rather than in `settings.json`.
///
/// Zed's settings system requires every key to be declared in the central `settings` crate, and
/// this is a fork that has to keep rebasing onto upstream tags — adding a field there would be
/// permanent conflict surface. The key-value store needs no upstream change, and it is where
/// this fork already keeps its other remembered state.
const CONNECTIONS_KEY: &str = "quarry_connections";

pub struct ConnectionStore {
    connections: Vec<ConnectionConfig>,
}

struct GlobalConnectionStore(Entity<ConnectionStore>);

impl Global for GlobalConnectionStore {}

impl ConnectionStore {
    pub fn init(cx: &mut App) {
        let store = cx.new(|cx| ConnectionStore::load(cx));
        cx.set_global(GlobalConnectionStore(store));
    }

    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalConnectionStore>()
            .map(|global| global.0.clone())
    }

    fn load(cx: &App) -> Self {
        let connections = KeyValueStore::global(cx)
            .read_kvp(CONNECTIONS_KEY)
            .log_err()
            .flatten()
            .and_then(|json| serde_json::from_str::<Vec<ConnectionConfig>>(&json).log_err())
            .unwrap_or_default();
        Self { connections }
    }

    pub fn connections(&self) -> &[ConnectionConfig] {
        &self.connections
    }

    pub fn get(&self, index: usize) -> Option<&ConnectionConfig> {
        self.connections.get(index)
    }

    pub fn add(&mut self, config: ConnectionConfig, cx: &mut Context<Self>) {
        self.connections.push(config);
        self.persist(cx);
        cx.notify();
    }

    pub fn remove(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.connections.len() {
            self.connections.remove(index);
            self.persist(cx);
            cx.notify();
        }
    }

    fn persist(&self, cx: &mut Context<Self>) {
        let Some(json) = serde_json::to_string(&self.connections).log_err() else {
            return;
        };
        let kvp = KeyValueStore::global(cx);
        cx.background_spawn(async move {
            kvp.write_kvp(CONNECTIONS_KEY.to_string(), json)
                .await
                .log_err()
        })
        .detach();
    }
}

/// Parses the single line a user types when adding a connection, e.g.
/// `postgres://scott@localhost:5432/store`.
///
/// A URL is asked for instead of a six-field form because it is both far less UI and the form
/// developers already have in their heads — it is what every one of these engines prints in its
/// own documentation. The password is deliberately *not* read from the URL even when present:
/// it belongs in the OS credential store, and accepting it here would invite pasting a secret
/// into something that gets saved to disk.
pub fn parse_connection_url(input: &str) -> Result<ConnectionConfig> {
    let input = input.trim();
    let (scheme, rest) = input
        .split_once("://")
        .ok_or_else(|| anyhow!("expected a URL like postgres://user@host:5432/database"))?;

    let engine = match scheme.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" => Engine::Postgres,
        "mssql" | "sqlserver" => Engine::SqlServer,
        "mongodb" => Engine::MongoDb,
        "redis" => Engine::Redis,
        "clickhouse" | "http" => Engine::ClickHouse,
        other => bail!(
            "unknown engine {other:?}; expected postgres, mssql, mongodb, redis or clickhouse"
        ),
    };

    let (authority, database) = match rest.split_once('/') {
        Some((authority, database)) if !database.is_empty() => (authority, Some(database)),
        Some((authority, _)) => (authority, None),
        None => (rest, None),
    };

    let (username, host_port) = match authority.rsplit_once('@') {
        Some((user, host_port)) => {
            // Tolerate a password in the pasted URL by dropping it rather than refusing: the
            // user gets a working connection and the secret never reaches disk.
            let user = user.split_once(':').map_or(user, |(user, _)| user);
            let user = (!user.is_empty()).then(|| user.to_string());
            (user, host_port)
        }
        None => (None, authority),
    };

    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) => {
            let port = port
                .parse::<u16>()
                .map_err(|_| anyhow!("{port:?} is not a port number"))?;
            (host, port)
        }
        None => (host_port, engine.default_port()),
    };

    if host.is_empty() {
        bail!("the URL has no host");
    }

    Ok(ConnectionConfig {
        name: match (&username, database) {
            (_, Some(database)) => format!("{host}/{database}"),
            (Some(user), None) => format!("{user}@{host}"),
            (None, None) => host.to_string(),
        },
        engine,
        host: host.to_string(),
        port,
        database: database.map(str::to_string),
        username,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_url() {
        let config = parse_connection_url("postgres://scott@localhost:5433/store").unwrap();
        assert_eq!(config.engine, Engine::Postgres);
        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 5433);
        assert_eq!(config.database.as_deref(), Some("store"));
        assert_eq!(config.username.as_deref(), Some("scott"));
    }

    #[test]
    fn falls_back_to_the_engine_default_port() {
        let config = parse_connection_url("redis://cache.internal").unwrap();
        assert_eq!(config.port, 6379);
        assert_eq!(config.username, None);
        assert_eq!(config.database, None);
    }

    #[test]
    fn drops_a_password_that_was_pasted_in() {
        let config = parse_connection_url("mongodb://admin:hunter2@db:27017/app").unwrap();
        assert_eq!(config.username.as_deref(), Some("admin"));
        // The config is serialised to disk, so nothing about the password may survive parsing.
        let json = serde_json::to_string(&config).unwrap();
        assert!(!json.contains("hunter2"), "password leaked into {json}");
    }

    #[test]
    fn rejects_input_that_is_not_a_url() {
        assert!(parse_connection_url("localhost:5432").is_err());
        assert!(parse_connection_url("ftp://localhost").is_err());
    }
}
