use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::{Context as _, Result};
use client::Client;
use gpui::{App, AppContext as _, AsyncApp};
use http_client::{HttpClient, Method};
use serde::Deserialize;
use util::ResultExt as _;

use crate::{
    ServerError, SyncClaudeSessions, notify, request_bytes, request_json, zed_authorization,
};

const POLL_INTERVAL: Duration = Duration::from_secs(180);
/// A log written to this recently belongs to a session Claude Code may still be appending to;
/// it is left alone until it goes quiet, so a half-written turn is never sent or overwritten.
const QUIET_PERIOD: Duration = Duration::from_secs(60);

#[derive(Deserialize)]
struct RemoteSessions {
    sessions: Vec<RemoteSession>,
}

#[derive(Deserialize)]
struct RemoteSession {
    project: String,
    session_id: String,
    size: u64,
}

struct LocalSession {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

#[derive(Default)]
struct SyncSummary {
    uploaded: usize,
    downloaded: usize,
    diverged: usize,
}

pub(crate) fn init(client: Arc<Client>, cx: &mut App) {
    cx.on_action({
        let client = client.clone();
        move |_: &SyncClaudeSessions, cx| {
            let client = client.clone();
            cx.spawn(async move |cx| {
                let message = match sync_once(&client, cx).await {
                    Ok(None) => {
                        "Sign in to Zed to sync Claude Code sessions with the fork server."
                            .to_string()
                    }
                    Ok(Some(summary)) => format!(
                        "Claude Code sessions synced: {} sent, {} received, {} diverged.",
                        summary.uploaded, summary.downloaded, summary.diverged
                    ),
                    Err(error) => format!("Claude Code session sync failed: {error:#}"),
                };
                cx.update(|cx| notify(message, cx));
            })
            .detach();
        }
    });

    cx.spawn(async move |cx| {
        let mut last_error = None;
        loop {
            match sync_once(&client, cx).await {
                Ok(summary) => {
                    last_error = None;
                    if let Some(summary) = summary
                        && summary.uploaded + summary.downloaded + summary.diverged > 0
                    {
                        log::info!(
                            "claude session sync: {} sent, {} received, {} diverged",
                            summary.uploaded,
                            summary.downloaded,
                            summary.diverged
                        );
                    }
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    if last_error.as_ref() != Some(&message) {
                        log::warn!("claude session sync failed: {message}");
                        last_error = Some(message);
                    }
                }
            }
            cx.background_executor().timer(POLL_INTERVAL).await;
        }
    })
    .detach();
}

async fn sync_once(client: &Arc<Client>, cx: &mut AsyncApp) -> Result<Option<SyncSummary>> {
    let Some(authorization) = zed_authorization(client, cx).await? else {
        return Ok(None);
    };
    let http: Arc<dyn HttpClient> = client.http_client();
    let summary = cx
        .background_spawn(async move { reconcile(&http, &authorization).await })
        .await?;
    Ok(Some(summary))
}

/// Session logs are append-only, so of two copies the longer one is the newer, and the
/// shorter is normally a prefix of it. When it is not, both machines added turns to the same
/// session; the longer copy wins and the other is kept in the conflicts folder.
async fn reconcile(http: &Arc<dyn HttpClient>, authorization: &str) -> Result<SyncSummary> {
    let projects_dir = projects_dir();
    let remote: RemoteSessions = request_json(
        http,
        Method::GET,
        "/v1/claude/sessions",
        Some(authorization),
        None::<&()>,
    )
    .await
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    let local = read_local_sessions(&projects_dir).await?;
    let mut summary = SyncSummary::default();
    let now = SystemTime::now();
    let is_quiet = |session: &LocalSession| {
        now.duration_since(session.modified)
            .is_ok_and(|elapsed| elapsed >= QUIET_PERIOD)
    };

    let remote_sizes: BTreeMap<(String, String), u64> = remote
        .sessions
        .into_iter()
        .map(|session| ((session.project, session.session_id), session.size))
        .collect();

    for (key, session) in &local {
        let remote_size = remote_sizes.get(key).copied().unwrap_or(0);
        if session.size <= remote_size || !is_quiet(session) {
            continue;
        }
        let (project, session_id) = key;
        let content = smol::fs::read(&session.path)
            .await
            .with_context(|| format!("reading {}", session.path.display()))?;
        match request_bytes(
            http,
            Method::PUT,
            &session_url(project, session_id),
            Some(authorization),
            Some(("application/x-ndjson", content)),
        )
        .await
        {
            Ok(_) => summary.uploaded += 1,
            // Another machine sent a longer copy in between; the next round downloads it.
            Err(ServerError::Conflict) => {}
            Err(error) => return Err(anyhow::anyhow!("{error}")),
        }
    }

    for (key, remote_size) in &remote_sizes {
        // The server checks these too; they become local paths, so check again here.
        if !is_valid_key(&key.0, &key.1) {
            continue;
        }
        let local_session = local.get(key);
        let local_size = local_session.map(|session| session.size).unwrap_or(0);
        if *remote_size <= local_size || local_session.is_some_and(|session| !is_quiet(session))
        {
            continue;
        }
        let (project, session_id) = key;
        let content = request_bytes(
            http,
            Method::GET,
            &session_url(project, session_id),
            Some(authorization),
            None,
        )
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;

        let target = local_session
            .map(|session| session.path.clone())
            .unwrap_or_else(|| {
                local_project_dir(&projects_dir, project).join(format!("{session_id}.jsonl"))
            });
        if let Some(local_session) = local_session {
            let local_content = smol::fs::read(&local_session.path).await?;
            if !content.starts_with(&local_content) {
                back_up_diverged(project, session_id, &local_content).await?;
                summary.diverged += 1;
            }
        }
        write_atomically(&target, &content).await?;
        summary.downloaded += 1;
    }

    Ok(summary)
}

fn session_url(project: &str, session_id: &str) -> String {
    format!("/v1/claude/sessions/{project}/{session_id}")
}

fn projects_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| util::paths::home_dir().join(".claude"))
        .join("projects")
}

/// Claude Code names a project's folder after its path with every character that is not an
/// ASCII letter or digit turned into `-`, so `C:\Users\Yusuf\zed` is `C--Users-Yusuf-zed`.
fn encode_path(path: &str) -> String {
    path.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

fn encoded_home() -> String {
    encode_path(&util::paths::home_dir().to_string_lossy())
}

/// The folder name with this machine's home directory swapped for `~`, so a project under the
/// home directory matches on a machine with another user name. The drive letter's case varies
/// between folders (`C--Users-…` and `c--Users-…` both occur), hence the case-insensitive match.
fn project_key(folder_name: &str, encoded_home: &str) -> String {
    let home_length = encoded_home.len();
    let matches_home = folder_name.len() >= home_length
        && folder_name.is_char_boundary(home_length)
        && folder_name[..home_length].eq_ignore_ascii_case(encoded_home)
        && (folder_name.len() == home_length || folder_name[home_length..].starts_with('-'));
    if matches_home {
        format!("~{}", &folder_name[home_length..])
    } else {
        folder_name.to_string()
    }
}

fn local_project_dir(projects_dir: &std::path::Path, project: &str) -> PathBuf {
    match project.strip_prefix('~') {
        Some(rest) => projects_dir.join(format!("{}{rest}", encoded_home())),
        None => projects_dir.join(project),
    }
}

fn is_valid_key(project: &str, session_id: &str) -> bool {
    let valid_project = !project.is_empty()
        && project.len() <= 200
        && project != "."
        && project != ".."
        && project
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "._~-".contains(character));
    let valid_session = !session_id.is_empty()
        && session_id.len() <= 100
        && session_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-');
    valid_project && valid_session
}

/// Only the top-level `<session id>.jsonl` of each project folder: the `<session id>/` folders
/// beside them (subagent logs, saved tool output) and `memory/` stay on their machine.
async fn read_local_sessions(
    projects_dir: &std::path::Path,
) -> Result<BTreeMap<(String, String), LocalSession>> {
    let mut sessions = BTreeMap::new();
    let mut projects = match smol::fs::read_dir(projects_dir).await {
        Ok(projects) => projects,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(sessions),
        Err(error) => {
            return Err(error).with_context(|| format!("listing {}", projects_dir.display()));
        }
    };
    let encoded_home = encoded_home();
    while let Some(project) = futures::StreamExt::next(&mut projects).await {
        let project = project?;
        if !project.file_type().await?.is_dir() {
            continue;
        }
        let key = project_key(&project.file_name().to_string_lossy(), &encoded_home);
        let mut entries = smol::fs::read_dir(project.path()).await?;
        while let Some(entry) = futures::StreamExt::next(&mut entries).await {
            let entry = entry?;
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let Some(session_id) = file_name.strip_suffix(".jsonl") else {
                continue;
            };
            if !is_valid_key(&key, session_id) {
                continue;
            }
            let metadata = entry.metadata().await?;
            if !metadata.is_file() {
                continue;
            }
            sessions.insert(
                (key.clone(), session_id.to_string()),
                LocalSession {
                    path: entry.path(),
                    size: metadata.len(),
                    modified: metadata.modified()?,
                },
            );
        }
    }
    Ok(sessions)
}

async fn write_atomically(target: &std::path::Path, content: &[u8]) -> Result<()> {
    let parent = target
        .parent()
        .context("a session path has no parent directory")?;
    smol::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("creating {}", parent.display()))?;
    let mut temporary = target.as_os_str().to_os_string();
    temporary.push(".fork-sync-download");
    let temporary = PathBuf::from(temporary);
    smol::fs::write(&temporary, content)
        .await
        .with_context(|| format!("writing {}", temporary.display()))?;
    if let Err(error) = smol::fs::rename(&temporary, target).await {
        smol::fs::remove_file(&temporary).await.log_err();
        return Err(error).with_context(|| format!("replacing {}", target.display()));
    }
    Ok(())
}

async fn back_up_diverged(project: &str, session_id: &str, content: &[u8]) -> Result<()> {
    let directory = paths::data_dir()
        .join("fork_sync")
        .join("conflicts")
        .join("claude");
    smol::fs::create_dir_all(&directory).await?;
    let timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let backup_path = directory.join(format!("{timestamp}-{project}-{session_id}.jsonl"));
    smol::fs::write(&backup_path, content)
        .await
        .with_context(|| format!("writing {}", backup_path.display()))?;
    log::warn!(
        "claude session sync: {project}/{session_id} grew differently on another machine; kept this machine's copy at {}",
        backup_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{encode_path, is_valid_key, project_key};

    #[test]
    fn project_keys_drop_the_home_directory() {
        let home = encode_path(r"C:\Users\JosephShall");
        assert_eq!(home, "C--Users-JosephShall");
        assert_eq!(project_key("C--Users-JosephShall-zed", &home), "~-zed");
        assert_eq!(project_key("c--Users-JosephShall-incapon", &home), "~-incapon");
        assert_eq!(project_key("C--Users-JosephShall", &home), "~");
        assert_eq!(project_key("C--Users-JosephShallX", &home), "C--Users-JosephShallX");
        assert_eq!(project_key("D--work-api", &home), "D--work-api");
    }

    #[test]
    fn keys_cannot_escape_the_projects_folder() {
        assert!(is_valid_key("~-zed", "0db85bbc-ce97-4024-8740-51db914d6a98"));
        assert!(!is_valid_key("..", "x"));
        assert!(!is_valid_key("~-zed", "../x"));
        assert!(!is_valid_key("a/b", "x"));
    }
}
