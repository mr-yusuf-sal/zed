//! Offers Quarry's connections to AI agents as an MCP server, so an agent can see the schema it
//! is writing a query for and check its query against real data.
//!
//! The server speaks MCP's streamable HTTP on 127.0.0.1, which both Zed's own agent and the
//! external agents Zed starts (Claude Code among them) can be pointed at from `context_servers`
//! in settings.json. An in-process server is not an option: Zed only starts context servers
//! that settings describe as a command or a URL. The URL carries a random token kept in the
//! key-value store, so another program on the machine, or a web page, cannot use it without
//! having read Zed's database first.
//!
//! Everything an agent can do is read-only: list connections and objects, read a CREATE
//! script, and run statements that `statements::may_write` does not flag.

use crate::{
    connection_store::ConnectionStore,
    driver::{ConnectionConfig, Driver, ObjectKind, ResultSet},
    runtime::QuarryRuntime,
    statements,
};
use anyhow::{Context as _, Result, anyhow, bail};
use db::kvp::KeyValueStore;
use futures::{
    StreamExt as _,
    channel::{mpsc, oneshot},
};
use gpui::{App, AppContext as _, AsyncApp, Global};
use serde_json::{Value, json};
use std::sync::Arc;
use ui::SharedString;
use util::ResultExt as _;

/// Fixed, so the URL written to settings.json stays valid across restarts. A second Zed running
/// at the same time cannot bind it; its Quarry then serves no agents, which the log says.
const PORT: u16 = 47_212;
const TOKEN_KEY: &str = "quarry_mcp_token";
/// Rows of a result an agent gets back. Enough to see what the data looks like without
/// filling the agent's context with a table dump.
const ROW_LIMIT: usize = 200;

/// The URL agents reach Quarry's MCP server at, once it is listening.
pub struct McpEndpoint {
    pub url: String,
}

impl Global for McpEndpoint {}

struct Call {
    message: Value,
    reply: oneshot::Sender<Value>,
}

pub fn init(cx: &mut App) {
    if let Some(url) = start(PORT, cx) {
        cx.set_global(McpEndpoint { url });
    }
}

/// Starts serving on `port` and returns the URL agents use, or `None` when the port is taken.
fn start(port: u16, cx: &mut App) -> Option<String> {
    let kvp = KeyValueStore::global(cx);
    let token = match kvp.read_kvp(TOKEN_KEY).log_err().flatten() {
        Some(token) => token,
        None => {
            let token = uuid::Uuid::new_v4().simple().to_string();
            let kvp = kvp.clone();
            let saved = token.clone();
            cx.background_spawn(async move {
                kvp.write_kvp(TOKEN_KEY.to_string(), saved).await.log_err()
            })
            .detach();
            token
        }
    };
    let server = match tiny_http::Server::http(("127.0.0.1", port)) {
        Ok(server) => server,
        Err(error) => {
            log::warn!("Quarry cannot serve AI agents: 127.0.0.1:{port} is taken ({error})");
            return None;
        }
    };
    let path = format!("/mcp/{token}");
    let url = format!("http://127.0.0.1:{port}{path}");

    let (calls, mut incoming) = mpsc::unbounded::<Call>();
    let spawned = std::thread::Builder::new()
        .name("quarry-mcp".to_string())
        .spawn(move || serve(server, path, calls));
    if let Err(error) = spawned {
        log::error!("Quarry could not start its MCP server: {error}");
        return None;
    }
    cx.spawn(async move |cx| {
        let mut drivers = Vec::new();
        while let Some(call) = incoming.next().await {
            let response = respond(call.message, &mut drivers, cx).await;
            call.reply.send(response).ok();
        }
    })
    .detach();
    Some(url)
}

/// Answers HTTP on its own thread. Each JSON-RPC request is handed to the foreground, where
/// connections and the credential store live, and its answer awaited.
fn serve(server: tiny_http::Server, path: String, calls: mpsc::UnboundedSender<Call>) {
    let Ok(json_header) = tiny_http::Header::from_bytes("Content-Type", "application/json") else {
        log::error!("Quarry could not build its MCP response header");
        return;
    };
    for mut request in server.incoming_requests() {
        if request.url() != path {
            request.respond(tiny_http::Response::empty(404)).log_err();
            continue;
        }
        let response = match request.method() {
            tiny_http::Method::Post => {
                let mut body = String::new();
                let message = request
                    .as_reader()
                    .read_to_string(&mut body)
                    .ok()
                    .and_then(|_| serde_json::from_str::<Value>(&body).ok());
                match message {
                    // A notification, such as `notifications/initialized`, has no id and gets
                    // no answer beyond being accepted.
                    Some(message) if message.get("id").is_none() => {
                        tiny_http::Response::from_string("").with_status_code(202)
                    }
                    Some(message) => {
                        let (reply, answer) = oneshot::channel();
                        if calls.unbounded_send(Call { message, reply }).is_err() {
                            return;
                        }
                        match futures::executor::block_on(answer) {
                            Ok(answer) => tiny_http::Response::from_string(answer.to_string())
                                .with_header(json_header.clone()),
                            Err(_) => return,
                        }
                    }
                    None => tiny_http::Response::from_string(
                        json!({
                            "jsonrpc": "2.0",
                            "id": null,
                            "error": { "code": -32700, "message": "the body is not JSON" },
                        })
                        .to_string(),
                    )
                    .with_status_code(400)
                    .with_header(json_header.clone()),
                }
            }
            // No server-initiated messages, so no event stream to open.
            tiny_http::Method::Get => tiny_http::Response::from_string("").with_status_code(405),
            tiny_http::Method::Delete => tiny_http::Response::from_string("").with_status_code(200),
            _ => tiny_http::Response::from_string("").with_status_code(405),
        };
        request.respond(response).log_err();
    }
}

async fn respond(
    message: Value,
    drivers: &mut Vec<(ConnectionConfig, Arc<dyn Driver>)>,
    cx: &mut AsyncApp,
) -> Value {
    let id = message.get("id").cloned().unwrap_or(Value::Null);
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": params
                .get("protocolVersion")
                .cloned()
                .unwrap_or_else(|| json!("2025-06-18")),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "quarry", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Quarry is the database browser in Zed. Use list_connections to see \
                the saved databases, list_objects to walk one's schemas and tables, \
                describe_object for a table's CREATE script, and run_query to read data. \
                run_query refuses anything that could change data.",
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
            Ok(match call_tool(&name, &arguments, drivers, cx).await {
                Ok(text) => json!({ "content": [{ "type": "text", "text": text }] }),
                Err(error) => json!({
                    "content": [{ "type": "text", "text": format!("{error:#}") }],
                    "isError": true,
                }),
            })
        }
        other => Err(json!({ "code": -32601, "message": format!("unknown method {other:?}") })),
    };
    match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
    }
}

fn tool_definitions() -> Value {
    let connection = json!({
        "type": ["integer", "string"],
        "description": "The connection's id from list_connections, or its name when that is unique.",
    });
    let path = json!({
        "type": "array",
        "items": { "type": "string" },
        "description": "The object's path as list_objects reports it, e.g. [\"app\", \"public\", \"orders\"].",
    });
    json!([
        {
            "name": "list_connections",
            "description": "Lists the databases saved in Quarry: id, name, engine, host, default database and environment (dev, staging, prod).",
            "inputSchema": { "type": "object", "properties": {} },
        },
        {
            "name": "list_objects",
            "description": "Lists what is inside an object of a connection: databases at the top, then schemas, tables, views, routines, columns and indexes (MongoDB: collections and fields; Redis: keys grouped by ':'). Leave path out for the top level.",
            "inputSchema": {
                "type": "object",
                "properties": { "connection": connection, "path": path },
                "required": ["connection"],
            },
        },
        {
            "name": "describe_object",
            "description": "Returns the CREATE script of a table or collection (columns, keys, constraints, indexes), or the definition of a view or routine.",
            "inputSchema": {
                "type": "object",
                "properties": { "connection": connection, "path": path },
                "required": ["connection", "path"],
            },
        },
        {
            "name": "run_query",
            "description": format!("Runs read-only statements and returns up to {ROW_LIMIT} rows of each result as JSON. Anything that could change data or schema is refused. SQL for PostgreSQL, SQL Server and ClickHouse; a JSON command document for MongoDB (e.g. {{\"find\": \"users\", \"limit\": 10}}); a command line for Redis."),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "connection": connection,
                    "statement": { "type": "string" },
                    "database": { "type": "string", "description": "The database to run in, when not the connection's default." },
                },
                "required": ["connection", "statement"],
            },
        },
    ])
}

async fn call_tool(
    name: &str,
    arguments: &Value,
    drivers: &mut Vec<(ConnectionConfig, Arc<dyn Driver>)>,
    cx: &mut AsyncApp,
) -> Result<String> {
    let connections = cx.update(|cx| {
        ConnectionStore::global(cx)
            .map(|store| store.read(cx).connections().to_vec())
            .unwrap_or_default()
    });
    if name == "list_connections" {
        let listed: Vec<Value> = connections
            .iter()
            .enumerate()
            .map(|(index, config)| {
                json!({
                    "id": index + 1,
                    "name": config.name,
                    "engine": config.engine.label(),
                    "host": config.host,
                    "port": config.port,
                    "database": config.database,
                    "environment": config.environment.map(|environment| environment.label()),
                    "folder": config.folder,
                })
            })
            .collect();
        return Ok(serde_json::to_string_pretty(&listed)?);
    }

    let config = find_connection(&connections, arguments.get("connection"))?;
    let path: Vec<SharedString> = match arguments.get("path") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(segments)) => segments
            .iter()
            .map(|segment| match segment {
                Value::String(text) => Ok(SharedString::from(text.clone())),
                other => Err(anyhow!("path segments are strings, got {other}")),
            })
            .collect::<Result<_>>()?,
        Some(other) => bail!("path must be an array of strings, got {other}"),
    };
    let driver = driver_for(config, drivers, cx).await?;
    let runtime = cx
        .update(|cx| QuarryRuntime::global(cx))
        .context("the Quarry runtime is not available")?;

    match name {
        "list_objects" => {
            let children = runtime
                .spawn(async move { driver.children(&path).await })
                .await?;
            let listed: Vec<Value> = children
                .into_iter()
                .map(|node| {
                    json!({
                        "name": node.name.as_ref(),
                        "kind": kind_name(node.kind),
                        "detail": node.detail.as_deref(),
                        "path": node.path.iter().map(|segment| segment.as_ref()).collect::<Vec<_>>(),
                        "has_children": node.has_children,
                    })
                })
                .collect();
            Ok(serde_json::to_string_pretty(&listed)?)
        }
        "describe_object" => {
            if path.is_empty() {
                bail!("describe_object needs the path of a table, collection, view or routine");
            }
            runtime
                .spawn(async move {
                    match driver.create_script(&path).await {
                        Ok(script) => Ok(script),
                        Err(_) => driver.definition(&path).await,
                    }
                })
                .await
        }
        "run_query" => {
            let text = arguments
                .get("statement")
                .and_then(Value::as_str)
                .context("run_query needs a statement")?;
            let engine = config.engine;
            let pieces: Vec<String> = statements::split(text, engine)
                .into_iter()
                .filter_map(|range| text.get(range).map(str::to_string))
                .collect();
            if pieces.is_empty() {
                bail!("the statement is empty");
            }
            if let Some(write) = pieces
                .iter()
                .find(|piece| statements::may_write(piece, engine))
            {
                bail!(
                    "refused: Quarry only runs read-only statements for agents, and this may change data or schema: {write}"
                );
            }
            let database = arguments
                .get("database")
                .and_then(Value::as_str)
                .map(str::to_string);
            let results = runtime
                .spawn(async move {
                    let mut results = Vec::new();
                    for piece in pieces {
                        results.extend(driver.query(&piece, database.as_deref(), ROW_LIMIT).await?);
                    }
                    anyhow::Ok(results)
                })
                .await?;
            Ok(serde_json::to_string_pretty(
                &results.iter().map(result_json).collect::<Vec<_>>(),
            )?)
        }
        other => bail!("unknown tool {other:?}"),
    }
}

/// A connection by the 1-based id `list_connections` gives, or by a name only one has.
fn find_connection<'a>(
    connections: &'a [ConnectionConfig],
    reference: Option<&Value>,
) -> Result<&'a ConnectionConfig> {
    let by_id = |id: u64| {
        usize::try_from(id)
            .ok()
            .and_then(|id| id.checked_sub(1))
            .and_then(|index| connections.get(index))
            .with_context(|| format!("there is no connection {id}; see list_connections"))
    };
    match reference {
        Some(Value::Number(number)) => by_id(number.as_u64().context("ids are positive integers")?),
        Some(Value::String(text)) => {
            if let Ok(id) = text.trim().parse::<u64>() {
                return by_id(id);
            }
            let mut matches = connections.iter().filter(|config| config.name == *text);
            match (matches.next(), matches.next()) {
                (Some(config), None) => Ok(config),
                (None, _) => bail!("there is no connection named {text:?}; see list_connections"),
                (Some(_), Some(_)) => {
                    bail!(
                        "several connections are named {text:?}; use the id from list_connections"
                    )
                }
            }
        }
        _ => bail!("connection is required: an id or a name from list_connections"),
    }
}

/// The open driver for `config`, connecting the first time it is asked for. Kept by config,
/// so an edited connection gets a new driver rather than the old session.
async fn driver_for(
    config: &ConnectionConfig,
    drivers: &mut Vec<(ConnectionConfig, Arc<dyn Driver>)>,
    cx: &mut AsyncApp,
) -> Result<Arc<dyn Driver>> {
    if let Some((_, driver)) = drivers.iter().find(|(open, _)| open == config) {
        return Ok(driver.clone());
    }
    let driver = crate::connect(config.clone(), cx)
        .await
        .with_context(|| format!("connecting to {}", config.name))?;
    drivers.push((config.clone(), driver.clone()));
    Ok(driver)
}

fn result_json(result: &ResultSet) -> Value {
    if result.columns.is_empty() {
        return json!({ "rows_affected": result.rows_affected });
    }
    json!({
        "columns": result.columns.iter().map(|column| column.as_ref()).collect::<Vec<_>>(),
        "rows": result
            .rows
            .iter()
            .map(|row| row.iter().map(|cell| cell.as_deref()).collect::<Vec<_>>())
            .collect::<Vec<_>>(),
        "truncated": result.truncated,
        "notices": result.notices.iter().map(|notice| notice.as_ref()).collect::<Vec<_>>(),
    })
}

fn kind_name(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Database => "database",
        ObjectKind::Schema => "schema",
        ObjectKind::Table => "table",
        ObjectKind::View => "view",
        ObjectKind::Column => "column",
        ObjectKind::Index => "index",
        ObjectKind::Collection => "collection",
        ObjectKind::Key => "key",
        ObjectKind::Folder => "folder",
        ObjectKind::Routine => "routine",
        ObjectKind::Sequence => "sequence",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection_store::parse_connection_url;
    use gpui::TestAppContext;
    use std::io::{Read as _, Write as _};

    /// POSTs `message` from a thread of its own, since the server answers only after the
    /// foreground the test runs on has handled the call.
    async fn post(url: &str, message: Value) -> Value {
        let url = url.to_string();
        let (sender, receiver) = oneshot::channel();
        std::thread::spawn(move || {
            let address = url
                .trim_start_matches("http://")
                .split_once('/')
                .map(|(address, _)| address.to_string())
                .unwrap_or_default();
            let path = url
                .trim_start_matches("http://")
                .split_once('/')
                .map(|(_, path)| format!("/{path}"))
                .unwrap_or_default();
            let body = message.to_string();
            let mut stream = std::net::TcpStream::connect(&address).unwrap();
            write!(
                stream,
                "POST {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            sender.send(response).ok();
        });
        let response = receiver.await.unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        if body.is_empty() {
            return json!({ "status": head.lines().next().unwrap_or_default() });
        }
        serde_json::from_str(body).unwrap()
    }

    fn tool_text(response: &Value) -> String {
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    #[gpui::test]
    #[ignore = "needs the quarry redis on :6380"]
    async fn test_agents_read_but_never_write(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let url = cx.update(|cx| {
            QuarryRuntime::init(cx);
            ConnectionStore::set_global_for_test(
                vec![parse_connection_url("redis://localhost:6380").unwrap()],
                cx,
            );
            start(47_299, cx).unwrap()
        });

        let initialized = post(
            &url,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
                    "params": { "protocolVersion": "2025-06-18" } }),
        )
        .await;
        assert_eq!(initialized["result"]["serverInfo"]["name"], "quarry");
        let accepted = post(
            &url,
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        )
        .await;
        assert!(accepted["status"].as_str().unwrap().contains("202"));

        let tools = post(
            &url,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
        )
        .await;
        let names: Vec<&str> = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert_eq!(
            names,
            [
                "list_connections",
                "list_objects",
                "describe_object",
                "run_query"
            ]
        );

        let call = |id: u64, name: &str, arguments: Value| {
            json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call",
                    "params": { "name": name, "arguments": arguments } })
        };
        let listed = post(&url, call(3, "list_connections", json!({}))).await;
        assert!(tool_text(&listed).contains("\"engine\": \"Redis\""));

        let databases = post(&url, call(4, "list_objects", json!({ "connection": 1 }))).await;
        assert!(tool_text(&databases).contains("db0"), "{databases}");

        let size = post(
            &url,
            call(
                5,
                "run_query",
                json!({ "connection": "1", "statement": "DBSIZE" }),
            ),
        )
        .await;
        assert!(size["result"]["isError"].is_null(), "{size}");
        assert!(tool_text(&size).contains("\"reply\""));

        // Refused before it reaches the server: the keyspace is untouched.
        let flushed = post(
            &url,
            call(
                6,
                "run_query",
                json!({ "connection": 1, "statement": "FLUSHDB" }),
            ),
        )
        .await;
        assert_eq!(flushed["result"]["isError"], true);
        assert!(tool_text(&flushed).starts_with("refused"));

        let unknown = post(&url, json!({ "jsonrpc": "2.0", "id": 7, "method": "nope" })).await;
        assert_eq!(unknown["error"]["code"], -32601);

        let wrong_path = url.replace("/mcp/", "/mcp/not-the-token");
        let refused = std::thread::spawn(move || {
            let address = wrong_path
                .trim_start_matches("http://")
                .split_once('/')
                .map(|(address, _)| address.to_string())
                .unwrap_or_default();
            let mut stream = std::net::TcpStream::connect(&address).unwrap();
            write!(
                stream,
                "POST /mcp/not-the-token HTTP/1.1\r\nHost: {address}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            )
            .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        })
        .join()
        .unwrap();
        assert!(refused.starts_with("HTTP/1.1 404"), "{refused}");
    }
}
