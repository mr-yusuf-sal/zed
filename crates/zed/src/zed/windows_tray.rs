use std::cell::{Cell, RefCell};

use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use futures::{StreamExt as _, channel::mpsc};
use gpui::{Action, AnyWindowHandle, App, AppContext as _, Global, Keystroke, Window};
use raw_window_handle::RawWindowHandle;
use release_channel::ReleaseChannel;
use settings::{OnLastWindowClosed, Settings as _, SettingsStore};
use util::ResultExt as _;
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{
                MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey,
                UnregisterHotKey, VIRTUAL_KEY, VK_BACK, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE,
                VK_F1, VK_HOME, VK_INSERT, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT,
                VK_SPACE, VK_TAB, VK_UP, VkKeyScanW,
            },
            Shell::{
                NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
                Shell_NotifyIconW,
            },
            WindowsAndMessaging::{
                AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
                GetCursorPos, GetSystemMetrics, HICON, IMAGE_ICON, IsWindowVisible, LR_SHARED,
                LoadImageW, MF_SEPARATOR, MF_STRING, PostMessageW, RegisterClassW,
                RegisterWindowMessageW, SM_CXSMICON, SM_CYSMICON, SW_HIDE, SetForegroundWindow,
                ShowWindow, TPM_RIGHTBUTTON, TrackPopupMenu, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
                WM_COMMAND, WM_CONTEXTMENU, WM_HOTKEY, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP,
                WNDCLASSW,
            },
        },
    },
    core::{PCWSTR, w},
};
use workspace::{
    MultiWorkspace, WorkspaceSettings,
    notifications::{
        NotificationId, show_app_notification, simple_message_notification::MessageNotification,
    },
};

const TRAY_ICON_ID: u32 = 1;
const WM_TRAY_ICON: u32 = WM_APP + 1;
const MENU_SHOW: usize = 1;
const MENU_QUIT: usize = 2;

enum TrayEvent {
    Show,
    Quit,
    Hotkey(i32),
    TaskbarCreated,
}

thread_local! {
    // The window procedure has no way to reach `App`, so it forwards events to
    // a foreground task through this channel. It runs on the main thread, as
    // does the message loop that dispatches to it.
    static EVENTS: RefCell<Option<mpsc::UnboundedSender<TrayEvent>>> = const { RefCell::new(None) };
    static TASKBAR_CREATED: Cell<u32> = const { Cell::new(0) };
}

struct GlobalHotkeyError;

struct WindowsTray {
    hwnd: HWND,
    icon_visible: bool,
    applied_hotkeys: Option<HashMap<String, Option<String>>>,
    hotkey_actions: HashMap<i32, Option<Box<dyn Action>>>,
    last_hidden_window: Option<AnyWindowHandle>,
}

impl Global for WindowsTray {}

pub fn init(cx: &mut App) {
    let (events_tx, mut events_rx) = mpsc::unbounded();
    let hwnd = match create_event_window() {
        Ok(hwnd) => hwnd,
        Err(error) => {
            log::error!("failed to create the system tray window: {error:#}");
            return;
        }
    };
    EVENTS.with(|events| *events.borrow_mut() = Some(events_tx));

    cx.set_global(WindowsTray {
        hwnd,
        icon_visible: false,
        applied_hotkeys: None,
        hotkey_actions: HashMap::default(),
        last_hidden_window: None,
    });
    apply_settings(cx);
    cx.observe_global::<SettingsStore>(apply_settings).detach();
    cx.on_app_quit(|cx| {
        if let Some(tray) = cx.try_global::<WindowsTray>() {
            remove_tray_icon(tray.hwnd);
        }
        async {}
    })
    .detach();

    cx.spawn(async move |cx| {
        while let Some(event) = events_rx.next().await {
            cx.update(|cx| handle_event(event, cx));
        }
    })
    .detach();
}

/// Hides the window instead of closing it when it is the last visible Zed window
/// and the user asked for Zed to stay in the system tray. Returns whether it did.
pub fn hide_instead_of_closing(window: &mut Window, cx: &mut App) -> bool {
    if WorkspaceSettings::get_global(cx).on_last_window_closed != OnLastWindowClosed::HideToTray
        || !cx.has_global::<WindowsTray>()
    {
        return false;
    }

    let current_window = window.window_handle();
    let other_window_visible = cx
        .windows()
        .into_iter()
        .filter(|handle| *handle != current_window)
        .filter(|handle| handle.downcast::<MultiWorkspace>().is_some())
        .any(|handle| {
            handle
                .update(cx, |_, window, _| is_visible(window))
                .unwrap_or(false)
        });
    if other_window_visible {
        return false;
    }

    let Some(hwnd) = hwnd_of(window).log_err() else {
        return false;
    };
    unsafe { ShowWindow(hwnd, SW_HIDE).as_bool() };
    cx.global_mut::<WindowsTray>().last_hidden_window = Some(current_window);
    true
}

fn apply_settings(cx: &mut App) {
    let settings = WorkspaceSettings::get_global(cx);
    let show_icon = settings.on_last_window_closed == OnLastWindowClosed::HideToTray;
    let hotkeys = settings.global_hotkeys.clone();

    let tray = cx.global::<WindowsTray>();
    let hwnd = tray.hwnd;
    if show_icon != tray.icon_visible {
        let icon_visible = if show_icon {
            add_tray_icon(hwnd, ReleaseChannel::global(cx).display_name())
                .context("adding the system tray icon")
                .log_err()
                .is_some()
        } else {
            remove_tray_icon(hwnd);
            // With the icon gone, nothing would be left to bring hidden windows back.
            for handle in cx.windows() {
                if handle.downcast::<MultiWorkspace>().is_some() {
                    handle
                        .update(cx, |_, window, _| {
                            if !is_visible(window) {
                                window.activate_window();
                            }
                        })
                        .log_err();
                }
            }
            false
        };
        cx.global_mut::<WindowsTray>().icon_visible = icon_visible;
    }

    if cx.global::<WindowsTray>().applied_hotkeys.as_ref() == Some(&hotkeys) {
        return;
    }
    let previous_ids = cx
        .global::<WindowsTray>()
        .hotkey_actions
        .keys()
        .copied()
        .collect::<Vec<_>>();
    for id in previous_ids {
        unsafe { UnregisterHotKey(Some(hwnd), id) }.log_err();
    }

    let mut entries = hotkeys.iter().collect::<Vec<_>>();
    entries.sort_by_key(|(keystroke, _)| *keystroke);
    let mut hotkey_actions = HashMap::default();
    let mut errors = Vec::new();
    for (id, (keystroke, action_name)) in (1..).zip(entries) {
        let action = match action_name {
            Some(action_name) => match cx.build_action(action_name, None) {
                Ok(action) => Some(action),
                Err(error) => {
                    errors.push(format!("{keystroke}: {error}"));
                    continue;
                }
            },
            None => None,
        };
        match register_hotkey(hwnd, id, keystroke) {
            Ok(()) => {
                hotkey_actions.insert(id, action);
            }
            Err(error) => errors.push(format!("{keystroke}: {error:#}")),
        }
    }

    let tray = cx.global_mut::<WindowsTray>();
    tray.hotkey_actions = hotkey_actions;
    tray.applied_hotkeys = Some(hotkeys);

    if !errors.is_empty() {
        let message = format!(
            "Some global hotkeys could not be registered:\n{}",
            errors.join("\n")
        );
        log::error!("{message}");
        show_app_notification(
            NotificationId::unique::<GlobalHotkeyError>(),
            cx,
            move |cx| cx.new(|cx| MessageNotification::new(message.clone(), cx)),
        );
    }
}

fn handle_event(event: TrayEvent, cx: &mut App) {
    match event {
        TrayEvent::Show => show_window(None, cx),
        TrayEvent::Hotkey(id) => {
            let action = cx
                .global::<WindowsTray>()
                .hotkey_actions
                .get(&id)
                .map(|action| action.as_ref().map(|action| action.boxed_clone()));
            if let Some(action) = action {
                show_window(action, cx);
            }
        }
        TrayEvent::Quit => {
            // Quitting may prompt to save or confirm, which has to happen in a visible window.
            show_window(None, cx);
            cx.dispatch_action(&zed_actions::Quit);
        }
        TrayEvent::TaskbarCreated => {
            let tray = cx.global::<WindowsTray>();
            if tray.icon_visible {
                add_tray_icon(tray.hwnd, ReleaseChannel::global(cx).display_name())
                    .context("re-adding the system tray icon after Explorer restarted")
                    .log_err();
            }
        }
    }
}

fn show_window(action: Option<Box<dyn Action>>, cx: &mut App) {
    let workspace_windows = cx
        .windows()
        .into_iter()
        .filter(|handle| handle.downcast::<MultiWorkspace>().is_some())
        .collect::<Vec<_>>();
    let last_hidden_window = cx.global_mut::<WindowsTray>().last_hidden_window.take();
    let target = cx
        .active_window()
        .filter(|handle| workspace_windows.contains(handle))
        .or(last_hidden_window.filter(|handle| workspace_windows.contains(handle)))
        .or(workspace_windows.first().copied());

    let Some(target) = target else {
        cx.dispatch_action(&workspace::NewWindow);
        return;
    };
    target
        .update(cx, |_, window, cx| {
            window.activate_window();
            if let Some(action) = action {
                window.dispatch_action(action, cx);
            }
        })
        .log_err();
}

fn is_visible(window: &Window) -> bool {
    hwnd_of(window)
        .map(|hwnd| unsafe { IsWindowVisible(hwnd).as_bool() })
        .unwrap_or(false)
}

fn hwnd_of(window: &Window) -> Result<HWND> {
    let handle = raw_window_handle::HasWindowHandle::window_handle(window)
        .map_err(|error| anyhow!("getting the window handle: {error}"))?;
    match handle.as_raw() {
        RawWindowHandle::Win32(handle) => Ok(HWND(handle.hwnd.get() as _)),
        _ => Err(anyhow!("window has no Win32 handle")),
    }
}

fn register_hotkey(hwnd: HWND, id: i32, source: &str) -> Result<()> {
    let keystroke = Keystroke::parse(source)?;
    let modifiers = keystroke.modifiers;
    if modifiers.function {
        return Err(anyhow!("the fn modifier cannot be used in a global hotkey"));
    }
    let mut flags = MOD_NOREPEAT;
    for (enabled, flag) in [
        (modifiers.control, MOD_CONTROL),
        (modifiers.alt, MOD_ALT),
        (modifiers.shift, MOD_SHIFT),
        (modifiers.platform, MOD_WIN),
    ] {
        if enabled {
            flags |= flag;
        }
    }
    if flags == MOD_NOREPEAT {
        return Err(anyhow!("a global hotkey needs at least one modifier"));
    }
    let virtual_key = virtual_key(&keystroke.key)
        .with_context(|| format!("unsupported key {:?}", keystroke.key))?;
    unsafe { RegisterHotKey(Some(hwnd), id, flags, virtual_key.0 as u32) }
        .context("the shortcut is already taken by Windows or another application")
}

fn virtual_key(key: &str) -> Option<VIRTUAL_KEY> {
    let named = match key {
        "space" => VK_SPACE,
        "enter" => VK_RETURN,
        "tab" => VK_TAB,
        "escape" => VK_ESCAPE,
        "backspace" => VK_BACK,
        "delete" => VK_DELETE,
        "insert" => VK_INSERT,
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" => VK_PRIOR,
        "pagedown" => VK_NEXT,
        "up" => VK_UP,
        "down" => VK_DOWN,
        "left" => VK_LEFT,
        "right" => VK_RIGHT,
        _ => {
            if let Some(number) = key
                .strip_prefix('f')
                .and_then(|number| number.parse::<u16>().ok())
                .filter(|number| (1..=24).contains(number))
            {
                return Some(VIRTUAL_KEY(VK_F1.0 + number - 1));
            }
            // Characters go through the active keyboard layout, so `ctrl-alt-ş`
            // works on a Turkish keyboard the same way `ctrl-alt-;` does on a US one.
            let mut units = key.encode_utf16();
            let (Some(unit), None) = (units.next(), units.next()) else {
                return None;
            };
            let scan = unsafe { VkKeyScanW(unit) };
            if scan == -1 {
                return None;
            }
            return Some(VIRTUAL_KEY((scan as u16) & 0xff));
        }
    };
    Some(named)
}

fn create_event_window() -> Result<HWND> {
    let module = unsafe { GetModuleHandleW(None) }.context("getting the module handle")?;
    let class_name = w!("ZedSystemTrayWindow");
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_procedure),
        hInstance: module.into(),
        lpszClassName: class_name,
        ..Default::default()
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(windows::core::Error::from_thread()).context("registering the window class");
    }
    TASKBAR_CREATED
        .with(|message| message.set(unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }));

    // A hidden top-level window rather than a message-only one, because only
    // top-level windows receive the `TaskbarCreated` broadcast sent when
    // Explorer restarts and every tray icon has to be added again.
    unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!("Zed"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(module.into()),
            None,
        )
    }
    .context("creating the window")
}

fn notify_icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        ..Default::default()
    }
}

fn add_tray_icon(hwnd: HWND, tooltip: &str) -> Result<()> {
    let module = unsafe { GetModuleHandleW(None) }.context("getting the module handle")?;
    let icon = unsafe {
        LoadImageW(
            Some(module.into()),
            PCWSTR(1 as _),
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_SHARED,
        )
    }
    .context("loading the application icon")?;

    let mut data = notify_icon_data(hwnd);
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = WM_TRAY_ICON;
    data.hIcon = HICON(icon.0);
    // The last slot stays zero to terminate the string.
    let tooltip_capacity = data.szTip.len() - 1;
    for (slot, unit) in data
        .szTip
        .iter_mut()
        .take(tooltip_capacity)
        .zip(tooltip.encode_utf16())
    {
        *slot = unit;
    }
    if unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
        Ok(())
    } else {
        Err(anyhow!("Shell_NotifyIconW refused to add the icon"))
    }
}

fn remove_tray_icon(hwnd: HWND) {
    let data = notify_icon_data(hwnd);
    if !unsafe { Shell_NotifyIconW(NIM_DELETE, &data) }.as_bool() {
        log::warn!("failed to remove the system tray icon");
    }
}

fn show_context_menu(hwnd: HWND) -> Result<()> {
    unsafe {
        let menu = CreatePopupMenu()?;
        AppendMenuW(menu, MF_STRING, MENU_SHOW, w!("Show Zed"))?;
        AppendMenuW(menu, MF_SEPARATOR, 0, None)?;
        AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("Quit Zed"))?;
        let mut point = POINT::default();
        GetCursorPos(&mut point)?;
        // Without this the menu stays open when the user clicks elsewhere, and
        // the WM_NULL afterwards keeps it from closing immediately the next time.
        SetForegroundWindow(hwnd).as_bool();
        TrackPopupMenu(menu, TPM_RIGHTBUTTON, point.x, point.y, None, hwnd, None).as_bool();
        PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0))?;
        DestroyMenu(menu)?;
    }
    Ok(())
}

fn send_event(event: TrayEvent) {
    EVENTS.with(|events| {
        if let Some(events) = events.borrow().as_ref() {
            events.unbounded_send(event).log_err();
        }
    });
}

unsafe extern "system" fn window_procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_HOTKEY => send_event(TrayEvent::Hotkey(wparam.0 as i32)),
        WM_TRAY_ICON => match lparam.0 as u32 {
            WM_LBUTTONUP => send_event(TrayEvent::Show),
            WM_RBUTTONUP | WM_CONTEXTMENU => {
                show_context_menu(hwnd)
                    .context("showing the system tray menu")
                    .log_err();
            }
            _ => {}
        },
        WM_COMMAND => match wparam.0 & 0xffff {
            MENU_SHOW => send_event(TrayEvent::Show),
            MENU_QUIT => send_event(TrayEvent::Quit),
            _ => {}
        },
        message if message == TASKBAR_CREATED.with(Cell::get) && message != 0 => {
            send_event(TrayEvent::TaskbarCreated)
        }
        _ => return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
    LRESULT(0)
}
