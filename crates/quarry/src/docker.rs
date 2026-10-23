//! Finds database servers running in local Docker containers and turns them into connections,
//! so a container started with `docker run -e POSTGRES_PASSWORD=...` does not have to be
//! described to Quarry a second time by hand.
//!
//! Everything comes from `docker inspect`: the image says which engine it is, the published
//! port says where it is reachable, and the environment variables the official images read on
//! first start say who to log in as.

use crate::driver::{ConnectionConfig, Engine};
use anyhow::{Context as _, Result, bail};
use serde_json::Value;

/// A container that looks like a database server, ready to be saved as a connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredServer {
    pub container: String,
    pub config: ConnectionConfig,
    /// Read from the container's environment, to be filed in the OS credential store.
    pub password: Option<String>,
}

/// Lists the running containers that serve one of Quarry's engines on a published port.
pub async fn discover() -> Result<Vec<DiscoveredServer>> {
    let listed = util::command::new_command("docker")
        .args(["ps", "--quiet", "--no-trunc"])
        .output()
        .await
        .context("running docker; is Docker installed and on the PATH?")?;
    if !listed.status.success() {
        bail!(
            "docker ps failed: {}",
            String::from_utf8_lossy(&listed.stderr).trim()
        );
    }
    let ids: Vec<String> = String::from_utf8_lossy(&listed.stdout)
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let inspected = util::command::new_command("docker")
        .arg("inspect")
        .args(&ids)
        .output()
        .await
        .context("running docker inspect")?;
    if !inspected.status.success() {
        bail!(
            "docker inspect failed: {}",
            String::from_utf8_lossy(&inspected.stderr).trim()
        );
    }
    parse_inspect(&String::from_utf8_lossy(&inspected.stdout))
}

/// Turns `docker inspect` output into the servers Quarry can connect to.
pub fn parse_inspect(json: &str) -> Result<Vec<DiscoveredServer>> {
    let containers: Vec<Value> =
        serde_json::from_str(json).context("reading the docker inspect output")?;
    let mut servers: Vec<DiscoveredServer> =
        containers.iter().filter_map(discovered_server).collect();
    servers.sort_by(|left, right| left.container.cmp(&right.container));
    Ok(servers)
}

fn discovered_server(container: &Value) -> Option<DiscoveredServer> {
    let name = container
        .get("Name")?
        .as_str()?
        .trim_start_matches('/')
        .to_string();
    let config = container.get("Config")?;
    let image = config.get("Image")?.as_str()?;
    let engine = engine_of(image)?;
    let environment: Vec<(&str, &str)> = config
        .get("Env")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|variable| variable.as_str()?.split_once('='))
        .collect();
    let variable = |name: &str| {
        environment
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_string())
            .filter(|value| !value.is_empty())
    };
    let (host, port) = published_port(container, engine)?;

    let (username, password, database) = match engine {
        Engine::Postgres => {
            // The official image's defaults: the user is `postgres`, and the database is
            // named after the user.
            let user = variable("POSTGRES_USER").unwrap_or_else(|| "postgres".to_string());
            let database = variable("POSTGRES_DB").unwrap_or_else(|| user.clone());
            (Some(user), variable("POSTGRES_PASSWORD"), Some(database))
        }
        Engine::MongoDb => (
            variable("MONGO_INITDB_ROOT_USERNAME"),
            variable("MONGO_INITDB_ROOT_PASSWORD"),
            variable("MONGO_INITDB_DATABASE"),
        ),
        Engine::SqlServer => (
            Some("sa".to_string()),
            variable("MSSQL_SA_PASSWORD").or_else(|| variable("SA_PASSWORD")),
            None,
        ),
        Engine::ClickHouse => (
            Some(variable("CLICKHOUSE_USER").unwrap_or_else(|| "default".to_string())),
            variable("CLICKHOUSE_PASSWORD"),
            variable("CLICKHOUSE_DB"),
        ),
        Engine::Redis => (
            None,
            variable("REDIS_PASSWORD").or_else(|| requirepass(config)),
            None,
        ),
    };

    Some(DiscoveredServer {
        config: ConnectionConfig {
            name: name.clone(),
            engine,
            host,
            port,
            database,
            username,
            environment: None,
            folder: Some("Docker".to_string()),
            ssh: None,
            tls: Default::default(),
        },
        password,
        container: name,
    })
}

/// The engine an image serves, judged by its repository name without registry or tag, so
/// `mcr.microsoft.com/mssql/server:2022-latest` and `bitnami/redis` are both recognised.
fn engine_of(image: &str) -> Option<Engine> {
    let repository = image.split('@').next()?;
    let repository = match repository.rsplit_once(':') {
        // A colon after the last slash is a tag; one before it is a registry port.
        Some((name, tag)) if !tag.contains('/') => name,
        _ => repository,
    }
    .to_ascii_lowercase();
    let last = repository.rsplit('/').next()?;
    if repository.contains("mssql") || last.contains("azure-sql-edge") {
        Some(Engine::SqlServer)
    } else if last.contains("postgres") || last.contains("postgis") || last == "timescaledb" {
        Some(Engine::Postgres)
    } else if last.starts_with("mongo") {
        Some(Engine::MongoDb)
    } else if last.contains("redis") || last.contains("valkey") {
        Some(Engine::Redis)
    } else if last.contains("clickhouse") {
        Some(Engine::ClickHouse)
    } else {
        None
    }
}

/// Where the engine's port is published on the host. A container that does not publish it is
/// skipped: it is only reachable from other containers on its network.
fn published_port(container: &Value, engine: Engine) -> Option<(String, u16)> {
    let key = format!("{}/tcp", engine.default_port());
    let bindings = container
        .get("NetworkSettings")?
        .get("Ports")?
        .get(&key)?
        .as_array()?;
    bindings.iter().find_map(|binding| {
        let port = binding.get("HostPort")?.as_str()?.parse().ok()?;
        let host = match binding.get("HostIp").and_then(Value::as_str) {
            None | Some("" | "0.0.0.0" | "::" | "127.0.0.1" | "::1") => "localhost".to_string(),
            Some(address) => address.to_string(),
        };
        Some((host, port))
    })
}

/// A password given on the command line, as in `redis-server --requirepass secret`.
fn requirepass(config: &Value) -> Option<String> {
    let arguments: Vec<&str> = config
        .get("Cmd")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let position = arguments
        .iter()
        .position(|argument| *argument == "--requirepass")?;
    arguments
        .get(position + 1)
        .map(|password| password.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(name: &str, image: &str, env: &[&str], ports: Value, cmd: &[&str]) -> Value {
        serde_json::json!({
            "Name": format!("/{name}"),
            "Config": { "Image": image, "Env": env, "Cmd": cmd },
            "NetworkSettings": { "Ports": ports },
        })
    }

    #[test]
    fn finds_each_engine_with_its_credentials() {
        let containers = Value::Array(vec![
            container(
                "pg",
                "postgres:17",
                &[
                    "POSTGRES_USER=app",
                    "POSTGRES_PASSWORD=secret",
                    "PATH=/usr/bin",
                ],
                serde_json::json!({ "5432/tcp": [{ "HostIp": "0.0.0.0", "HostPort": "55432" }] }),
                &["postgres"],
            ),
            container(
                "mssql",
                "mcr.microsoft.com/mssql/server:2022-latest",
                &["MSSQL_SA_PASSWORD=Pass_123!"],
                serde_json::json!({ "1433/tcp": [{ "HostIp": "", "HostPort": "1433" }] }),
                &[],
            ),
            container(
                "cache",
                "registry.local:5000/redis:7",
                &[],
                serde_json::json!({ "6379/tcp": [{ "HostIp": "127.0.0.1", "HostPort": "6380" }] }),
                &["redis-server", "--requirepass", "hunter2"],
            ),
            container(
                "unpublished",
                "mongo:8",
                &[],
                serde_json::json!({ "27017/tcp": null }),
                &[],
            ),
            container(
                "web",
                "nginx:latest",
                &[],
                serde_json::json!({ "80/tcp": [{ "HostIp": "0.0.0.0", "HostPort": "8080" }] }),
                &[],
            ),
        ]);
        let servers = parse_inspect(&containers.to_string()).unwrap();
        let names: Vec<&str> = servers
            .iter()
            .map(|server| server.container.as_str())
            .collect();
        assert_eq!(names, ["cache", "mssql", "pg"]);

        let postgres = &servers[2];
        assert_eq!(postgres.config.engine, Engine::Postgres);
        assert_eq!(postgres.config.host, "localhost");
        assert_eq!(postgres.config.port, 55432);
        assert_eq!(postgres.config.username.as_deref(), Some("app"));
        // The official image names the default database after the user.
        assert_eq!(postgres.config.database.as_deref(), Some("app"));
        assert_eq!(postgres.password.as_deref(), Some("secret"));

        assert_eq!(servers[1].config.username.as_deref(), Some("sa"));
        assert_eq!(servers[1].password.as_deref(), Some("Pass_123!"));
        assert_eq!(servers[0].config.engine, Engine::Redis);
        assert_eq!(servers[0].password.as_deref(), Some("hunter2"));
    }

    #[test]
    fn reads_registry_ports_and_tags_apart() {
        assert_eq!(
            engine_of("registry.local:5000/team/postgres"),
            Some(Engine::Postgres)
        );
        assert_eq!(engine_of("bitnami/mongodb:7.0"), Some(Engine::MongoDb));
        assert_eq!(
            engine_of("clickhouse/clickhouse-server:latest"),
            Some(Engine::ClickHouse)
        );
        assert_eq!(engine_of("mysql:8"), None);
    }
}
