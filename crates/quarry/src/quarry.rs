//! Quarry connects Zed to a database server and lets you look around inside it: browse the
//! object tree, page through a table, and run queries.
//!
//! The engine-specific parts live behind [`driver::Driver`]. Everything above that — the tree,
//! the query view, credential handling — is written once and knows nothing about Postgres.

pub mod completion;
pub mod connection_store;
pub mod docker;
pub mod driver;
pub mod drivers;
pub mod panel;
pub mod query_view;
pub mod result_table;
pub mod runtime;
pub mod schema_diff;
pub mod statements;
pub mod templates;
pub mod tunnel;

use anyhow::{Context as _, Result, anyhow};
use driver::{ConnectionConfig, Driver, Engine};
use gpui::App;
use runtime::QuarryRuntime;
use std::sync::Arc;

pub fn init(cx: &mut App) {
    QuarryRuntime::init(cx);
    connection_store::ConnectionStore::init(cx);
    panel::init(cx);
    workspace::register_serializable_item::<query_view::QueryView>(cx);
}

/// Opens a connection, fetching the password from the OS credential store on the way.
///
/// The password is read on the gpui side because that is where the platform keychain lives, and
/// handed to the driver as a borrowed string so it is not copied into a long-lived config. It
/// is never written to settings or to the workspace database.
pub async fn connect(config: ConnectionConfig, cx: &mut gpui::AsyncApp) -> Result<Arc<dyn Driver>> {
    let credential_url = config.credential_url();
    let credentials = cx
        .update(|cx| cx.read_credentials(&credential_url))
        .await
        .with_context(|| format!("reading saved credentials for {credential_url}"))?;

    let password = credentials
        .as_ref()
        .map(|(_, password)| String::from_utf8_lossy(password).into_owned());
    // A username stored alongside the password wins over the one in the config: it is the one
    // the user actually authenticated with last time.
    let username = credentials
        .as_ref()
        .map(|(username, _)| username.clone())
        .or_else(|| config.username.clone());

    open_driver(config, username, password, cx).await
}

/// Connects with `password` instead of the saved one, for the Test button on the connection
/// form: the point is to try what was just typed before it is saved anywhere. A blank password
/// falls back to the saved one, which is what saving the form would do too.
pub async fn test_connection(
    config: ConnectionConfig,
    password: Option<String>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    match password {
        Some(password) => {
            let username = config.username.clone();
            open_driver(config, username, Some(password), cx).await?;
        }
        None => {
            connect(config, cx).await?;
        }
    }
    Ok(())
}

async fn open_driver(
    mut config: ConnectionConfig,
    username: Option<String>,
    password: Option<String>,
    cx: &mut gpui::AsyncApp,
) -> Result<Arc<dyn Driver>> {
    let runtime = cx
        .update(|cx| QuarryRuntime::global(cx))
        .ok_or_else(|| anyhow!("the Quarry runtime failed to start; see the log"))?;

    // Through a bastion, the driver connects to the tunnel's local end instead. The config is
    // only a copy, so the saved host and port, which name the server as the bastion sees it
    // and file its password, are left alone.
    let tunnel = match config.ssh.clone() {
        Some(ssh) => {
            let host = config.host.clone();
            let port = config.port;
            let description = ssh.to_string();
            let tunnel = runtime
                .spawn(async move { tunnel::open(&ssh, &host, port).await })
                .await
                .with_context(|| format!("opening the SSH tunnel through {description}"))?;
            config.host = "127.0.0.1".to_string();
            config.port = tunnel.local_port;
            Some(tunnel)
        }
        None => None,
    };
    let tls = config.tls;

    let driver: Arc<dyn Driver> = match config.engine {
        Engine::Postgres => {
            let host = config.host.clone();
            let database = config.database.clone();
            runtime
                .spawn(async move {
                    let driver = drivers::postgres::PostgresDriver::connect_with(
                        &host,
                        config.port,
                        database.as_deref(),
                        username.as_deref(),
                        password.as_deref(),
                        tls,
                    )
                    .await?;
                    Ok(Arc::new(driver) as Arc<dyn Driver>)
                })
                .await?
        }
        Engine::SqlServer => {
            open(
                &runtime,
                &config,
                username,
                password,
                move |h, p, d, u, pw| {
                    Box::pin(async move {
                        Ok(Arc::new(
                            drivers::sqlserver::SqlServerDriver::connect_with(
                                &h,
                                p,
                                d.as_deref(),
                                u.as_deref(),
                                pw.as_deref(),
                                tls,
                            )
                            .await?,
                        ) as Arc<dyn Driver>)
                    })
                },
            )
            .await?
        }
        Engine::MongoDb => {
            open(
                &runtime,
                &config,
                username,
                password,
                move |h, p, d, u, pw| {
                    Box::pin(async move {
                        Ok(Arc::new(
                            drivers::mongodb::MongoDriver::connect_with(
                                &h,
                                p,
                                d.as_deref(),
                                u.as_deref(),
                                pw.as_deref(),
                                tls,
                            )
                            .await?,
                        ) as Arc<dyn Driver>)
                    })
                },
            )
            .await?
        }
        Engine::Redis => {
            open(
                &runtime,
                &config,
                username,
                password,
                move |h, p, d, u, pw| {
                    Box::pin(async move {
                        Ok(Arc::new(
                            drivers::redis::RedisDriver::connect_with(
                                &h,
                                p,
                                d.as_deref(),
                                u.as_deref(),
                                pw.as_deref(),
                                tls,
                            )
                            .await?,
                        ) as Arc<dyn Driver>)
                    })
                },
            )
            .await?
        }
        Engine::ClickHouse => {
            open(
                &runtime,
                &config,
                username,
                password,
                move |h, p, d, u, pw| {
                    Box::pin(async move {
                        Ok(Arc::new(
                            drivers::clickhouse::ClickHouseDriver::connect_with(
                                &h,
                                p,
                                d.as_deref(),
                                u.as_deref(),
                                pw.as_deref(),
                                tls,
                            )
                            .await?,
                        ) as Arc<dyn Driver>)
                    })
                },
            )
            .await?
        }
    };

    Ok(match tunnel {
        Some(tunnel) => Arc::new(tunnel::TunneledDriver::new(driver, tunnel)),
        None => driver,
    })
}

/// Every engine connects the same way — move the owned config fields onto the Quarry runtime,
/// build the driver there, hand back a trait object — so only the driver constructor differs.
/// The closure returns a boxed future because each driver's is a different concrete type.
fn open<F>(
    runtime: &QuarryRuntime,
    config: &ConnectionConfig,
    username: Option<String>,
    password: Option<String>,
    build: F,
) -> impl std::future::Future<Output = Result<Arc<dyn Driver>>> + Send + 'static
where
    F: FnOnce(
            String,
            u16,
            Option<String>,
            Option<String>,
            Option<String>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Arc<dyn Driver>>> + Send>,
        > + Send
        + 'static,
{
    let host = config.host.clone();
    let port = config.port;
    let database = config.database.clone();
    runtime.spawn(async move { build(host, port, database, username, password).await })
}

/// Stores the password for `config` in the OS credential store, replacing any previous one.
pub async fn save_password(
    config: &ConnectionConfig,
    username: &str,
    password: &str,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let url = config.credential_url();
    cx.update(|cx| cx.write_credentials(&url, username, password.as_bytes()))
        .await
        .with_context(|| format!("saving credentials for {url}"))
}
