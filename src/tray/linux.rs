// SPDX-License-Identifier: MPL-2.0
//! The tray on Linux: a StatusNotifierItem over D-Bus (`ksni`, pure Rust),
//! which KDE Plasma (the Steam Deck's desktop), Xfce and others show; GNOME
//! needs the AppIndicator extension.

use anyhow::{Context, Result};
use ksni::TrayMethods;
use ksni::menu::{MenuItem, StandardItem, SubMenu};
use tokio::sync::mpsc::UnboundedSender;

use super::{Action, Entry, View, menu, tooltip};

/// What `ksni` shows: the view, and where chosen actions go.
struct Item {
    view: View,
    actions: UnboundedSender<Action>,
    /// The icon theme folder holding `rommd`; `None` when it couldn't be
    /// written, and a stock icon stands in.
    icon_theme: Option<std::path::PathBuf>,
}

impl ksni::Tray for Item {
    fn id(&self) -> String {
        "rommd".into()
    }

    fn title(&self) -> String {
        "rommd".into()
    }

    fn icon_theme_path(&self) -> String {
        self.icon_theme
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    }

    /// rommd's own, or a freedesktop icon every theme has.
    fn icon_name(&self) -> String {
        if self.icon_theme.is_some() {
            "rommd"
        } else {
            "input-gaming"
        }
        .into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: tooltip(&self.view),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        menu(&self.view).into_iter().map(to_ksni).collect()
    }

    fn menu_about_to_show(&mut self) {
        let _ = self.actions.send(Action::Refresh);
    }
}

fn to_ksni(entry: Entry) -> MenuItem<Item> {
    match entry {
        Entry::Separator => MenuItem::Separator,
        Entry::Submenu { label, entries } => SubMenu {
            label,
            submenu: entries.into_iter().map(to_ksni).collect(),
            ..Default::default()
        }
        .into(),
        Entry::Item { label, action } => StandardItem {
            // ksni reads underscores as access keys; game names have them.
            label: label.replace('_', "__"),
            enabled: action.is_some(),
            activate: Box::new(move |item: &mut Item| {
                if let Some(action) = &action {
                    let _ = item.actions.send(action.clone());
                }
            }),
            ..Default::default()
        }
        .into(),
    }
}

pub struct Tray(ksni::Handle<Item>);

impl Tray {
    pub async fn spawn(
        actions: UnboundedSender<Action>,
        icon_theme: Option<std::path::PathBuf>,
    ) -> Result<Self> {
        let item = Item {
            view: View::default(),
            actions,
            icon_theme,
        };
        let handle = item
            .spawn()
            .await
            .context("showing the tray icon (does the desktop have a system tray?)")?;
        Ok(Self(handle))
    }

    pub async fn show(&self, view: View) {
        self.0.update(|item| item.view = view).await;
    }

    pub async fn shutdown(&self) {
        self.0.shutdown().await;
    }
}
