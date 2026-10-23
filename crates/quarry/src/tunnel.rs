//! Reaching a database through an SSH bastion. The tunnel is the system's own `ssh -L`, not an
//! SSH implementation in Quarry: that way the user's keys, agent, `~/.ssh/config` and known
//! hosts all apply exactly as they do in a terminal, and nothing about logging in has to be
//! asked for or stored here.

use crate::driver::{
    CatalogEntry, CellEdit, Driver, KeyAction, ObjectNode, ResultSet, RowKey, SshTunnel,
};
use anyhow::{Context as _, Result, bail};
use std::{
    io::Read as _,
    process::{Child, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
use ui::SharedString;
use util::ResultExt as _;

/// How long ssh gets to log in and start forwarding before the connection is given up on.
const OPEN_TIMEOUT: Duration = Duration::from_secs(30);

/// A running `ssh -N -L` forwarding a local port to the database. Dropping it ends the process.
pub struct Tunnel {
    child: Child,
    pub local_port: u16,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.child.kill().log_err();
        self.child.wait().log_err();
    }
}

/// Starts ssh forwarding a free local port to `host:port` as `ssh` sees it, and waits until the
/// port accepts connections, which ssh only does once it has logged in. `BatchMode` makes ssh
/// fail instead of prompting, since there is no terminal to answer in: keys or the agent have
/// to log in. A bastion never seen before has its host key accepted and remembered, as the
/// first `ssh` to it in a terminal would after a "yes"; a key that has changed since is still
/// refused.
pub async fn open(ssh: &SshTunnel, host: &str, port: u16) -> Result<Tunnel> {
    let local_port = {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
            .context("finding a free local port for the tunnel")?;
        listener.local_addr()?.port()
    };
    let destination = match &ssh.user {
        Some(user) => format!("{user}@{}", ssh.host),
        None => ssh.host.clone(),
    };
    let mut command = util::command::new_std_command("ssh");
    if let Some(identity_file) = &ssh.identity_file {
        command.args(["-i", identity_file, "-o", "IdentitiesOnly=yes"]);
    }
    let mut child = command
        .args([
            "-N",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=accept-new",
            "-o",
            "ServerAliveInterval=30",
            "-o",
            "ConnectTimeout=15",
            "-p",
            &ssh.port.to_string(),
            "-L",
            &format!("127.0.0.1:{local_port}:{host}:{port}"),
            &destination,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting ssh; is OpenSSH installed and on the PATH?")?;

    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().context("watching ssh")? {
            let mut message = String::new();
            if let Some(stderr) = child.stderr.as_mut() {
                stderr.read_to_string(&mut message).log_err();
            }
            bail!("ssh to {ssh} exited ({status}): {}", message.trim());
        }
        if tokio::net::TcpStream::connect(("127.0.0.1", local_port))
            .await
            .is_ok()
        {
            return Ok(Tunnel { child, local_port });
        }
        if started.elapsed() > OPEN_TIMEOUT {
            child.kill().log_err();
            child.wait().log_err();
            bail!(
                "ssh to {ssh} did not start forwarding within {} seconds",
                OPEN_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A driver connected through a tunnel, keeping the tunnel open exactly as long as the driver
/// is in use. Every method is passed straight through; a method added to `Driver` has to be
/// added here too, or a tunnelled connection silently gets the trait's default instead.
pub struct TunneledDriver {
    inner: Arc<dyn Driver>,
    _tunnel: Tunnel,
}

impl TunneledDriver {
    pub fn new(inner: Arc<dyn Driver>, tunnel: Tunnel) -> Self {
        Self {
            inner,
            _tunnel: tunnel,
        }
    }
}

#[async_trait::async_trait]
impl Driver for TunneledDriver {
    async fn children(&self, path: &[SharedString]) -> Result<Vec<ObjectNode>> {
        self.inner.children(path).await
    }

    async fn query(
        &self,
        statement: &str,
        database: Option<&str>,
        row_limit: usize,
    ) -> Result<Vec<ResultSet>> {
        self.inner.query(statement, database, row_limit).await
    }

    async fn preview(&self, path: &[SharedString], row_limit: usize) -> Result<ResultSet> {
        self.inner.preview(path, row_limit).await
    }

    async fn preview_page(
        &self,
        path: &[SharedString],
        row_limit: usize,
        offset: usize,
    ) -> Result<ResultSet> {
        self.inner.preview_page(path, row_limit, offset).await
    }

    async fn cancel(&self) -> Result<()> {
        self.inner.cancel().await
    }

    async fn databases(&self) -> Result<Vec<SharedString>> {
        self.inner.databases().await
    }

    fn default_database(&self) -> Option<SharedString> {
        self.inner.default_database()
    }

    async fn catalog(&self, database: Option<&str>) -> Result<Vec<CatalogEntry>> {
        self.inner.catalog(database).await
    }

    async fn definition(&self, path: &[SharedString]) -> Result<String> {
        self.inner.definition(path).await
    }

    async fn create_script(&self, path: &[SharedString]) -> Result<String> {
        self.inner.create_script(path).await
    }

    fn qualified_name(&self, path: &[SharedString]) -> Option<String> {
        self.inner.qualified_name(path)
    }

    fn select_statement(&self, path: &[SharedString], limit: usize) -> Option<String> {
        self.inner.select_statement(path, limit)
    }

    fn count_statement(&self, path: &[SharedString]) -> Option<String> {
        self.inner.count_statement(path)
    }

    async fn explain(
        &self,
        statement: &str,
        database: Option<&str>,
        analyze: bool,
    ) -> Result<Vec<ResultSet>> {
        self.inner.explain(statement, database, analyze).await
    }

    async fn search(&self, path: &[SharedString], pattern: &str) -> Result<ResultSet> {
        self.inner.search(path, pattern).await
    }

    async fn key_action(&self, path: &[SharedString], action: &KeyAction) -> Result<()> {
        self.inner.key_action(path, action).await
    }

    async fn insert_row(
        &self,
        path: &[SharedString],
        values: &[(SharedString, String)],
    ) -> Result<u64> {
        self.inner.insert_row(path, values).await
    }

    async fn delete_row(&self, path: &[SharedString], key: &RowKey) -> Result<u64> {
        self.inner.delete_row(path, key).await
    }

    async fn update_cell(&self, edit: &CellEdit) -> Result<u64> {
        self.inner.update_cell(edit).await
    }
}
