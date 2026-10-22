use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context as _, Result};
use client::Client;
use gpui::{App, AppContext as _, AsyncApp, Global};
use http_client::{HttpClient, Method};
use release_channel::{AppCommitSha, ReleaseChannel};
use serde::Deserialize;
use util::ResultExt as _;
use workspace::notifications::{
    NotificationId, show_app_notification, simple_message_notification::MessageNotification,
};

use crate::{CheckForUpdate, ServerError, notify, request_json};

const POLL_INTERVAL: Duration = Duration::from_secs(60 * 60);
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(30);
const STAGING_DIR_NAME: &str = "fork-update";

#[derive(Deserialize)]
struct LatestRelease {
    sha: String,
    name: String,
    url: String,
}

/// The commit of the build already swapped in next to the running exe, so the hourly poll
/// does not download it again while the user has not restarted yet.
#[derive(Default)]
struct StagedUpdate(Option<String>);

impl Global for StagedUpdate {}

pub(crate) fn init(client: Arc<Client>, cx: &mut App) {
    cx.set_global(StagedUpdate::default());

    cx.on_action({
        let client = client.clone();
        move |_: &CheckForUpdate, cx| {
            let client = client.clone();
            cx.spawn(async move |cx| {
                let message = match check_and_install(&client, cx).await {
                    Ok(Outcome::UpToDate) => "Zed fork is up to date.".to_string(),
                    Ok(Outcome::AlreadyStaged) => {
                        "The newest fork build is already installed; restart Zed to use it."
                            .to_string()
                    }
                    Ok(Outcome::Unsupported(reason)) => reason.to_string(),
                    Ok(Outcome::Installed) => return,
                    Err(error) => format!("Fork update failed: {error:#}"),
                };
                cx.update(|cx| notify(message, cx));
            })
            .detach();
        }
    });

    if unsupported_reason(cx).is_some() {
        return;
    }
    if let Some(exe_path) = std::env::current_exe().log_err() {
        cx.background_spawn(async move { remove_leftovers(&exe_path).await })
            .detach();
    }

    cx.spawn(async move |cx| {
        cx.background_executor().timer(FIRST_CHECK_DELAY).await;
        loop {
            if let Err(error) = check_and_install(&client, cx).await {
                log::warn!("fork update check failed: {error:#}");
            }
            cx.background_executor().timer(POLL_INTERVAL).await;
        }
    })
    .detach();
}

enum Outcome {
    UpToDate,
    AlreadyStaged,
    Unsupported(&'static str),
    Installed,
}

fn unsupported_reason(cx: &App) -> Option<&'static str> {
    if !cfg!(target_os = "windows") {
        return Some("Fork updates are only built for Windows.");
    }
    if ReleaseChannel::global(cx) == ReleaseChannel::Dev {
        return Some("Fork updates are off in dev builds.");
    }
    if AppCommitSha::try_global(cx).is_none() {
        return Some("This build has no commit SHA to compare against.");
    }
    let running_from_target = std::env::current_exe()
        .map(|path| path.components().any(|component| component.as_os_str() == "target"))
        .unwrap_or(true);
    if running_from_target {
        // Swapping cargo's own output would only confuse the next `cargo build`.
        return Some("Fork updates are off when Zed runs from a cargo target directory.");
    }
    None
}

async fn check_and_install(client: &Arc<Client>, cx: &mut AsyncApp) -> Result<Outcome> {
    if let Some(reason) = cx.update(|cx| unsupported_reason(cx)) {
        return Ok(Outcome::Unsupported(reason));
    }
    let Some(current_sha) = cx.update(|cx| AppCommitSha::try_global(cx).map(|sha| sha.full()))
    else {
        return Ok(Outcome::Unsupported("This build has no commit SHA to compare against."));
    };

    let http: Arc<dyn HttpClient> = client.http_client();
    let path = format!(
        "/v1/releases/latest?os={}&arch={}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    let release: LatestRelease = request_json(&http, Method::GET, &path, None, None::<&()>)
        .await
        .map_err(|error| match error {
            ServerError::Other(error) => error,
            error => anyhow::anyhow!("{error}"),
        })?;

    if release.sha == current_sha {
        return Ok(Outcome::UpToDate);
    }
    let already_staged = cx.update(|cx| {
        cx.global::<StagedUpdate>().0.as_deref() == Some(release.sha.as_str())
    });
    if already_staged {
        return Ok(Outcome::AlreadyStaged);
    }

    let exe_path = std::env::current_exe().context("locating the running Zed.exe")?;
    log::info!(
        "installing fork build {} ({}) over {}",
        release.name,
        release.sha,
        exe_path.display()
    );
    cx.background_spawn({
        let http = http.clone();
        let exe_path = exe_path.clone();
        let url = release.url.clone();
        async move { download_and_swap(&http, &url, &exe_path).await }
    })
    .await?;

    let short_sha: String = release.sha.chars().take(7).collect();
    let message = format!(
        "Zed fork updated to {} ({short_sha}). Restart to use it.",
        release.name
    );
    cx.update(|cx| {
        cx.global_mut::<StagedUpdate>().0 = Some(release.sha.clone());
        show_restart_notification(message, exe_path, cx);
    });
    Ok(Outcome::Installed)
}

/// Windows refuses to overwrite or delete a running exe but does allow renaming it, so the
/// running one steps aside as `Zed.exe.old` and the new build takes its name. The process keeps
/// running from the renamed file; the next launch, from any shortcut or the CLI, gets the new one.
async fn download_and_swap(http: &Arc<dyn HttpClient>, url: &str, exe_path: &Path) -> Result<()> {
    let install_dir = exe_path
        .parent()
        .context("the running exe has no parent directory")?;
    let staging_dir = install_dir.join(STAGING_DIR_NAME);
    remove_dir_if_present(&staging_dir).await?;
    smol::fs::create_dir_all(&staging_dir)
        .await
        .with_context(|| format!("creating {}", staging_dir.display()))?;

    let response = http
        .get(url, Default::default(), true)
        .await
        .with_context(|| format!("downloading {url}"))?;
    anyhow::ensure!(
        response.status().is_success(),
        "downloading {url} answered {}",
        response.status()
    );
    util::archive::extract_zip(&staging_dir, response.into_body())
        .await
        .context("unpacking the fork build")?;

    let new_exe = find_exe(&staging_dir)
        .await?
        .context("the downloaded archive has no zed.exe")?;
    let old_exe = old_exe_path(exe_path);
    remove_file_if_present(&old_exe).await?;
    smol::fs::rename(exe_path, &old_exe)
        .await
        .with_context(|| format!("moving {} aside", exe_path.display()))?;
    if let Err(error) = smol::fs::rename(&new_exe, exe_path).await {
        smol::fs::rename(&old_exe, exe_path).await.log_err();
        return Err(error).with_context(|| format!("putting the new build at {}", exe_path.display()));
    }
    remove_dir_if_present(&staging_dir).await.log_err();
    Ok(())
}

async fn find_exe(directory: &Path) -> Result<Option<PathBuf>> {
    let mut entries = smol::fs::read_dir(directory).await?;
    while let Some(entry) = futures::StreamExt::next(&mut entries).await {
        let entry = entry?;
        if entry.file_name().to_string_lossy().eq_ignore_ascii_case("zed.exe") {
            return Ok(Some(entry.path()));
        }
    }
    Ok(None)
}

fn old_exe_path(exe_path: &Path) -> PathBuf {
    let mut file_name = exe_path.file_name().unwrap_or_default().to_os_string();
    file_name.push(".old");
    exe_path.with_file_name(file_name)
}

async fn remove_leftovers(exe_path: &Path) {
    // Fails harmlessly while an older window from before the swap is still running.
    remove_file_if_present(&old_exe_path(exe_path)).await.log_err();
    if let Some(install_dir) = exe_path.parent() {
        remove_dir_if_present(&install_dir.join(STAGING_DIR_NAME))
            .await
            .log_err();
    }
}

async fn remove_file_if_present(path: &Path) -> Result<()> {
    match smol::fs::remove_file(path).await {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

async fn remove_dir_if_present(path: &Path) -> Result<()> {
    match smol::fs::remove_dir_all(path).await {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

struct RestartNotification;

fn show_restart_notification(message: String, exe_path: PathBuf, cx: &mut App) {
    show_app_notification(
        NotificationId::unique::<RestartNotification>(),
        cx,
        move |cx| {
            let message = message.clone();
            let exe_path = exe_path.clone();
            cx.new(|cx| {
                MessageNotification::new(message, cx)
                    .primary_message("Restart")
                    .primary_on_click(move |_window, cx| {
                        // The running process now lives at Zed.exe.old; restart the new build.
                        cx.set_restart_path(exe_path.clone());
                        cx.restart();
                    })
                    .show_suppress_button(false)
            })
        },
    );
}
