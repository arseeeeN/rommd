// SPDX-License-Identifier: MPL-2.0
//! The tray on macOS: an item in the menu bar (`tray-icon`, NSStatusItem).
//! AppKit must run on the main thread, so the tray's loop (`super::serve`)
//! runs on tokio in a thread of its own and hands each view over through the
//! main dispatch queue. Everything AppKit touches lives in `STATE`, on the
//! main thread only.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use dispatch2::DispatchQueue;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use tokio::sync::mpsc::UnboundedSender;
use tray_icon::menu::{IsMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use super::{Action, Entry, View, menu, tooltip};

/// How long a click waits for fresh news before the menu opens anyway.
const OPEN_WAIT: Duration = Duration::from_millis(300);

/// Black on transparent, shown as a template image so the menu bar colours
/// it for light and dark; 44 px for the bar's 22 pt on Retina.
const ICON: &[u8] = include_bytes!("../../assets/rommd-tray-mac.png");

/// A menu as shown, and what its items do.
struct Shown {
    menu: Menu,
    actions: HashMap<MenuId, Action>,
}

struct State {
    tray: TrayIcon,
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
}

impl Tray {
    pub async fn show(&self, view: View) {
        if self.views.send(view).is_ok() {
            DispatchQueue::main().exec_async(|| with_state(show_pending));
        }
    }

    pub async fn shutdown(&self) {
        quit();
    }
}

fn quit() {
    DispatchQueue::main().exec_async(|| {
        if let Some(mtm) = MainThreadMarker::new() {
            NSApplication::sharedApplication(mtm).terminate(None);
        }
    });
}

/// Show the tray, run `serve` (the tray's loop, handed the `Tray` to show
/// views with) on a thread of its own, and AppKit here until the tray quits.
/// Must be called on the main thread; it doesn't return.
pub fn run(
    actions: UnboundedSender<Action>,
    serve: impl FnOnce(Tray) -> Result<()> + Send + 'static,
) -> Result<()> {
    let mtm = MainThreadMarker::new().context("the macOS tray must run on the main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    // A menu bar item only: no Dock icon, no app menu.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let view = View::default();
    let entries = menu(&view);
    let current = build(&entries)?;
    let tray = TrayIconBuilder::new()
        .with_id("rommd")
        .with_icon_templated(icon()?)
        .with_tooltip(tooltip(&view))
        .with_menu(Box::new(current.menu.clone()))
        .build()
        .context("showing the tray icon")?;
    let (views_tx, views) = mpsc::channel();
    STATE.set(Some(State {
        tray,
        entries,
        current,
        previous: None,
        views,
        actions,
    }));

    // Both run on the main thread: tray-icon calls this from the click,
    // before it opens the menu, and muda from the chosen item.
    TrayIconEvent::set_event_handler(Some(|event| {
        if let TrayIconEvent::Click {
            button_state: MouseButtonState::Down,
            ..
        } = event
        {
            with_state(refresh_before_opening);
        }
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

    std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || match serve(Tray { views: views_tx }) {
            Ok(()) => quit(),
            Err(e) => {
                eprintln!("Error: {e:#}");
                std::process::exit(1);
            }
        })
        .context("starting the tray's thread")?;
    app.run();
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
    let _ = state.tray.set_tooltip(Some(tooltip(view)));
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

fn icon() -> Result<Icon> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(ICON)).read_info()?;
    anyhow::ensure!(
        reader.output_color_type() == (png::ColorType::Rgba, png::BitDepth::Eight),
        "the tray icon must be 8-bit RGBA"
    );
    let mut rgba = vec![0; reader.output_buffer_size().context("tray icon size")?];
    let frame = reader.next_frame(&mut rgba)?;
    rgba.truncate(frame.buffer_size());
    Ok(Icon::from_rgba(rgba, frame.width, frame.height)?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_icon_decodes() {
        super::icon().unwrap();
    }

    #[test]
    fn ampersands_stay() {
        assert_eq!(super::escape("Banjo & Kazooie"), "Banjo && Kazooie");
    }
}
