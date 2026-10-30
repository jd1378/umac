//! StatusNotifierItem tray (KDE, and other desktops with an SNI host) via `ksni`. Only plain
//! items and a radio group are used; ksni owns its D-Bus thread and calls back on it.

use anyhow::{Context, Result};
use ksni::{
    Icon, MenuItem, ToolTip, TrayMethods,
    menu::{RadioGroup, RadioItem, StandardItem, SubMenu},
};
use tokio::sync::mpsc::UnboundedSender;

use super::{Action, Menu, icon::Pixmap};

struct UmacTray {
    menu: Menu,
    icon: Vec<Pixmap>,
    actions: UnboundedSender<Action>,
}

fn item(label: &str, action: Action) -> MenuItem<UmacTray> {
    StandardItem {
        label: label.into(),
        activate: Box::new(move |t: &mut UmacTray| {
            let _ = t.actions.send(action.clone());
        }),
        ..Default::default()
    }
    .into()
}

impl ksni::Tray for UmacTray {
    fn id(&self) -> String {
        "umac".into()
    }

    fn title(&self) -> String {
        "umac".into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        self.icon.iter().map(|p| Icon { width: p.size as i32, height: p.size as i32, data: p.argb.clone() }).collect()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "umac".into(),
            description: self.menu.selected_label().map_or("No Mac selected".into(), |h| format!("Mac: {h}")),
            ..Default::default()
        }
    }

    /// Left click.
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.actions.send(Action::Apps);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let hosts = self.menu.hosts.clone();
        let mut items = vec![
            item("Apps", Action::Apps),
            item("Open a terminal window", Action::Terminal),
            item("Show Desktop", Action::Desktop),
            item("Show task manager", Action::TaskManager),
            item("Show settings", Action::Settings),
            MenuItem::Separator,
        ];
        if !hosts.is_empty() {
            let group = RadioGroup {
                selected: self.menu.selected.unwrap_or(usize::MAX),
                select: Box::new(move |t: &mut Self, i| {
                    if let Some(h) = hosts.get(i) {
                        t.menu.selected = Some(i);
                        let _ = t.actions.send(Action::SelectHost(h.fingerprint.clone()));
                    }
                }),
                options: self.menu.hosts.iter().map(|h| RadioItem { label: h.label.clone(), ..Default::default() }).collect(),
            };
            items.push(SubMenu { label: "Paired hosts".into(), submenu: vec![group.into()], ..Default::default() }.into());
            items.push(MenuItem::Separator);
        }
        items.push(item("Exit", Action::Exit));
        items
    }
}

pub struct Handle(ksni::Handle<UmacTray>);

pub async fn spawn(icon: Vec<Pixmap>, menu: Menu, actions: UnboundedSender<Action>) -> Result<Handle> {
    let tray = UmacTray { menu, icon, actions };
    let handle = tray.spawn().await.context("registering the tray icon (is a StatusNotifierItem host running?)")?;
    Ok(Handle(handle))
}

impl Handle {
    pub async fn set_menu(&self, menu: Menu) {
        self.0.update(|t| t.menu = menu).await;
    }

    pub async fn shutdown(&self) {
        self.0.shutdown().await;
    }
}
