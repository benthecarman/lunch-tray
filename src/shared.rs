//! State shared between the window, the tray, and the sync thread.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::{DateTime, Utc};

use crate::model::Store;
use crate::sync::SyncCmd;
use crate::theme::SystemTheme;
use crate::tray::TrayLink;

pub type SharedRef = Arc<Mutex<Shared>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Error,
}

#[derive(Clone, Debug)]
pub struct Notice {
    pub kind: NoticeKind,
    pub text: String,
    pub at: Instant,
}

impl Notice {
    pub fn info(text: impl Into<String>) -> Self {
        Notice {
            kind: NoticeKind::Info,
            text: text.into(),
            at: Instant::now(),
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Notice {
            kind: NoticeKind::Error,
            text: text.into(),
            at: Instant::now(),
        }
    }
}

#[derive(Default)]
pub struct SyncStatus {
    pub in_progress: bool,
    pub last_finished: Option<DateTime<Utc>>,
    pub errors: Vec<String>,
}

/// The two shapes the window can take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowMode {
    /// The full task list with all sections.
    Full,
    /// A compact, undecorated panel opened from the tray icon.
    Popover,
}

/// Commands for the main thread, which owns the window lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MainCmd {
    OpenWindow(WindowMode),
    OpenAddDialog,
    Quit,
}

/// Requests queued for the window while it is open or the next time it opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiRequest {
    OpenAddDialog,
}

pub struct Shared {
    pub store: Store,
    pub notices: Vec<Notice>,
    pub sync: SyncStatus,
    pub system_theme: SystemTheme,
    /// Set while a window is open, so background threads can repaint it.
    pub ctx: Option<egui::Context>,
    pub window_mode: Option<WindowMode>,
    pub ui_requests: Vec<UiRequest>,
    pub tray: TrayLink,
    pub main_tx: Sender<MainCmd>,
    pub sync_tx: Sender<SyncCmd>,
}

impl Shared {
    pub fn push_notice(&mut self, n: Notice) {
        log::info!("notice: {}", n.text);
        self.notices.push(n);
        if self.notices.len() > 8 {
            self.notices.remove(0);
        }
        self.notify();
    }

    /// Ask the window to repaint and the tray to rebuild its menu.
    pub fn notify(&self) {
        if let Some(ctx) = &self.ctx {
            ctx.request_repaint();
        }
        self.tray.schedule_refresh();
    }

    pub fn save_store(&mut self) {
        if let Err(e) = self.store.save() {
            self.push_notice(Notice::error(format!("Could not save tasks: {e:#}")));
        }
    }

    pub fn request_sync(&self) {
        let _ = self.sync_tx.send(SyncCmd::Now);
    }

    /// Bring a window of `mode` up: focus it if it is already open, switch
    /// if a different mode is open, or ask the main thread to open one.
    pub fn open_window(&self, mode: WindowMode) {
        match (&self.ctx, self.window_mode) {
            (Some(ctx), Some(current)) if current == mode => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                ctx.request_repaint();
            }
            (Some(ctx), _) => {
                let _ = self.main_tx.send(MainCmd::OpenWindow(mode));
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                ctx.request_repaint();
            }
            (None, _) => {
                let _ = self.main_tx.send(MainCmd::OpenWindow(mode));
            }
        }
    }

    /// Left click on the tray icon: toggle the popover.
    pub fn toggle_popover(&self) {
        match (&self.ctx, self.window_mode) {
            (Some(ctx), Some(WindowMode::Popover)) => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                ctx.request_repaint();
            }
            _ => self.open_window(WindowMode::Popover),
        }
    }

    pub fn quit(&self) {
        let _ = self.main_tx.send(MainCmd::Quit);
        if let Some(ctx) = &self.ctx {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            ctx.request_repaint();
        }
    }
}
