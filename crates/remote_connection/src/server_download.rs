//! Fetches `zed-remote-server` releases from zed.dev for remote connections.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};

use anyhow::{Context as _, Result};
use client::Client;
use futures::{AsyncReadExt as _, StreamExt as _};
use gpui::AsyncApp;
use http_client::{HttpClient as _, HttpClientWithUrl};
use release_channel::ReleaseChannel;
use semver::Version;
use serde::{Deserialize, Serialize};

const REMOTE_SERVER_CACHE_LIMIT: usize = 5;

#[derive(Serialize)]
struct AssetQuery<'a> {
    asset: &'a str,
    os: &'a str,
    arch: &'a str,
    metrics_id: Option<&'a str>,
    system_id: Option<&'a str>,
    is_staff: Option<bool>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    version: String,
    url: String,
}

// If you are packaging Zed and need to override the place it downloads SSH remotes from,
// you can override this function. You should also update get_remote_server_release_url to return
// Ok(None).
pub(crate) async fn download_remote_server_release(
    release_channel: ReleaseChannel,
    version: Option<Version>,
    os: &str,
    arch: &str,
    set_status: impl Fn(&str, &mut AsyncApp) + Send + 'static,
    cx: &mut AsyncApp,
) -> Result<PathBuf> {
    let client = cx.update(|cx| Client::global(cx));

    set_status("Fetching remote server release", cx);
    let release = get_remote_server_release(&client, release_channel, version, os, arch).await?;

    let servers_dir = paths::remote_servers_dir();
    let channel_dir = servers_dir.join(release_channel.dev_name());
    let platform_dir = channel_dir.join(format!("{}-{}", os, arch));
    let version_path = platform_dir.join(format!("{}.gz", release.version));
    smol::fs::create_dir_all(&platform_dir).await.ok();

    if smol::fs::metadata(&version_path).await.is_err() {
        log::info!(
            "downloading zed-remote-server {os} {arch} version {}",
            release.version
        );
        set_status("Downloading remote server", cx);
        download_remote_server_binary(&version_path, release, client.http_client()).await?;
    }

    if let Err(error) =
        cleanup_remote_server_cache(&platform_dir, &version_path, REMOTE_SERVER_CACHE_LIMIT).await
    {
        log::warn!(
            "Failed to clean up remote server cache in {:?}: {error:#}",
            platform_dir
        );
    }

    Ok(version_path)
}

pub(crate) async fn get_remote_server_release_url(
    channel: ReleaseChannel,
    version: Option<Version>,
    os: &str,
    arch: &str,
    cx: &mut AsyncApp,
) -> Result<Option<String>> {
    let client = cx.update(|cx| Client::global(cx));
    let release = get_remote_server_release(&client, channel, version, os, arch).await?;
    Ok(Some(release.url))
}

async fn get_remote_server_release(
    client: &Client,
    release_channel: ReleaseChannel,
    version: Option<Version>,
    os: &str,
    arch: &str,
) -> Result<ReleaseAsset> {
    let (system_id, metrics_id, is_staff) = if client.telemetry().metrics_enabled() {
        (
            client.telemetry().system_id(),
            client.telemetry().metrics_id(),
            client.telemetry().is_staff(),
        )
    } else {
        (None, None, None)
    };

    let version = if let Some(mut version) = version {
        version.pre = semver::Prerelease::EMPTY;
        version.build = semver::BuildMetadata::EMPTY;
        version.to_string()
    } else {
        "latest".to_string()
    };
    let http_client = client.http_client();

    let path = format!("/releases/{}/{}/asset", release_channel.dev_name(), version);
    let url = http_client.build_zed_cloud_url_with_query(
        &path,
        AssetQuery {
            os,
            arch,
            asset: "zed-remote-server",
            metrics_id: metrics_id.as_deref(),
            system_id: system_id.as_deref(),
            is_staff,
        },
    )?;

    let mut response = http_client
        .get(url.as_str(), Default::default(), true)
        .await?;
    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;

    anyhow::ensure!(
        response.status().is_success(),
        "failed to fetch release: {:?}",
        String::from_utf8_lossy(&body),
    );

    serde_json::from_slice(body.as_slice()).with_context(|| {
        format!(
            "error deserializing release {:?}",
            String::from_utf8_lossy(&body),
        )
    })
}

async fn download_remote_server_binary(
    target_path: &PathBuf,
    release: ReleaseAsset,
    client: Arc<HttpClientWithUrl>,
) -> Result<()> {
    let temp = tempfile::Builder::new().tempfile_in(paths::remote_servers_dir())?;
    let mut temp_file = smol::fs::File::create(&temp).await?;

    let mut response = client.get(&release.url, Default::default(), true).await?;
    anyhow::ensure!(
        response.status().is_success(),
        "failed to download remote server release: {:?}",
        response.status()
    );
    smol::io::copy(response.body_mut(), &mut temp_file).await?;
    smol::fs::rename(&temp, &target_path).await?;

    Ok(())
}

async fn cleanup_remote_server_cache(
    platform_dir: &Path,
    keep_path: &Path,
    limit: usize,
) -> Result<()> {
    if limit == 0 {
        return Ok(());
    }

    let mut entries = smol::fs::read_dir(platform_dir).await?;
    let now = SystemTime::now();
    let mut candidates = Vec::new();

    while let Some(entry) = entries.next().await {
        let entry = entry?;
        let path = entry.path();
        if path.extension() != Some(OsStr::new("gz")) {
            continue;
        }

        let mtime = if path == keep_path {
            now
        } else {
            smol::fs::metadata(&path)
                .await
                .and_then(|metadata| metadata.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH)
        };

        candidates.push((path, mtime));
    }

    if candidates.len() <= limit {
        return Ok(());
    }

    candidates.sort_by(|(path_a, time_a), (path_b, time_b)| {
        time_b.cmp(time_a).then_with(|| path_a.cmp(path_b))
    });

    for (index, (path, _)) in candidates.into_iter().enumerate() {
        if index < limit || path == keep_path {
            continue;
        }

        if let Err(error) = smol::fs::remove_file(&path).await {
            log::warn!(
                "Failed to remove old remote server archive {:?}: {}",
                path,
                error
            );
        }
    }

    Ok(())
}
