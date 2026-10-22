use super::debug_log::{AcpDebugLog, AcpDebugMessageDirection};
use anyhow::{Context as _, Result};
use futures::{
    AsyncBufReadExt as _, AsyncWrite, AsyncWriteExt as _, FutureExt as _, Sink, StreamExt as _,
    channel::{mpsc, oneshot},
    future::{BoxFuture, LocalBoxFuture},
    io::BufReader,
    stream::BoxStream,
};
use gpui::{AsyncApp, Entity};
use project::{Project, agent_server_store::AgentServerCommand};
use remote::remote_client::Interactive;
use std::{
    io,
    pin::Pin,
    process::{ExitStatus, Stdio},
};
use task::{Shell, ShellBuilder};
use util::{ResultExt as _, process::Child};

pub(super) struct StdioProcess {
    /// `None` when the agent runs in an agent host, which outlives this
    /// connection instead of being killed with it.
    pub child: Option<Child>,
    pub status: LocalBoxFuture<'static, io::Result<ExitStatus>>,
    pub incoming: BoxStream<'static, io::Result<String>>,
    pub outgoing: Pin<Box<dyn Sink<String, Error = io::Error> + Send>>,
    // The connection owns when this starts and when it is cancelled.
    pub stderr: BoxFuture<'static, Result<()>>,
    pub debug_log: AcpDebugLog,
}

/// Starts the agent server, or with `hosted` connects to the agent host that
/// runs it, so that its sessions carry on after Zed quits.
pub(super) async fn spawn_stdio(
    project: &Entity<Project>,
    command: AgentServerCommand,
    hosted: bool,
    cx: &AsyncApp,
) -> Result<StdioProcess> {
    let root_dir = project.read_with(cx, |project, cx| {
        project
            .default_path_list(cx)
            .ordered_paths()
            .next()
            .cloned()
    });
    let (path, arguments, environment) = project
        .read_with(cx, |project, cx| {
            project.remote_client().and_then(|client| {
                let template = client
                    .read(cx)
                    .build_command(
                        Some(command.path.display().to_string()),
                        &command.args,
                        &command.env.clone().into_iter().flatten().collect(),
                        root_dir.as_ref().map(|path| path.display().to_string()),
                        None,
                        Interactive::No,
                    )
                    .log_err()?;
                Some((template.program, template.args, template.env))
            })
        })
        .unwrap_or_else(|| {
            (
                command.path.display().to_string(),
                command.args,
                command.env.unwrap_or_default(),
            )
        });

    let builder = ShellBuilder::new(&Shell::System, cfg!(windows)).non_interactive();
    let mut child = builder.build_std_command(Some(path.clone()), &arguments);
    child.envs(environment);
    let is_local = project.read_with(cx, |project, _cx| project.is_local());
    if is_local && let Some(cwd) = root_dir.as_ref() {
        child.current_dir(cwd);
    }

    if hosted && is_local && agent_host::is_enabled() {
        let hosted_command = agent_host::HostedCommand::from_std(&child);
        match agent_host::connect(&hosted_command).await {
            Ok(Some(connection)) => {
                log::debug!("Connected to the agent host of {:?}, {:?}", path, arguments);
                return Ok(hosted_process(connection));
            }
            Ok(None) => {
                log::info!(
                    "The agent host is serving another connection, spawning the agent directly"
                );
            }
            Err(error) => {
                log::error!("Failed to use an agent host, spawning the agent directly: {error:#}");
            }
        }
    }

    let mut child = Child::spawn(child, Stdio::piped(), Stdio::piped(), Stdio::piped())?;

    let stdout = child.stdout.take().context("Failed to take stdout")?;
    let stdin = child.stdin.take().context("Failed to take stdin")?;
    let stderr = child.stderr.take().context("Failed to take stderr")?;
    log::debug!(
        "Spawning external agent server: {:?}, {:?}",
        path,
        arguments
    );
    log::trace!("Spawned (pid: {})", child.id());

    let debug_log = AcpDebugLog::default();
    let incoming = BufReader::new(stdout)
        .lines()
        .inspect(record_incoming_line(debug_log.clone()))
        .boxed();

    let outgoing = outgoing_sink(stdin, debug_log.clone());

    let stderr = {
        let debug_log = debug_log.clone();
        async move {
            let mut stderr = BufReader::new(stderr);
            let mut line = String::new();
            while let Ok(bytes_read) = stderr.read_line(&mut line).await
                && bytes_read > 0
            {
                record_stderr_line(&debug_log, line.trim_end_matches(['\n', '\r']));
                line.clear();
            }
            Ok(())
        }
        .boxed()
    };

    Ok(StdioProcess {
        status: child.status().boxed_local(),
        child: Some(child),
        incoming,
        outgoing,
        stderr,
        debug_log,
    })
}

fn hosted_process(connection: agent_host::HostConnection) -> StdioProcess {
    let debug_log = AcpDebugLog::default();
    let (stdout_sender, stdout_receiver) = mpsc::unbounded::<io::Result<String>>();
    let (exit_sender, exit_receiver) = oneshot::channel::<Option<i32>>();

    let incoming = stdout_receiver
        .inspect(record_incoming_line(debug_log.clone()))
        .boxed();

    let outgoing = outgoing_sink(connection.writer, debug_log.clone());

    // The host sends the agent's stdout, stderr and exit over one socket, so
    // the task the connection runs for stderr routes all three.
    let stderr = {
        let debug_log = debug_log.clone();
        let mut messages = connection.messages;
        async move {
            let mut exit_sender = Some(exit_sender);
            while let Some(message) = messages.next().await {
                match message {
                    Ok(agent_host::HostMessage::Stdout(line)) => {
                        stdout_sender.unbounded_send(Ok(line)).ok();
                    }
                    Ok(agent_host::HostMessage::Stderr(line)) => {
                        record_stderr_line(&debug_log, &line);
                    }
                    Ok(agent_host::HostMessage::Exited(code)) => {
                        if let Some(exit_sender) = exit_sender.take() {
                            exit_sender.send(code).ok();
                        }
                    }
                    Err(error) => {
                        stdout_sender.unbounded_send(Err(error)).ok();
                        break;
                    }
                }
            }
            Ok(())
        }
        .boxed()
    };

    // A host that goes away without reporting an exit counts as a failed one.
    let status = async move { Ok(exit_status(exit_receiver.await.ok().flatten())) }.boxed_local();

    StdioProcess {
        child: None,
        status,
        incoming,
        outgoing,
        stderr,
        debug_log,
    }
}

fn record_incoming_line(debug_log: AcpDebugLog) -> impl FnMut(&io::Result<String>) {
    move |result| match result {
        Ok(line) => debug_log.record_line(AcpDebugMessageDirection::Incoming, line),
        Err(error) => {
            log::warn!("ACP transport read error: {error}");
        }
    }
}

fn outgoing_sink(
    writer: impl AsyncWrite + Send + 'static,
    debug_log: AcpDebugLog,
) -> Pin<Box<dyn Sink<String, Error = io::Error> + Send>> {
    Box::pin(futures::sink::unfold(
        (Box::pin(writer), debug_log),
        async move |(mut writer, debug_log), line: String| {
            debug_log.record_line(AcpDebugMessageDirection::Outgoing, &line);
            let mut bytes = line.into_bytes();
            bytes.push(b'\n');
            writer.write_all(&bytes).await?;
            Ok::<_, io::Error>((writer, debug_log))
        },
    ))
}

fn record_stderr_line(debug_log: &AcpDebugLog, line: &str) {
    log::warn!("agent stderr: {line}");
    debug_log.record_line(AcpDebugMessageDirection::Stderr, line);
}

fn exit_status(code: Option<i32>) -> ExitStatus {
    let code = code.unwrap_or(1);
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt as _;
        ExitStatus::from_raw(code as u32)
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::process::ExitStatusExt as _;
        ExitStatus::from_raw(code << 8)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::acp::AcpDebugMessageContent;
    use collections::HashMap;
    use futures::SinkExt as _;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn repeated_spawns_preserve_command_and_stdio(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().expect("create working directory");
        let expected_directory = directory
            .path()
            .canonicalize()
            .expect("resolve working directory");
        let project = Project::example([directory.path()], &mut cx.to_async()).await;
        let command = AgentServerCommand {
            path: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                r#"printf '%s\n' "$ACP_TRANSPORT_TEST"; pwd -P; printf 'transport stderr\r\n' >&2; IFS= read -r line && printf '%s\n' "$line""#.into(),
            ],
            env: Some(HashMap::from_iter([(
                "ACP_TRANSPORT_TEST".into(),
                "value with spaces and $literal".into(),
            )])),
        };

        for attempt in 0..2 {
            let StdioProcess {
                child: _child,
                status,
                mut incoming,
                mut outgoing,
                stderr,
                debug_log,
            } = spawn_stdio(&project, command.clone(), false, &cx.to_async())
                .await
                .expect("spawn fresh transport");
            let stderr_task = cx.background_executor.spawn(stderr);
            let params = serde_json::json!({ "attempt": attempt });
            let request = serde_json::json!({
                "jsonrpc": "2.0",
                "id": attempt,
                "method": "transport/test",
                "params": params,
            })
            .to_string();
            let exchange = async {
                outgoing.send(request.clone()).await.expect("write request");
                drop(outgoing);

                let mut output = Vec::new();
                while let Some(line) = incoming.next().await {
                    output.push(line.expect("read output"));
                }
                assert_eq!(
                    output,
                    [
                        "value with spaces and $literal".to_string(),
                        expected_directory.to_string_lossy().into_owned(),
                        request,
                    ]
                );
                assert!(status.await.expect("wait for process").success());
                stderr_task.await.expect("read stderr");
            }
            .fuse();
            let timeout = cx
                .background_executor
                .timer(std::time::Duration::from_secs(5))
                .fuse();
            futures::pin_mut!(exchange, timeout);
            futures::select! {
                _ = exchange => {}
                _ = timeout => panic!("timed out waiting for stdio exchange on attempt {attempt}"),
            }

            let (messages, _receiver) = debug_log.subscribe();
            assert_eq!(messages.len(), 3, "each process must have its own log");
            for direction in [
                AcpDebugMessageDirection::Outgoing,
                AcpDebugMessageDirection::Incoming,
            ] {
                assert!(messages.iter().any(|message| {
                    message.direction == direction
                        && matches!(
                            &message.message,
                            AcpDebugMessageContent::Request { method, params: Some(recorded), .. }
                                if method.as_ref() == "transport/test" && recorded == &params
                        )
                }));
            }
            assert!(messages.iter().any(|message| {
                message.direction == AcpDebugMessageDirection::Stderr
                    && matches!(
                        &message.message,
                        AcpDebugMessageContent::Stderr { line } if line.as_ref() == "transport stderr"
                    )
            }));
        }
    }
}
