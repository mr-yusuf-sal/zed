use std::{sync::Arc, thread::JoinHandle};

use anyhow::Context;
use cli::{CliRequest, CliResponse, IpcHandshake, ipc::IpcOneShotServer};
use parking_lot::Mutex;
use release_channel::app_identifier;
use util::ResultExt;
use windows::{
    Win32::{
        Foundation::{
            CloseHandle, ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, GENERIC_WRITE, GetLastError,
            HANDLE, HLOCAL, LocalFree,
        },
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1,
            },
            GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
            TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE, OPEN_EXISTING,
            PIPE_ACCESS_INBOUND, ReadFile, WriteFile,
        },
        System::{
            Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
                PIPE_TYPE_MESSAGE, PIPE_WAIT,
            },
            Threading::{CreateMutexW, GetCurrentProcess, OpenProcessToken},
        },
    },
    core::{HSTRING, PWSTR},
};

use crate::{Args, OpenListener, RawOpenRequest};

/// Security for the instance mutex and pipe, so that a Zed running elevated (started by a
/// highest-privilege task) still receives the files Explorer opens: Explorer launches Zed at
/// medium integrity, and objects an elevated process creates with default security carry a high
/// mandatory label and an administrators-only DACL, which lock the medium-integrity launch out
/// of both. The descriptor grants the current user explicitly and labels the objects medium.
struct InstanceSecurity {
    descriptor: PSECURITY_DESCRIPTOR,
}

impl InstanceSecurity {
    fn new() -> anyhow::Result<Self> {
        let user_sid = current_user_sid()?;
        let sddl = HSTRING::from(format!(
            "D:P(A;;GA;;;{user_sid})(A;;GA;;;SY)(A;;GA;;;BA)S:(ML;;NW;;;ME)"
        ));
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &sddl,
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .context("Failed to build the instance security descriptor")?;
        Ok(Self { descriptor })
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor.0,
            bInheritHandle: false.into(),
        }
    }
}

impl Drop for InstanceSecurity {
    fn drop(&mut self) {
        unsafe { LocalFree(Some(HLOCAL(self.descriptor.0))) };
    }
}

fn current_user_sid() -> anyhow::Result<String> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .context("Failed to open the process token")?;
    let user_sid = token_user_sid(token);
    unsafe { CloseHandle(token) }.log_err();
    user_sid
}

fn token_user_sid(token: HANDLE) -> anyhow::Result<String> {
    let mut length = 0u32;
    // This call only reports the buffer size; it fails by design because no buffer is passed.
    if let Err(error) = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut length) }
        && length == 0
    {
        return Err(error).context("Failed to query the token user size");
    }
    // u64 elements keep the buffer aligned for the pointer inside TOKEN_USER.
    let mut buffer = vec![0u64; (length as usize).div_ceil(std::mem::size_of::<u64>())];
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            length,
            &mut length,
        )
    }
    .context("Failed to query the token user")?;
    let token_user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut string_sid = PWSTR::null();
    unsafe { ConvertSidToStringSidW(token_user.User.Sid, &mut string_sid) }
        .context("Failed to convert the user SID")?;
    let user_sid = unsafe { string_sid.to_string() };
    unsafe { LocalFree(Some(HLOCAL(string_sid.0.cast()))) };
    Ok(user_sid?)
}

fn is_first_instance(security: Option<&InstanceSecurity>) -> bool {
    let attributes = security.map(InstanceSecurity::attributes);
    let mutex = unsafe {
        CreateMutexW(
            attributes.as_ref().map(|attributes| attributes as *const _),
            false,
            &HSTRING::from(format!("{}-Instance-Mutex", app_identifier())),
        )
    };
    match mutex {
        Ok(_) => unsafe { GetLastError() != ERROR_ALREADY_EXISTS },
        // The mutex exists but this process may not open it: an elevated Zed built before the
        // mutex had explicit security is still running.
        Err(error) if error.code() == ERROR_ACCESS_DENIED.to_hresult() => {
            log::error!("Another Zed instance is running and cannot be reached: {error}");
            false
        }
        Err(error) => {
            log::error!("Failed to create the instance mutex: {error}");
            true
        }
    }
}

pub fn handle_single_instance(opener: OpenListener, args: &Args) -> bool {
    let is_first_instance = is_first_instance(InstanceSecurity::new().log_err().as_ref());
    if is_first_instance {
        // We are the first instance, listen for messages sent from other instances
        std::thread::Builder::new()
            .name("EnsureSingleton".to_owned())
            .spawn(move || {
                with_pipe(&|url| {
                    opener.open(RawOpenRequest {
                        urls: vec![url],
                        ..Default::default()
                    })
                })
            })
            .unwrap();
    } else if !args.foreground && !args.hidden {
        // We are not the first instance, send args to the first instance. A hidden start
        // (e.g. at login) has nothing to hand off and must not bring the running Zed up.
        send_args_to_instance(args).log_err();
    }

    is_first_instance
}

fn with_pipe(f: &dyn Fn(String)) {
    let security = InstanceSecurity::new().log_err();
    let attributes = security.as_ref().map(InstanceSecurity::attributes);
    let pipe = unsafe {
        CreateNamedPipeW(
            &HSTRING::from(format!("\\\\.\\pipe\\{}-Named-Pipe", app_identifier())),
            PIPE_ACCESS_INBOUND,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
            1,
            128,
            128,
            0,
            attributes.as_ref().map(|attributes| attributes as *const _),
        )
    };
    drop(security);
    if pipe.is_invalid() {
        log::error!("Failed to create named pipe: {:?}", unsafe {
            GetLastError()
        });
        return;
    }

    loop {
        if let Some(message) = retrieve_message_from_pipe(pipe)
            .context("Failed to read from named pipe")
            .log_err()
        {
            f(message);
        }
    }
}

fn retrieve_message_from_pipe(pipe: HANDLE) -> anyhow::Result<String> {
    unsafe { ConnectNamedPipe(pipe, None)? };
    let message = retrieve_message_from_pipe_inner(pipe);
    unsafe { DisconnectNamedPipe(pipe).log_err() };
    message
}

fn retrieve_message_from_pipe_inner(pipe: HANDLE) -> anyhow::Result<String> {
    let mut buffer = [0u8; 128];
    unsafe {
        ReadFile(pipe, Some(&mut buffer), None, None)?;
    }
    let message = std::ffi::CStr::from_bytes_until_nul(&buffer)?;
    Ok(message.to_string_lossy().into_owned())
}

// This part of code is mostly from crates/cli/src/main.rs
fn send_args_to_instance(args: &Args) -> anyhow::Result<()> {
    if let Some(dock_menu_action_idx) = args.dock_action {
        let url = format!("zed-dock-action://{}", dock_menu_action_idx);
        return write_message_to_instance_pipe(url.as_bytes());
    }

    let (server, server_name) =
        IpcOneShotServer::<IpcHandshake>::new().context("Handshake before Zed spawn")?;
    let url = format!("zed-cli://{server_name}");

    let request = {
        let mut paths = vec![];
        let mut urls = vec![];
        let mut diff_paths = vec![];
        for path in args.paths_or_urls.iter() {
            match std::fs::canonicalize(&path) {
                Ok(path) => paths.push(path.to_string_lossy().into_owned()),
                Err(error) => {
                    if path.starts_with("zed://")
                        || path.starts_with("http://")
                        || path.starts_with("https://")
                        || path.starts_with("file://")
                        || path.starts_with("ssh://")
                    {
                        urls.push(path.clone());
                    } else {
                        log::error!("error parsing path argument: {}", error);
                    }
                }
            }
        }

        for path in args.diff.chunks(2) {
            let old = std::fs::canonicalize(&path[0]).log_err();
            let new = std::fs::canonicalize(&path[1]).log_err();
            if let Some((old, new)) = old.zip(new) {
                diff_paths.push([
                    old.to_string_lossy().into_owned(),
                    new.to_string_lossy().into_owned(),
                ]);
            }
        }

        CliRequest::Open {
            paths,
            urls,
            diff_paths,
            diff_all: false,
            wait: false,
            wsl: args.wsl.clone(),
            open_behavior: Default::default(),
            env: None,
            user_data_dir: args.user_data_dir.clone(),
            dev_container: args.dev_container,
            cwd: std::env::current_dir().ok(),
        }
    };

    let exit_status = Arc::new(Mutex::new(None));
    let sender: JoinHandle<anyhow::Result<()>> = std::thread::Builder::new()
        .name("CliReceiver".to_owned())
        .spawn({
            let exit_status = exit_status.clone();
            move || {
                let (_, handshake) = server.accept().context("Handshake after Zed spawn")?;
                let (tx, rx) = (handshake.requests, handshake.responses);

                tx.send(request)?;

                while let Ok(response) = rx.recv() {
                    match response {
                        CliResponse::Ping => {}
                        CliResponse::Stdout { message } => log::info!("{message}"),
                        CliResponse::Stderr { message } => log::error!("{message}"),
                        CliResponse::Exit { status } => {
                            exit_status.lock().replace(status);
                            return Ok(());
                        }
                        CliResponse::PromptOpenBehavior => {
                            tx.send(CliRequest::SetOpenBehavior {
                                behavior: cli::CliBehaviorSetting::ExistingWindow,
                            })?;
                        }
                    }
                }
                Ok(())
            }
        })
        .unwrap();

    write_message_to_instance_pipe(url.as_bytes())?;
    sender.join().unwrap()?;
    if let Some(exit_status) = exit_status.lock().take() {
        std::process::exit(exit_status);
    }
    Ok(())
}

fn write_message_to_instance_pipe(message: &[u8]) -> anyhow::Result<()> {
    unsafe {
        let pipe = CreateFileW(
            &HSTRING::from(format!("\\\\.\\pipe\\{}-Named-Pipe", app_identifier())),
            GENERIC_WRITE.0,
            FILE_SHARE_MODE::default(),
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES::default(),
            None,
        )?;
        WriteFile(pipe, Some(message), None, None)?;
        CloseHandle(pipe)?;
    }
    Ok(())
}
