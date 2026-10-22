//! Keeps this fork in step with the companion server in incapon (`workers/zed-fork`): it
//! installs builds published from `script\fork\build.ps1 -Publish`, and syncs the user's
//! config files and Claude Code sessions between machines under the GitHub account they
//! signed in to Zed with.

mod claude_sessions;
mod settings_sync;
mod updater;

use std::sync::Arc;

use anyhow::{Context as _, Result};
use client::{Client, ClientSettings};
use futures::AsyncReadExt as _;
use gpui::{App, AppContext as _, AsyncApp, actions};
use http_client::{AsyncBody, HttpClient, Method, Request, StatusCode};
use serde::{Serialize, de::DeserializeOwned};
use settings::Settings as _;
use workspace::notifications::{
    NotificationId, show_app_notification, simple_message_notification::MessageNotification,
};

const DEFAULT_SERVER_URL: &str = "https://zed.incapon.com";

actions!(
    fork_sync,
    [
        /// Checks the fork's update server for a newer build and installs it.
        CheckForUpdate,
        /// Syncs settings, keymap, tasks, snippets and themes with the fork's server now.
        SyncSettings,
        /// Syncs Claude Code sessions (~/.claude/projects) with the fork's server now.
        SyncClaudeSessions,
    ]
);

pub fn init(client: Arc<Client>, cx: &mut App) {
    updater::init(client.clone(), cx);
    settings_sync::init(client.clone(), cx);
    claude_sessions::init(client, cx);
}

fn server_url() -> String {
    std::env::var("ZED_FORK_SERVER_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

/// The `Authorization` header Zed itself sends to cloud.zed.dev. The server forwards it to
/// `/client/users/me` to learn the GitHub login, so the fork needs no sign-in of its own.
///
/// Read from the keychain rather than from `Client`, which keeps its credentials private.
async fn zed_authorization(client: &Client, cx: &AsyncApp) -> Result<Option<String>> {
    let credentials_url = cx.update(|cx| {
        let settings = ClientSettings::get_global(cx);
        settings
            .credentials_url
            .clone()
            .unwrap_or_else(|| settings.server_url.clone())
    });
    let Some((user_id, access_token)) = client
        .credentials_provider()
        .read_credentials(&credentials_url, cx)
        .await?
    else {
        return Ok(None);
    };
    let access_token = String::from_utf8(access_token).context("stored Zed token is not UTF-8")?;
    Ok(Some(format!("{user_id} {access_token}")))
}

#[derive(Debug)]
enum ServerError {
    Conflict,
    Unauthorized(String),
    Other(anyhow::Error),
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServerError::Conflict => write!(formatter, "the server has a newer version"),
            ServerError::Unauthorized(message) => {
                write!(formatter, "the fork server refused the Zed sign-in: {message}")
            }
            ServerError::Other(error) => write!(formatter, "{error:#}"),
        }
    }
}

impl std::error::Error for ServerError {}

impl From<anyhow::Error> for ServerError {
    fn from(error: anyhow::Error) -> Self {
        ServerError::Other(error)
    }
}

async fn request_json<Response: DeserializeOwned>(
    http: &Arc<dyn HttpClient>,
    method: Method,
    path: &str,
    authorization: Option<&str>,
    body: Option<&impl Serialize>,
) -> std::result::Result<Response, ServerError> {
    let body = match body {
        Some(body) => Some((
            "application/json",
            serde_json::to_vec(body).map_err(anyhow::Error::from)?,
        )),
        None => None,
    };
    let response = request_bytes(http, method, path, authorization, body).await?;
    Ok(serde_json::from_slice(&response)
        .with_context(|| format!("parsing the response of {path}"))?)
}

/// Sends `body` as `(content type, bytes)` and returns the response body of a 2xx answer.
async fn request_bytes(
    http: &Arc<dyn HttpClient>,
    method: Method,
    path: &str,
    authorization: Option<&str>,
    body: Option<(&str, Vec<u8>)>,
) -> std::result::Result<Vec<u8>, ServerError> {
    let url = format!("{}{path}", server_url());
    let mut builder = Request::builder().method(method).uri(&url);
    if let Some(authorization) = authorization {
        builder = builder.header("Authorization", authorization);
    }
    let body = match body {
        Some((content_type, bytes)) => {
            builder = builder.header("Content-Type", content_type);
            AsyncBody::from(bytes)
        }
        None => AsyncBody::empty(),
    };
    let request = builder.body(body).map_err(anyhow::Error::from)?;

    let mut response = http
        .send(request)
        .await
        .with_context(|| format!("requesting {url}"))?;
    let mut bytes = Vec::new();
    response
        .body_mut()
        .read_to_end(&mut bytes)
        .await
        .with_context(|| format!("reading the response of {url}"))?;

    match response.status() {
        status if status.is_success() => Ok(bytes),
        StatusCode::CONFLICT => Err(ServerError::Conflict),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(ServerError::Unauthorized(
            String::from_utf8_lossy(&bytes).into_owned(),
        )),
        status => Err(ServerError::Other(anyhow::anyhow!(
            "{url} answered {status}: {}",
            String::from_utf8_lossy(&bytes)
        ))),
    }
}

struct ForkSyncNotification;

fn notify(message: String, cx: &mut App) {
    show_app_notification(
        NotificationId::unique::<ForkSyncNotification>(),
        cx,
        move |cx| {
            let message = message.clone();
            cx.new(|cx| MessageNotification::new(message, cx))
        },
    );
}
