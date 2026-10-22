use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use client::Client;
use gpui::{App, AppContext as _, AsyncApp};
use http_client::{HttpClient, Method};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use util::ResultExt as _;

use crate::{ServerError, SyncSettings, notify, request_json, zed_authorization};

/// Local files are hashed this often; it is a handful of small files, and polling sees edits
/// made through the symlinks into `tools\Backups\zed`, which a directory watch would not.
const LOCAL_POLL_INTERVAL: Duration = Duration::from_secs(15);
/// The server is asked for other machines' changes at least this often.
const REMOTE_POLL_INTERVAL: Duration = Duration::from_secs(120);

const SYNCED_FILES: &[&str] = &["settings.json", "keymap.json", "tasks.json"];
const SYNCED_DIRECTORIES: &[&str] = &["snippets", "themes"];

#[derive(Deserialize)]
struct RemoteFiles {
    files: Vec<RemoteFile>,
}

#[derive(Deserialize)]
struct RemoteFile {
    path: String,
    revision: i64,
    content: Option<String>,
}

#[derive(Serialize)]
struct PutFile<'a> {
    path: &'a str,
    base_revision: i64,
    content: Option<&'a str>,
}

#[derive(Deserialize)]
struct PutFileResponse {
    revision: i64,
}

/// What this machine last agreed with the server on, per file. A file whose hash moved since is
/// a local edit; a file whose revision moved is another machine's.
#[derive(Default, Serialize, Deserialize)]
struct SyncState {
    files: BTreeMap<String, KnownFile>,
}

#[derive(Clone, Serialize, Deserialize)]
struct KnownFile {
    revision: i64,
    /// `None` once the file was deleted.
    sha256: Option<String>,
}

#[derive(Default)]
struct SyncSummary {
    pushed: usize,
    pulled: usize,
    conflicts: usize,
}

pub(crate) fn init(client: Arc<Client>, cx: &mut App) {
    cx.on_action({
        let client = client.clone();
        move |_: &SyncSettings, cx| {
            let client = client.clone();
            cx.spawn(async move |cx| {
                let message = match sync_once(&client, cx).await {
                    Ok(None) => "Sign in to Zed to sync settings with the fork server.".to_string(),
                    Ok(Some(summary)) => format!(
                        "Settings synced: {} sent, {} received, {} conflicts.",
                        summary.pushed, summary.pulled, summary.conflicts
                    ),
                    Err(error) => format!("Settings sync failed: {error:#}"),
                };
                cx.update(|cx| notify(message, cx));
            })
            .detach();
        }
    });

    cx.spawn(async move |cx| {
        let mut last_local_hashes = None;
        let mut last_remote_sync: Option<Instant> = None;
        let mut last_error = None;
        loop {
            let local_hashes = cx
                .background_spawn(async { hash_local_files(&read_local_files().await?) })
                .await
                .log_err();
            let local_changed = local_hashes.is_some() && local_hashes != last_local_hashes;
            let remote_due = last_remote_sync
                .is_none_or(|last_sync| last_sync.elapsed() >= REMOTE_POLL_INTERVAL);

            if local_changed || remote_due {
                last_remote_sync = Some(Instant::now());
                match sync_once(&client, cx).await {
                    Ok(summary) => {
                        last_error = None;
                        if let Some(summary) = summary
                            && summary.pushed + summary.pulled + summary.conflicts > 0
                        {
                            log::info!(
                                "settings sync: {} sent, {} received, {} conflicts",
                                summary.pushed,
                                summary.pulled,
                                summary.conflicts
                            );
                        }
                    }
                    Err(error) => {
                        let message = format!("{error:#}");
                        // Logged once per distinct failure, not every 15 seconds while offline.
                        if last_error.as_ref() != Some(&message) {
                            log::warn!("settings sync failed: {message}");
                            last_error = Some(message);
                        }
                    }
                }
                // Re-hashed so the files this sync just wrote do not count as local edits.
                last_local_hashes = cx
                    .background_spawn(async { hash_local_files(&read_local_files().await?) })
                    .await
                    .log_err();
            }

            cx.background_executor().timer(LOCAL_POLL_INTERVAL).await;
        }
    })
    .detach();
}

/// Returns `None` when Zed is not signed in, since the server identifies the user from that.
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

async fn reconcile(http: &Arc<dyn HttpClient>, authorization: &str) -> Result<SyncSummary> {
    let remote: RemoteFiles = request_json(
        http,
        Method::GET,
        "/v1/settings",
        Some(authorization),
        None::<&()>,
    )
    .await
    .map_err(|error| anyhow::anyhow!("{error}"))?;
    let remote: BTreeMap<String, RemoteFile> = remote
        .files
        .into_iter()
        .filter(|file| is_synced_path(&file.path))
        .map(|file| (file.path.clone(), file))
        .collect();

    let local = read_local_files().await?;
    let mut state = read_state().await;
    let mut summary = SyncSummary::default();

    let paths: BTreeSet<String> = local
        .keys()
        .chain(remote.keys())
        .chain(state.files.keys())
        .cloned()
        .collect();

    for path in paths {
        let local_content = local.get(&path);
        let local_hash = local_content.map(|content| sha256_hex(content.as_bytes()));
        let known = state.files.get(&path).cloned();
        let remote_file = remote.get(&path);
        let remote_hash = remote_file
            .and_then(|file| file.content.as_ref())
            .map(|content| sha256_hex(content.as_bytes()));

        let known_hash = known.as_ref().and_then(|known| known.sha256.clone());
        let known_revision = known.as_ref().map(|known| known.revision);
        let local_changed = local_hash != known_hash;
        let remote_changed = remote_file.map(|file| file.revision) != known_revision;

        let action = match (local_changed, remote_changed) {
            (false, false) => continue,
            (true, false) => Action::Push,
            (false, true) => Action::Pull,
            (true, true) if local_hash == remote_hash => Action::Agree,
            // Both sides moved: a deletion over there loses to an edit here, otherwise the
            // server wins and this machine's version is kept in the conflicts folder.
            (true, true) if remote_hash.is_none() && local_hash.is_some() => Action::Push,
            (true, true) => Action::PullOverConflict,
        };

        let remote_revision = remote_file.map(|file| file.revision).unwrap_or(0);
        match action {
            Action::Push => {
                let base_revision = remote_revision;
                let request = PutFile {
                    path: &path,
                    base_revision,
                    content: local_content.map(String::as_str),
                };
                match request_json::<PutFileResponse>(
                    http,
                    Method::PUT,
                    "/v1/settings/files",
                    Some(authorization),
                    Some(&request),
                )
                .await
                {
                    Ok(response) => {
                        state.files.insert(
                            path.clone(),
                            KnownFile {
                                revision: response.revision,
                                sha256: local_hash,
                            },
                        );
                        summary.pushed += 1;
                    }
                    // Another machine wrote in between; the next round sees its revision.
                    Err(ServerError::Conflict) => continue,
                    Err(error) => return Err(anyhow::anyhow!("{error}")),
                }
            }
            Action::Agree => {
                state.files.insert(
                    path.clone(),
                    KnownFile {
                        revision: remote_revision,
                        sha256: local_hash,
                    },
                );
            }
            Action::Pull | Action::PullOverConflict => {
                if action == Action::PullOverConflict
                    && let Some(local_content) = local_content
                {
                    back_up_conflict(&path, local_content).await?;
                    summary.conflicts += 1;
                }
                let content = remote_file.and_then(|file| file.content.as_deref());
                write_local_file(&path, content).await?;
                state.files.insert(
                    path.clone(),
                    KnownFile {
                        revision: remote_revision,
                        sha256: remote_hash,
                    },
                );
                summary.pulled += 1;
            }
        }
        write_state(&state).await?;
    }

    Ok(summary)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Push,
    Pull,
    PullOverConflict,
    Agree,
}

/// The server validates paths the same way; anything else is never read or written.
fn is_synced_path(path: &str) -> bool {
    if SYNCED_FILES.contains(&path) {
        return true;
    }
    let Some((directory, file_name)) = path.split_once('/') else {
        return false;
    };
    SYNCED_DIRECTORIES.contains(&directory)
        && file_name.ends_with(".json")
        && !file_name.starts_with('.')
        && !file_name.contains(['/', '\\', ':'])
        && file_name.len() <= 200
}

fn local_path(path: &str) -> PathBuf {
    path.split('/')
        .fold(paths::config_dir().clone(), |full_path, component| {
            full_path.join(component)
        })
}

async fn read_local_files() -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    for path in SYNCED_FILES {
        if let Some(content) = read_text(&local_path(path)).await? {
            files.insert(path.to_string(), content);
        }
    }
    for directory in SYNCED_DIRECTORIES {
        let directory_path = paths::config_dir().join(directory);
        let mut entries = match smol::fs::read_dir(&directory_path).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("listing {}", directory_path.display()));
            }
        };
        while let Some(entry) = futures::StreamExt::next(&mut entries).await {
            let entry = entry?;
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let path = format!("{directory}/{file_name}");
            if !is_synced_path(&path) || !entry.file_type().await?.is_file() {
                continue;
            }
            if let Some(content) = read_text(&entry.path()).await? {
                files.insert(path, content);
            }
        }
    }
    Ok(files)
}

async fn read_text(path: &Path) -> Result<Option<String>> {
    match smol::fs::read(path).await {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => Ok(Some(text)),
            Err(_) => {
                log::warn!("settings sync skips {}: not UTF-8", path.display());
                Ok(None)
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes through `smol::fs::write`, not an atomic rename, so that a config file which is a
/// symlink (settings.json points into `tools\Backups\zed`) stays a symlink.
async fn write_local_file(path: &str, content: Option<&str>) -> Result<()> {
    let full_path = local_path(path);
    match content {
        Some(content) => {
            if let Some(parent) = full_path.parent() {
                smol::fs::create_dir_all(parent)
                    .await
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            smol::fs::write(&full_path, content)
                .await
                .with_context(|| format!("writing {}", full_path.display()))
        }
        None => match smol::fs::remove_file(&full_path).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(error).with_context(|| format!("removing {}", full_path.display()))
            }
            _ => Ok(()),
        },
    }
}

async fn back_up_conflict(path: &str, content: &str) -> Result<()> {
    let directory = sync_data_dir().join("conflicts");
    smol::fs::create_dir_all(&directory).await?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let backup_path = directory.join(format!("{timestamp}-{}", path.replace('/', "-")));
    smol::fs::write(&backup_path, content)
        .await
        .with_context(|| format!("writing {}", backup_path.display()))?;
    log::warn!(
        "settings sync: {path} changed here and on another machine; kept this machine's copy at {}",
        backup_path.display()
    );
    Ok(())
}

fn sync_data_dir() -> PathBuf {
    paths::data_dir().join("fork_sync")
}

fn state_path() -> PathBuf {
    sync_data_dir().join("settings_state.json")
}

async fn read_state() -> SyncState {
    match smol::fs::read(state_path()).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .context("parsing the settings sync state")
            .log_err()
            .unwrap_or_default(),
        Err(_) => SyncState::default(),
    }
}

async fn write_state(state: &SyncState) -> Result<()> {
    smol::fs::create_dir_all(sync_data_dir()).await?;
    smol::fs::write(state_path(), serde_json::to_vec_pretty(state)?)
        .await
        .context("writing the settings sync state")
}

fn hash_local_files(files: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    Ok(files
        .iter()
        .map(|(path, content)| (path.clone(), sha256_hex(content.as_bytes())))
        .collect())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::is_synced_path;

    #[test]
    fn only_config_files_are_synced() {
        assert!(is_synced_path("settings.json"));
        assert!(is_synced_path("themes/martin.json"));
        assert!(is_synced_path("snippets/rust.json"));
        assert!(!is_synced_path("themes/../settings.json.json"));
        assert!(!is_synced_path("themes/sub/x.json"));
        assert!(!is_synced_path("extensions/x.json"));
        assert!(!is_synced_path("AGENTS.md"));
        assert!(!is_synced_path("themes/.hidden.json"));
    }
}
