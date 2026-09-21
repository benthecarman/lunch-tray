//! StatusNotifierItem tray icon and menu.

use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender};

use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, StandardItem};

use crate::shared::{MainCmd, SharedRef, UiRequest, WindowMode};

/// Lets any thread ask the tray to rebuild itself without holding locks
/// across the tray service.
pub struct TrayLink {
    tx: Mutex<Option<Sender<()>>>,
}

impl TrayLink {
    pub fn new() -> Self {
        TrayLink {
            tx: Mutex::new(None),
        }
    }

    pub fn schedule_refresh(&self) {
        if let Some(tx) = self.tx.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let _ = tx.send(());
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct TraySnapshot {
    pub active: usize,
    pub pinned: usize,
    pub unseen: usize,
    pub syncing: bool,
    pub error: bool,
}

impl TraySnapshot {
    pub fn from_shared(shared: &SharedRef) -> Self {
        let s = shared.lock().unwrap_or_else(|e| e.into_inner());
        let sections = s.store.sections();
        TraySnapshot {
            active: sections.pinned.len() + sections.active.len(),
            pinned: sections.pinned.len(),
            unseen: s.store.count_unseen(),
            syncing: s.sync.in_progress,
            error: !s.sync.errors.is_empty(),
        }
    }
}

pub struct TrayApp {
    shared: SharedRef,
    snapshot: TraySnapshot,
}

impl ksni::Tray for TrayApp {
    fn id(&self) -> String {
        crate::config::APP_ID.into()
    }

    fn title(&self) -> String {
        "Lunch Tray".into()
    }

    /// Left click: toggle the compact popover. The native menu stays on the
    /// right click and only carries app-level actions.
    fn activate(&mut self, _x: i32, _y: i32) {
        self.shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .toggle_popover();
    }

    fn watcher_online(&self) {
        log::info!("tray host is up");
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        log::warn!("tray host is not available ({reason:?}); waiting for it");
        true
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let dot = if self.snapshot.error {
            Some(Dot::Error)
        } else if self.snapshot.unseen > 0 {
            Some(Dot::Attention)
        } else {
            None
        };
        vec![
            render_icon(22, dot),
            render_icon(32, dot),
            render_icon(48, dot),
        ]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let mut parts = vec![format!("{} active", self.snapshot.active)];
        if self.snapshot.pinned > 0 {
            parts.push(format!("{} pinned", self.snapshot.pinned));
        }
        if self.snapshot.unseen > 0 {
            parts.push(format!("{} new", self.snapshot.unseen));
        }
        if self.snapshot.syncing {
            parts.push("syncing".into());
        }
        if self.snapshot.error {
            parts.push("sync error".into());
        }
        ksni::ToolTip {
            title: "Lunch Tray".into(),
            description: parts.join(" · "),
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut status = format!("{} active", self.snapshot.active);
        if self.snapshot.unseen > 0 {
            status.push_str(&format!(", {} new", self.snapshot.unseen));
        }
        if self.snapshot.error {
            status.push_str(", sync error");
        }
        vec![
            StandardItem {
                label: status,
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Open Lunch Tray".into(),
                icon_name: "view-list-symbolic".into(),
                activate: Box::new(|this: &mut Self| {
                    this.shared
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .open_window(WindowMode::Full);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Add task…".into(),
                icon_name: "list-add-symbolic".into(),
                activate: Box::new(|this: &mut Self| {
                    let mut s = this.shared.lock().unwrap_or_else(|e| e.into_inner());
                    if s.window_mode == Some(WindowMode::Full) {
                        s.ui_requests.push(UiRequest::OpenAddDialog);
                        s.open_window(WindowMode::Full);
                    } else {
                        // No window, or the popover: reopen as Full with the
                        // dialog, after the current window has closed.
                        let _ = s.main_tx.send(MainCmd::OpenAddDialog);
                        if let Some(ctx) = &s.ctx {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            ctx.request_repaint();
                        }
                    }
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: if self.snapshot.syncing {
                    "Syncing…".into()
                } else {
                    "Sync now".into()
                },
                icon_name: "view-refresh-symbolic".into(),
                enabled: !self.snapshot.syncing,
                activate: Box::new(|this: &mut Self| {
                    this.shared
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .request_sync();
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit-symbolic".into(),
                activate: Box::new(|this: &mut Self| {
                    this.shared.lock().unwrap_or_else(|e| e.into_inner()).quit();
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Start the tray service and a refresher thread. Returns an error when no
/// StatusNotifier host is available; the app still works without a tray.
pub fn start(shared: SharedRef) -> Result<(), ksni::Error> {
    let tray = TrayApp {
        snapshot: TraySnapshot::from_shared(&shared),
        shared: shared.clone(),
    };
    // Register even if no tray host is up yet: under systemd we may start
    // before the shell's AppIndicator extension. ksni retries on its own.
    let handle = tray.assume_sni_available(true).spawn()?;
    let (tx, rx): (Sender<()>, Receiver<()>) = std::sync::mpsc::channel();
    *shared
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .tray
        .tx
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(tx);
    std::thread::Builder::new()
        .name("tray-refresh".into())
        .spawn(move || {
            while rx.recv().is_ok() {
                // Coalesce bursts of refresh requests.
                std::thread::sleep(std::time::Duration::from_millis(50));
                while rx.try_recv().is_ok() {}
                let snapshot = TraySnapshot::from_shared(&shared);
                if handle.update(|t| t.snapshot = snapshot).is_none() {
                    log::warn!("tray service is gone");
                    return;
                }
            }
        })
        .expect("spawn tray refresher");
    Ok(())
}

// ----- icon rendering -------------------------------------------------------

#[derive(Clone, Copy)]
enum Dot {
    Attention,
    Error,
}

fn render_icon(size: usize, dot: Option<Dot>) -> ksni::Icon {
    let badge = match dot {
        Some(Dot::Attention) => Some([0xe0, 0xb2, 0x3a]),
        Some(Dot::Error) => Some([0xef, 0x44, 0x44]),
        None => None,
    };
    // Panels are dark on GNOME: a light mark.
    let rgba = crate::brand::mark_rgba(size, [0xf2, 0xf2, 0xf2], badge);
    ksni::Icon {
        width: size as i32,
        height: size as i32,
        // ARGB32, network byte order.
        data: rgba
            .chunks(4)
            .flat_map(|p| [p[3], p[0], p[1], p[2]])
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `LUNCH_TRAY_ICON_DUMP=dir cargo test` writes the icons as PNG files.
    #[test]
    fn dump_icons_for_review() {
        let Some(dir) = std::env::var_os("LUNCH_TRAY_ICON_DUMP") else {
            return;
        };
        for (name, dot) in [("plain", None), ("attention", Some(Dot::Attention))] {
            let size = 96;
            let rgba: Vec<u8> = render_icon(size, dot)
                .data
                .chunks(4)
                .flat_map(|p| [p[1], p[2], p[3], p[0]])
                .collect();
            let img = image::RgbaImage::from_raw(size as u32, size as u32, rgba).unwrap();
            img.save(std::path::Path::new(&dir).join(format!("icon-{name}.png")))
                .unwrap();
        }
    }

    #[test]
    fn icon_has_visible_pixels_and_correct_size() {
        let icon = render_icon(22, None);
        assert_eq!(icon.data.len(), 22 * 22 * 4);
        let opaque = icon.data.chunks(4).filter(|p| p[0] > 200).count();
        assert!(opaque > 40, "glyph too faint: {opaque}");
        let with_dot = render_icon(22, Some(Dot::Attention));
        let orange = with_dot
            .data
            .chunks(4)
            .filter(|p| p[0] > 200 && p[1] == 0xe0)
            .count();
        assert!(orange > 4);
    }
}
