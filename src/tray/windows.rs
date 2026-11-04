// SPDX-License-Identifier: MPL-2.0
//! The tray on Windows: a notification area icon (`tray-icon`,
//! Shell_NotifyIcon). Like macOS: the icon and its message pump stay on the
//! main thread, the tray's loop (`super::serve`) runs on tokio in a thread of
//! its own, and each view reaches the pump as a posted thread message.
//! Everything the icon touches lives in `STATE`, on the main thread only.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc::UnboundedSender;
use tray_icon::menu::{IsMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, GetLastError,
};
use windows_sys::Win32::System::Registry::{
    HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_DWORD, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, GetCurrentThreadId, OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE,
};
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, GetSystemMetrics, MSG, PostThreadMessageW, SM_CXSMICON,
    TranslateMessage, WM_APP, WM_QUIT,
};

use super::{Action, Entry, View, menu, tooltip};

/// How long a click waits for fresh news before the menu opens anyway.
const OPEN_WAIT: Duration = Duration::from_millis(300);

/// Posted to the main thread when the loop has sent a view.
const WM_SHOW: u32 = WM_APP + 1;

/// White on transparent, one per notification area size (16 px at 100 %
/// scaling, 20 at 125 %, 24 at 150 %, 32 at 200 %); black for a light
/// taskbar is made from them. Rendered from `assets/rommd-tray.svg`.
const ICONS: [(u32, &[u8]); 5] = [
    (16, include_bytes!("../../assets/rommd-tray-win-16.png")),
    (20, include_bytes!("../../assets/rommd-tray-win-20.png")),
    (24, include_bytes!("../../assets/rommd-tray-win-24.png")),
    (32, include_bytes!("../../assets/rommd-tray-win-32.png")),
    (48, include_bytes!("../../assets/rommd-tray-win-48.png")),
];

/// The single-instance mutex, per login session.
const MUTEX: &str = r"Local\rommd-tray";

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "rommd tray";

/// A menu as shown, and what its items do.
struct Shown {
    menu: Menu,
    actions: HashMap<MenuId, Action>,
}

struct State {
    tray: TrayIcon,
    /// Whether the icon shown is the one for a light taskbar.
    light: bool,
    entries: Vec<Entry>,
    current: Shown,
    /// The menu before, which may still be open when a late view replaces
    /// the current one; its items keep working until the next change.
    previous: Option<Shown>,
    views: mpsc::Receiver<View>,
    actions: UnboundedSender<Action>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// Runs `f` on the state; nothing off the main thread or before `run`.
fn with_state(f: impl FnOnce(&mut State)) {
    STATE.with_borrow_mut(|state| {
        if let Some(state) = state {
            f(state)
        }
    });
}

/// The tray's handle for the loop on tokio.
pub struct Tray {
    views: mpsc::Sender<View>,
    main_thread: u32,
}

impl Tray {
    pub async fn show(&self, view: View) {
        if self.views.send(view).is_ok() {
            // SAFETY: plain values; fails harmlessly once the thread is gone.
            unsafe { PostThreadMessageW(self.main_thread, WM_SHOW, 0, 0) };
        }
    }

    pub async fn shutdown(&self) {
        quit(self.main_thread);
    }
}

fn quit(main_thread: u32) {
    // SAFETY: plain values.
    unsafe { PostThreadMessageW(main_thread, WM_QUIT, 0, 0) };
}

/// Show the tray, run `serve` (the tray's loop, handed the `Tray` to show
/// views with) on a thread of its own, and pump messages here until the tray
/// quits.
pub fn run(
    actions: UnboundedSender<Action>,
    serve: impl FnOnce(Tray) -> Result<()> + Send + 'static,
) -> Result<()> {
    // Without it Windows reports 16 px icons at every scaling and blurs them
    // up; fails before Windows 10 1703, which then gets those.
    // SAFETY: a constant context; called before any window exists.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    let view = View::default();
    let entries = menu(&view);
    let current = build(&entries)?;
    let light = light_taskbar();
    let tray = TrayIconBuilder::new()
        .with_id("rommd")
        .with_icon(icon(light)?)
        .with_tooltip(tip(&view))
        .with_menu(Box::new(current.menu.clone()))
        .build()
        .context("showing the tray icon")?;
    let (views_tx, views) = mpsc::channel();
    STATE.set(Some(State {
        tray,
        light,
        entries,
        current,
        previous: None,
        views,
        actions,
    }));

    // Both run on this thread, from the pump: tray-icon calls this before it
    // opens the menu (on the button's release), and muda from the chosen item.
    TrayIconEvent::set_event_handler(Some(|event| match event {
        TrayIconEvent::Click {
            button: MouseButton::Left | MouseButton::Right,
            button_state: MouseButtonState::Up,
            ..
        } => with_state(refresh_before_opening),
        // A view posted while a menu was open went to its modal loop, which
        // drops thread messages; show it at least for the tooltip.
        TrayIconEvent::Enter { .. } => with_state(show_pending),
        _ => {}
    }));
    MenuEvent::set_event_handler(Some(|event: MenuEvent| {
        with_state(|state| {
            let action = state.current.actions.get(&event.id).or_else(|| {
                state
                    .previous
                    .as_ref()
                    .and_then(|p| p.actions.get(&event.id))
            });
            if let Some(action) = action {
                let _ = state.actions.send(action.clone());
            }
        });
    }));

    // SAFETY: no arguments. The thread has a message queue: building the
    // icon created a window.
    let main_thread = unsafe { GetCurrentThreadId() };
    let loop_thread = std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            let result = serve(Tray {
                views: views_tx,
                main_thread,
            });
            quit(main_thread);
            result
        })
        .context("starting the tray's thread")?;

    // SAFETY: an all-zero MSG is valid; it is only written by GetMessageW.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: `msg` is a valid MSG to write to; null = any window.
        match unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } {
            0 | -1 => break,
            _ if msg.hwnd.is_null() && msg.message == WM_SHOW => with_state(show_pending),
            _ => unsafe {
                // SAFETY: a message GetMessageW just filled in.
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            },
        }
    }
    // Dropping the icon removes it; left behind, it would stay in the
    // notification area until the mouse passes over it.
    STATE.take();
    if loop_thread.is_finished() {
        return loop_thread
            .join()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("the tray's thread panicked")));
    }
    Ok(())
}

/// The menu opens right after this returns; ask for news and give the
/// answer a moment, so the menu doesn't show a game that ended seconds ago.
fn refresh_before_opening(state: &mut State) {
    show_pending(state);
    if state.actions.send(Action::Refresh).is_err() {
        return;
    }
    if let Ok(view) = state.views.recv_timeout(OPEN_WAIT) {
        let latest = state.views.try_iter().last().unwrap_or(view);
        show(state, &latest);
    }
}

/// Show the newest view the loop sent, if any.
fn show_pending(state: &mut State) {
    if let Some(view) = state.views.try_iter().last() {
        show(state, &view);
    }
}

fn show(state: &mut State, view: &View) {
    let _ = state.tray.set_tooltip(Some(tip(view)));
    // Read on each view rather than on WM_SETTINGCHANGE, which only
    // tray-icon's own window receives.
    let light = light_taskbar();
    if light != state.light {
        match icon(light) {
            Ok(icon) => {
                let _ = state.tray.set_icon(Some(icon));
                state.light = light;
            }
            Err(e) => tracing::warn!("making the tray icon: {e:#}"),
        }
    }
    let entries = menu(view);
    if entries == state.entries {
        return;
    }
    match build(&entries) {
        Ok(shown) => {
            state.tray.set_menu(Some(Box::new(shown.menu.clone())));
            state.previous = Some(std::mem::replace(&mut state.current, shown));
            state.entries = entries;
        }
        Err(e) => tracing::warn!("building the tray menu: {e:#}"),
    }
}

/// The tooltip, cut to the 127 characters the notification area keeps
/// (tray-icon would drop the terminating NUL of a longer one).
fn tip(view: &View) -> String {
    let mut tip = tooltip(view);
    while tip.encode_utf16().count() > 127 {
        tip.pop();
    }
    tip
}

fn build(entries: &[Entry]) -> Result<Shown> {
    let menu = Menu::new();
    let mut actions = HashMap::new();
    for entry in entries {
        menu.append(to_muda(entry, &mut actions)?.as_ref())?;
    }
    Ok(Shown { menu, actions })
}

fn to_muda(entry: &Entry, actions: &mut HashMap<MenuId, Action>) -> Result<Box<dyn IsMenuItem>> {
    Ok(match entry {
        Entry::Separator => Box::new(PredefinedMenuItem::separator()),
        Entry::Submenu { label, entries } => {
            let submenu = Submenu::new(escape(label), true);
            for entry in entries {
                submenu.append(to_muda(entry, actions)?.as_ref())?;
            }
            Box::new(submenu)
        }
        Entry::Item { label, action } => {
            let item = MenuItem::new(escape(label), action.is_some(), None);
            if let Some(action) = action {
                actions.insert(item.id().clone(), action.clone());
            }
            Box::new(item)
        }
    })
}

/// muda reads `&` as an access key mark; game names have them.
fn escape(label: &str) -> String {
    label.replace('&', "&&")
}

/// The icon for the notification area's size, white, or black for a light
/// taskbar.
fn icon(light: bool) -> Result<Icon> {
    // SAFETY: no pointers.
    let wanted = u32::try_from(unsafe { GetSystemMetrics(SM_CXSMICON) }).unwrap_or(16);
    let (_, png) = ICONS
        .iter()
        .find(|(size, _)| *size >= wanted)
        .unwrap_or(&ICONS[ICONS.len() - 1]);
    let (mut rgba, width, height) = decode(png)?;
    if light {
        for pixel in rgba.as_chunks_mut::<4>().0 {
            pixel[..3].fill(0);
        }
    }
    Ok(Icon::from_rgba(rgba, width, height)?)
}

fn decode(png: &[u8]) -> Result<(Vec<u8>, u32, u32)> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(png)).read_info()?;
    anyhow::ensure!(
        reader.output_color_type() == (png::ColorType::Rgba, png::BitDepth::Eight),
        "the tray icon must be 8-bit RGBA"
    );
    let mut rgba = vec![0; reader.output_buffer_size().context("tray icon size")?];
    let frame = reader.next_frame(&mut rgba)?;
    rgba.truncate(frame.buffer_size());
    Ok((rgba, frame.width, frame.height))
}

/// Whether the taskbar is light (Settings > Personalisation > Colours,
/// "default Windows mode"); dark when unset, as before Windows 10 1903.
fn light_taskbar() -> bool {
    let key = wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let name = wide("SystemUsesLightTheme");
    let mut value: u32 = 0;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: NUL-terminated strings, and a DWORD with its size to fill.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&raw mut value).cast(),
            &mut size,
        )
    };
    status == ERROR_SUCCESS && value != 0
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ── One tray at a time ────────────────────────────────────────────────────────

/// Whether a tray holds the mutex, without taking it.
pub fn other_tray_running() -> bool {
    let name = wide(MUTEX);
    // SAFETY: a NUL-terminated name.
    let handle = unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr()) };
    if handle.is_null() {
        return false;
    }
    // SAFETY: the handle just opened.
    unsafe { CloseHandle(handle) };
    true
}

/// Take the mutex for this process's lifetime; false when another tray has
/// it.
pub fn claim() -> bool {
    let name = wide(MUTEX);
    // SAFETY: no security attributes, a NUL-terminated name. The handle is
    // never closed: the mutex lives as long as this process.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    // SAFETY: no arguments; read right after the call it reports on.
    let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if handle.is_null() {
        // Couldn't tell; running twice beats not running.
        return true;
    }
    if exists {
        // SAFETY: the handle just opened.
        unsafe { CloseHandle(handle) };
    }
    !exists
}

// ── Starting at login ─────────────────────────────────────────────────────────

/// The command the Run value starts: `--background`, since rommd is a
/// console program and would otherwise keep a console window open.
fn run_command(exe: &std::path::Path) -> String {
    format!("\"{}\" --background tray", exe.display())
}

/// Start the tray at login (a value in the user's `Run` key), or with
/// `enabled` false no longer.
pub fn autostart(enabled: bool) -> Result<()> {
    let key = wide(RUN_KEY);
    let name = wide(RUN_VALUE);
    let status = if enabled {
        let exe = std::env::current_exe().context("locating the rommd binary")?;
        let data = wide(&run_command(&exe));
        // SAFETY: NUL-terminated strings; the data's size in bytes includes
        // its NUL, as REG_SZ wants.
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                REG_SZ,
                data.as_ptr().cast(),
                (data.len() * 2) as u32,
            )
        }
    } else {
        // SAFETY: NUL-terminated strings.
        match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) } {
            ERROR_FILE_NOT_FOUND => ERROR_SUCCESS,
            status => status,
        }
    };
    if status != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(status as i32))
            .with_context(|| format!("writing HKCU\\{RUN_KEY}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_icons_decode() {
        for (size, png) in super::ICONS {
            let (_, width, height) = super::decode(png).unwrap();
            assert_eq!((width, height), (size, size));
        }
        super::icon(true).unwrap();
    }

    #[test]
    fn ampersands_stay() {
        assert_eq!(super::escape("Banjo & Kazooie"), "Banjo && Kazooie");
    }

    #[test]
    fn the_run_value_starts_the_tray_without_a_console() {
        assert_eq!(
            super::run_command(std::path::Path::new(r"C:\Users\A B\rommd.exe")),
            r#""C:\Users\A B\rommd.exe" --background tray"#
        );
    }
}
