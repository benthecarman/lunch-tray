//! The task window, built with egui. One `App` serves both the full window
//! and the compact popover opened from the tray.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use egui::containers::menu::{MenuButton, MenuConfig};
use egui::{
    Align, Align2, Button, Color32, CornerRadius, Id, Key, Label, Layout, Margin,
    PopupCloseBehavior, Rect, RichText, Sense, Stroke, StrokeKind, TextStyle, UiBuilder, Vec2,
    ViewportBuilder, ViewportCommand, pos2, vec2,
};

use crate::config::APP_ID;
use crate::model::{Origin, RemoteKind, RemoteState, Rung, Sections, Task, TaskState, ThemePref};
use crate::shared::{Notice, NoticeKind, SharedRef, UiRequest, WindowMode};
use crate::style::{
    self, Palette, RADIUS, WINDOW_RADIUS, danger_button, icon_button, icons, primary_button,
    section_style, soft_count, tint, title_style,
};
use crate::theme::SystemTheme;

const ROW_HEIGHT: f32 = 54.0;
const NOTICE_TTL: Duration = Duration::from_secs(8);
const ERROR_TTL: Duration = Duration::from_secs(20);

pub fn run_window(shared: SharedRef, mode: WindowMode, open_add: bool) -> eframe::Result {
    let size = 64;
    let icon = egui::IconData {
        rgba: crate::tray::icon_rgba(size),
        width: size as u32,
        height: size as u32,
    };
    let viewport = match mode {
        WindowMode::Full => ViewportBuilder::default()
            .with_title("Lunch Tray")
            .with_app_id(APP_ID)
            .with_inner_size([560.0, 720.0])
            .with_min_inner_size([420.0, 320.0])
            .with_icon(icon),
        WindowMode::Popover => ViewportBuilder::default()
            .with_title("Lunch Tray")
            .with_app_id(APP_ID)
            .with_inner_size([400.0, 560.0])
            .with_min_inner_size([320.0, 240.0])
            .with_decorations(false)
            .with_transparent(true)
            .with_icon(icon),
    };
    let options = eframe::NativeOptions {
        viewport,
        run_and_return: true,
        persist_window: false,
        centered: true,
        ..Default::default()
    };
    eframe::run_native(
        "Lunch Tray",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, shared, mode, open_add)))),
    )
}

/// The delete confirmation, or the task editor.
enum Modal {
    ConfirmDelete {
        id: String,
        title: String,
        /// True on the frame the dialog opened, so the key that opened it
        /// cannot also confirm it.
        fresh: bool,
    },
    EditTask {
        id: Option<String>,
        title: String,
        notes: String,
        link: String,
        manual: bool,
        focused: bool,
    },
    /// Archive every task with no activity for a year.
    ConfirmArchiveStale { ids: Vec<String>, fresh: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuAction {
    Rename,
    Pin,
    Unpin,
    Settle,
    Reopen,
    Archive,
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    OpenLink(String),
    Settle(String),
    Reopen(String),
    Archive(String),
    Delete(String),
    Rename(String),
    Pin(String, bool),
}

pub struct App {
    shared: SharedRef,
    mode: WindowMode,
    modal: Option<Modal>,
    theme_applied: Option<egui::Theme>,
    collapsed: HashSet<&'static str>,
    /// Row whose overflow menu is open, so its buttons stay while the
    /// pointer is inside the menu.
    menu_row: Option<String>,
    /// The popover closes when it loses focus, once it had focus and the
    /// pointer has been inside it.
    was_focused: bool,
    armed: bool,
    last_focus: Option<bool>,
    /// Development aid: `LUNCH_TRAY_SCREENSHOT=file.png` saves the window
    /// after a few frames and quits.
    screenshot: Option<(std::path::PathBuf, u32)>,
}

/// A snapshot of shared state for one frame.
struct Frame {
    sections: Sections,
    notices: Vec<Notice>,
    syncing: bool,
    last_sync: Option<DateTime<Utc>>,
    sync_errors: Vec<String>,
    theme: ThemePref,
    show_archived: bool,
}

impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        shared: SharedRef,
        mode: WindowMode,
        open_add: bool,
    ) -> Self {
        let mut app = App {
            shared,
            mode,
            modal: None,
            theme_applied: None,
            collapsed: HashSet::new(),
            menu_row: None,
            was_focused: false,
            armed: false,
            last_focus: None,
            screenshot: std::env::var_os("LUNCH_TRAY_SCREENSHOT")
                .map(|p| (std::path::PathBuf::from(p), 0)),
        };
        {
            let mut s = app.shared.lock().unwrap();
            s.ctx = Some(cc.egui_ctx.clone());
            s.window_mode = Some(mode);
        }
        style::apply(&cc.egui_ctx);
        // egui's default of 40 points per wheel notch feels sluggish next to
        // GTK apps. About one and a half rows per notch matches them.
        cc.egui_ctx
            .options_mut(|o| o.input_options.line_scroll_speed = 90.0);
        if open_add {
            app.modal = Some(new_task_modal());
        }
        app
    }

    fn apply_theme(&mut self, ctx: &egui::Context, pref: ThemePref, system: SystemTheme) {
        let theme = match (pref, system) {
            (ThemePref::Light, _) | (ThemePref::System, SystemTheme::Light) => egui::Theme::Light,
            (ThemePref::Dark, _) | (ThemePref::System, SystemTheme::Dark) => egui::Theme::Dark,
        };
        if self.theme_applied != Some(theme) {
            ctx.set_theme(theme);
            self.theme_applied = Some(theme);
        }
    }

    fn act(&mut self, action: Action) {
        match action {
            Action::OpenLink(id) => {
                let (url, manual) = {
                    let mut s = self.shared.lock().unwrap();
                    if s.store.mark_seen(&id) {
                        s.save_store();
                        s.notify();
                    }
                    let t = s.store.get(&id);
                    (
                        t.and_then(|t| t.url().map(str::to_string)),
                        t.is_some_and(|t| t.origin == Origin::Manual),
                    )
                };
                match url {
                    Some(url) => {
                        if let Err(e) = open::that_detached(&url) {
                            self.shared
                                .lock()
                                .unwrap()
                                .push_notice(Notice::error(format!("Could not open {url}: {e}")));
                        }
                    }
                    // A note without a link: open it for editing.
                    None if manual => self.act(Action::Rename(id)),
                    None => {}
                }
            }
            Action::Settle(id) => {
                let mut s = self.shared.lock().unwrap();
                let r = s.store.settle(&id);
                finish(&mut s, r);
            }
            Action::Reopen(id) => {
                let mut s = self.shared.lock().unwrap();
                let r = s.store.reopen(&id);
                finish(&mut s, r);
            }
            Action::Archive(id) => {
                let mut s = self.shared.lock().unwrap();
                let r = s.store.archive(&id);
                finish(&mut s, r);
            }
            Action::Delete(id) => {
                let title = self
                    .shared
                    .lock()
                    .unwrap()
                    .store
                    .get(&id)
                    .map(|t| t.title.clone())
                    .unwrap_or_default();
                self.modal = Some(Modal::ConfirmDelete {
                    id,
                    title,
                    fresh: true,
                });
            }
            Action::Rename(id) => {
                let s = self.shared.lock().unwrap();
                if let Some(t) = s.store.get(&id) {
                    self.modal = Some(Modal::EditTask {
                        id: Some(id.clone()),
                        title: t.title.clone(),
                        notes: t.notes.clone(),
                        link: t.link.clone().unwrap_or_default(),
                        manual: t.origin == Origin::Manual,
                        focused: false,
                    });
                }
            }
            Action::Pin(id, pinned) => {
                let mut s = self.shared.lock().unwrap();
                let r = s.store.set_pinned(&id, pinned);
                finish(&mut s, r);
            }
        }
    }

    fn confirm_delete(&mut self, id: &str) {
        let mut s = self.shared.lock().unwrap();
        let r = s.store.delete(id);
        finish(&mut s, r);
    }

    fn save_task(&mut self, id: Option<String>, title: &str, notes: &str, link: &str) -> bool {
        let mut s = self.shared.lock().unwrap();
        match id {
            None => {
                if title.trim().is_empty() {
                    s.push_notice(Notice::error("A task needs a title."));
                    return false;
                }
                s.store.add_manual(title, notes, Some(link.to_string()));
                s.save_store();
                s.notify();
                true
            }
            Some(id) => {
                let renamed = match s.store.rename(&id, title) {
                    Ok(changed) => changed,
                    Err(e) => {
                        s.push_notice(Notice::error(e));
                        return false;
                    }
                };
                let manual = s
                    .store
                    .get(&id)
                    .map(|t| t.origin == Origin::Manual)
                    .unwrap_or(false);
                let edited = manual
                    && s.store
                        .edit_manual(&id, notes, Some(link.to_string()))
                        .unwrap_or(false);
                if renamed || edited {
                    s.save_store();
                    s.notify();
                }
                true
            }
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.memory(|m| m.focused().is_some()) {
            return;
        }
        if self.mode == WindowMode::Popover && ctx.input(|i| i.key_pressed(Key::Escape)) {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }

    fn screenshot_hook(&mut self, ctx: &egui::Context) {
        let Some((path, frames)) = &mut self.screenshot else {
            return;
        };
        *frames += 1;
        ctx.request_repaint();
        if *frames == 6 {
            ctx.send_viewport_cmd(ViewportCommand::Screenshot(Default::default()));
        }
        let shot = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = shot {
            let [w, h] = image.size;
            let saved = image::RgbaImage::from_raw(w as u32, h as u32, image.as_raw().to_vec())
                .ok_or_else(|| "bad image buffer".to_string())
                .and_then(|img| img.save(&*path).map_err(|e| e.to_string()));
            match saved {
                Ok(()) => log::info!("screenshot saved to {}", path.display()),
                Err(e) => log::error!("screenshot: {e}"),
            }
            self.shared.lock().unwrap().quit();
        }
    }
}

/// Apply the result of a transition: persist on change, show errors.
fn finish(s: &mut crate::shared::Shared, r: Result<bool, String>) -> bool {
    match r {
        Ok(true) => {
            s.save_store();
            s.notify();
            true
        }
        Ok(false) => false,
        Err(e) => {
            s.push_notice(Notice::error(e));
            false
        }
    }
}

fn new_task_modal() -> Modal {
    Modal::EditTask {
        id: None,
        title: String::new(),
        notes: String::new(),
        link: String::new(),
        manual: true,
        focused: false,
    }
}

impl eframe::App for App {
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        match self.mode {
            WindowMode::Popover => [0.0, 0.0, 0.0, 0.0],
            WindowMode::Full => visuals.panel_fill.to_normalized_gamma_f32(),
        }
    }

    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &root.ctx().clone();
        let frame = {
            let mut s = self.shared.lock().unwrap();
            if s.ctx.is_none() {
                s.ctx = Some(ctx.clone());
                s.window_mode = Some(self.mode);
            }
            // Requests are for the full window; the popover leaves them
            // queued for it.
            let requests = if self.mode == WindowMode::Full {
                std::mem::take(&mut s.ui_requests)
            } else {
                Vec::new()
            };
            let pref = s.store.settings().theme;
            let system = s.system_theme;
            let now = std::time::Instant::now();
            s.notices.retain(|n| {
                let ttl = if n.kind == NoticeKind::Error {
                    ERROR_TTL
                } else {
                    NOTICE_TTL
                };
                now.duration_since(n.at) < ttl
            });
            let f = Frame {
                sections: s.store.sections(),
                notices: s.notices.clone(),
                syncing: s.sync.in_progress,
                last_sync: s.sync.last_finished,
                sync_errors: s.sync.errors.clone(),
                theme: pref,
                show_archived: s.store.settings().show_archived,
            };
            drop(s);
            self.apply_theme(ctx, pref, system);
            for r in requests {
                match r {
                    UiRequest::OpenAddDialog => self.modal = Some(new_task_modal()),
                }
            }
            f
        };
        if !frame.notices.is_empty() || frame.syncing {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        self.screenshot_hook(ctx);

        let p = Palette::for_theme(ctx.theme());
        let popover = self.mode == WindowMode::Popover;

        // Popover: close when focus leaves, and draw the rounded card.
        let focused = ctx.input(|i| i.viewport().focused);
        if focused != self.last_focus {
            log::debug!(
                "{:?} focus: {:?} -> {:?}",
                self.mode,
                self.last_focus,
                focused
            );
            self.last_focus = focused;
        }
        if popover {
            // GNOME may take focus back from a window opened without an
            // activation token, so a focus loss only counts once the pointer
            // has been inside the popover: that is the user leaving it.
            if ctx.input(|i| i.pointer.hover_pos().is_some()) {
                self.armed = true;
            }
            if focused == Some(true) {
                self.was_focused = true;
            } else if focused == Some(false)
                && self.was_focused
                && self.armed
                && self.screenshot.is_none()
            {
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            let card = root.max_rect();
            root.painter().rect(
                card,
                CornerRadius::same(WINDOW_RADIUS),
                p.bg,
                Stroke::new(1.0, p.border_strong),
                StrokeKind::Inside,
            );
        }

        let mut actions: Vec<Action> = Vec::new();

        let panel_frame = |margin: Margin| {
            if popover {
                egui::Frame::NONE.inner_margin(margin)
            } else {
                egui::Frame::NONE.fill(p.bg).inner_margin(margin)
            }
        };

        egui::Panel::top("top")
            .frame(panel_frame(Margin::symmetric(12, 10)))
            .show(root, |ui| {
                self.header(ui, &p, &frame);
            });

        egui::Panel::bottom("bottom")
            .frame(panel_frame(Margin::symmetric(14, 8)))
            .show(root, |ui| {
                self.footer(ui, &p, &frame);
            });

        egui::CentralPanel::default()
            .frame(panel_frame(Margin::symmetric(8, 0)))
            .show(root, |ui| {
                // Hairline under the header.
                let top = ui.max_rect().top();
                ui.painter()
                    .hline(ui.max_rect().x_range(), top, Stroke::new(1.0, p.border));
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add_space(6.0);
                        self.task_list(ui, &p, &frame, &mut actions);
                        ui.add_space(8.0);
                    });
            });

        self.notices_overlay(ctx, &p, &frame);

        for a in actions {
            self.act(a);
        }

        // While a modal is open no other keyboard handling runs.
        if self.modal.is_some() {
            self.modal_ui(ctx, &p);
        } else {
            self.handle_keys(ctx);
        }
    }

    fn on_exit(&mut self) {
        let mut s = self.shared.lock().unwrap();
        s.ctx = None;
        s.window_mode = None;
    }
}

impl App {
    fn header(&mut self, ui: &mut egui::Ui, p: &Palette, frame: &Frame) {
        let popover = self.mode == WindowMode::Popover;
        let header_rect = ui.available_rect_before_wrap();
        ui.horizontal(|ui| {
            ui.set_height(30.0);
            ui.add_space(2.0);
            ui.label(RichText::new(icons::TRAY).size(21.0).color(p.text));
            ui.add_space(2.0);
            ui.label(RichText::new("Lunch Tray").text_style(TextStyle::Heading));
            if popover {
                let n = frame.sections.pinned.len() + frame.sections.active.len();
                ui.add_space(4.0);
                soft_count(ui, n);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if popover {
                    if icon_button(ui, icons::X, "Close").clicked() {
                        ui.ctx().send_viewport_cmd(ViewportCommand::Close);
                    }
                    if icon_button(ui, icons::ARROWS_OUT_SIMPLE, "Open full window").clicked() {
                        self.shared.lock().unwrap().open_window(WindowMode::Full);
                    }
                } else {
                    self.app_menu(ui, p, frame);
                }
                let sync = ui.add_enabled_ui(!frame.syncing, |ui| {
                    icon_button(
                        ui,
                        icons::ARROWS_CLOCKWISE,
                        if frame.syncing {
                            "Syncing…"
                        } else {
                            "Sync now"
                        },
                    )
                });
                if sync.inner.clicked() {
                    self.shared.lock().unwrap().request_sync();
                }
                if icon_button(ui, icons::PLUS, "Add task").clicked() {
                    self.modal = Some(new_task_modal());
                }
            });
        });
        if popover {
            // The undecorated popover can be dragged by its header.
            let drag = ui.interact(header_rect, Id::new("popover-drag"), Sense::drag());
            if drag.drag_started() {
                ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
            }
        }
    }

    fn app_menu(&mut self, ui: &mut egui::Ui, p: &Palette, frame: &Frame) {
        let button = Button::new(RichText::new(icons::DOTS_THREE_VERTICAL).size(17.0))
            .frame_when_inactive(false)
            .min_size(Vec2::splat(30.0));
        MenuButton::from_button(button).ui(ui, |ui| {
            ui.set_min_width(200.0);
            ui.label(RichText::new("Theme").color(p.text_weak));
            let mut pref = frame.theme;
            let before = pref;
            for (value, icon, label) in [
                (ThemePref::System, icons::CIRCLE_HALF, "Follow system"),
                (ThemePref::Light, icons::SUN, "Light"),
                (ThemePref::Dark, icons::MOON, "Dark"),
            ] {
                ui.radio_value(&mut pref, value, format!("{icon}  {label}"));
            }
            if pref != before {
                let mut s = self.shared.lock().unwrap();
                s.store.settings_mut().theme = pref;
                s.save_store();
            }
            ui.separator();
            let mut show_archived = frame.show_archived;
            if ui
                .checkbox(
                    &mut show_archived,
                    format!("{}  Show archived", icons::ARCHIVE),
                )
                .changed()
            {
                let mut s = self.shared.lock().unwrap();
                s.store.settings_mut().show_archived = show_archived;
                s.save_store();
            }
            ui.separator();
            if ui
                .button(format!("{}  Archive older than a year", icons::ARCHIVE))
                .clicked()
            {
                let cutoff = Utc::now() - chrono::Duration::days(365);
                let mut s = self.shared.lock().unwrap();
                let ids = s.store.stale_ids(cutoff);
                if ids.is_empty() {
                    s.push_notice(Notice::info("Nothing has been quiet for a year."));
                } else {
                    drop(s);
                    self.modal = Some(Modal::ConfirmArchiveStale { ids, fresh: true });
                }
                ui.close();
            }
            if ui
                .button(format!("{}  Open config file", icons::GEAR))
                .clicked()
            {
                let path = crate::config::config_path();
                if let Err(e) = open::that_detached(&path) {
                    self.shared
                        .lock()
                        .unwrap()
                        .push_notice(Notice::error(format!(
                            "Could not open {}: {e}",
                            path.display()
                        )));
                }
                ui.close();
            }
            if ui.button(format!("{}  Close window", icons::X)).clicked() {
                ui.ctx().send_viewport_cmd(ViewportCommand::Close);
                ui.close();
            }
            if ui
                .button(
                    RichText::new(format!("{}  Quit Lunch Tray", icons::X_CIRCLE)).color(p.danger),
                )
                .clicked()
            {
                self.shared.lock().unwrap().quit();
                ui.close();
            }
        });
    }

    fn footer(&mut self, ui: &mut egui::Ui, p: &Palette, frame: &Frame) {
        ui.painter().hline(
            ui.max_rect().x_range(),
            ui.max_rect().top() - 8.0,
            Stroke::new(1.0, p.border),
        );
        ui.horizontal(|ui| {
            ui.set_height(22.0);
            let (icon, color, text) = if frame.syncing {
                (icons::SPINNER, p.text_weak, "Syncing…".to_string())
            } else if !frame.sync_errors.is_empty() {
                (icons::WARNING_CIRCLE, p.warning, "Sync error".to_string())
            } else if let Some(t) = frame.last_sync {
                (
                    icons::CHECK_CIRCLE,
                    p.text_weak,
                    format!("Synced {}", ago(t)),
                )
            } else {
                (
                    icons::CIRCLE_DASHED,
                    p.text_weak,
                    "Not synced yet".to_string(),
                )
            };
            let status = ui.label(
                RichText::new(format!("{icon}  {text}"))
                    .text_style(TextStyle::Small)
                    .color(color),
            );
            if !frame.sync_errors.is_empty() {
                status.on_hover_text(frame.sync_errors.join("\n"));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let settled = frame.sections.settled.len();
                let archived = frame.sections.archived.len();
                if self.mode == WindowMode::Popover {
                    if ui
                        .add(
                            Button::new(
                                RichText::new(format!("Show {settled} settled"))
                                    .text_style(TextStyle::Small)
                                    .color(p.text),
                            )
                            .frame_when_inactive(false),
                        )
                        .on_hover_text("Open the full window")
                        .clicked()
                    {
                        self.shared.lock().unwrap().open_window(WindowMode::Full);
                    }
                } else {
                    let mut text = format!("{settled} settled");
                    if archived > 0 {
                        text.push_str(&format!(", {archived} archived"));
                    }
                    ui.label(
                        RichText::new(text)
                            .text_style(TextStyle::Small)
                            .color(p.text_weak),
                    );
                }
            });
        });
    }

    fn task_list(
        &mut self,
        ui: &mut egui::Ui,
        p: &Palette,
        frame: &Frame,
        actions: &mut Vec<Action>,
    ) {
        let popover = self.mode == WindowMode::Popover;
        let s = &frame.sections;
        let shown = if popover {
            s.pinned.len() + s.active.len()
        } else {
            s.pinned.len() + s.active.len() + s.settled.len() + s.archived.len()
        };
        if shown == 0 {
            ui.add_space(56.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(icons::TRAY).size(44.0).color(p.text_weak));
                ui.add_space(6.0);
                ui.label(RichText::new("Nothing on your tray").text_style(title_style()));
                ui.label(
                    RichText::new(if popover {
                        "Nothing needs attention right now."
                    } else {
                        "Add a task, or wait for the next sync."
                    })
                    .text_style(TextStyle::Small)
                    .color(p.text_weak),
                );
            });
            return;
        }
        let menu_row = self.menu_row.take();
        // Development screenshots show the first row's actions.
        let mut force_actions = self.screenshot.is_some();
        let mut next_menu_row: Option<String> = None;
        let mut sections: Vec<(&'static str, &Vec<Task>)> =
            vec![("Pinned", &s.pinned), ("Active", &s.active)];
        if !popover {
            sections.push(("Settled", &s.settled));
            if frame.show_archived {
                sections.push(("Archived", &s.archived));
            }
        }
        for (name, tasks) in sections {
            if tasks.is_empty() {
                continue;
            }
            let collapsed = self.collapsed.contains(name);
            let well = egui::Frame::new()
                .fill(p.well)
                .stroke(Stroke::new(1.0, p.well_edge))
                .corner_radius(CornerRadius::same(12))
                .inner_margin(Margin {
                    left: 6,
                    right: 6,
                    top: 4,
                    bottom: if collapsed { 4 } else { 8 },
                });
            let resp = well.show(ui, |ui| {
                if section_header(ui, p, name, tasks.len(), collapsed) {
                    if collapsed {
                        self.collapsed.remove(name);
                    } else {
                        self.collapsed.insert(name);
                    }
                }
                if collapsed {
                    return;
                }
                for t in tasks {
                    let ctx = RowCtx {
                        compact: popover,
                        menu_open: menu_row.as_deref() == Some(t.id.as_str()),
                        force_actions: std::mem::take(&mut force_actions),
                    };
                    if row(ui, p, t, ctx, actions) {
                        next_menu_row = Some(t.id.clone());
                    }
                }
            });
            // A recessed lip: shadow along the top edge, light along the bottom.
            let r = resp.response.rect;
            let lip = r.x_range().shrink(14.0);
            ui.painter().hline(
                lip,
                r.top() + 1.5,
                Stroke::new(1.0, tint(Color32::BLACK, 18)),
            );
            ui.painter().hline(
                lip,
                r.bottom() - 1.5,
                Stroke::new(1.0, tint(Color32::WHITE, if p.dark { 14 } else { 120 })),
            );
            ui.add_space(8.0);
        }
        self.menu_row = next_menu_row;
    }

    fn notices_overlay(&mut self, ctx: &egui::Context, p: &Palette, frame: &Frame) {
        if frame.notices.is_empty() {
            return;
        }
        let mut dismiss: Option<usize> = None;
        egui::Area::new(Id::new("notices"))
            .anchor(Align2::RIGHT_BOTTOM, vec2(-14.0, -46.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.set_max_width(360.0);
                for (i, n) in frame.notices.iter().enumerate().rev().take(3) {
                    let (icon, color) = match n.kind {
                        NoticeKind::Info => (icons::INFO, p.text_weak),
                        NoticeKind::Error => (icons::WARNING_CIRCLE, p.danger),
                    };
                    egui::Frame::new()
                        .fill(p.field)
                        .stroke(Stroke::new(1.0, p.border_strong))
                        .corner_radius(CornerRadius::same(RADIUS))
                        .shadow(ui.visuals().popup_shadow)
                        .inner_margin(Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(icon).size(16.0).color(color));
                                ui.add(
                                    Label::new(RichText::new(&n.text).text_style(TextStyle::Body))
                                        .wrap(),
                                );
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if icon_button(ui, icons::X, "Dismiss").clicked() {
                                        dismiss = Some(i);
                                    }
                                });
                            });
                        });
                }
            });
        if let Some(i) = dismiss {
            // The list may have changed since this frame's snapshot, so match
            // the notice itself rather than its index.
            let target = &frame.notices[i];
            let mut s = self.shared.lock().unwrap();
            s.notices
                .retain(|n| !(n.at == target.at && n.text == target.text));
        }
    }

    fn modal_ui(&mut self, ctx: &egui::Context, p: &Palette) {
        let Some(modal) = self.modal.take() else {
            return;
        };
        let frame = egui::Frame::window(&ctx.style_of(ctx.theme()))
            .fill(p.bg)
            .inner_margin(Margin::same(20));
        match modal {
            Modal::ConfirmDelete { id, title, fresh } => {
                let mut confirmed = false;
                let mut cancel = false;
                let resp = egui::Modal::new(Id::new("confirm-delete"))
                    .frame(frame)
                    .show(ctx, |ui| {
                        ui.set_width(340.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(icons::TRASH).size(22.0).color(p.danger));
                            ui.label(
                                RichText::new("Delete this task?").text_style(TextStyle::Heading),
                            );
                        });
                        ui.add_space(6.0);
                        ui.add(Label::new(RichText::new(&title).text_style(title_style())).wrap());
                        ui.label(
                            RichText::new("This cannot be undone.")
                                .text_style(TextStyle::Small)
                                .color(p.text_weak),
                        );
                        ui.add_space(14.0);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if danger_button(ui, icons::TRASH, "Delete").clicked() {
                                confirmed = true;
                            }
                            if ui.button("Cancel").clicked() {
                                cancel = true;
                            }
                        });
                    });
                if !fresh && ctx.input(|i| i.key_pressed(Key::Enter)) {
                    confirmed = true;
                }
                if confirmed {
                    self.confirm_delete(&id);
                } else if !cancel && !resp.should_close() {
                    self.modal = Some(Modal::ConfirmDelete {
                        id,
                        title,
                        fresh: false,
                    });
                }
            }
            Modal::ConfirmArchiveStale { ids, fresh } => {
                let mut confirmed = false;
                let mut cancel = false;
                let n = ids.len();
                let label = if n == 1 {
                    "Archive 1 task".to_string()
                } else {
                    format!("Archive {n} tasks")
                };
                let resp = egui::Modal::new(Id::new("confirm-archive-stale"))
                    .frame(frame)
                    .show(ctx, |ui| {
                        ui.set_width(360.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(icons::ARCHIVE).size(22.0).color(p.text));
                            ui.label(
                                RichText::new(format!("{label}?")).text_style(TextStyle::Heading),
                            );
                        });
                        ui.add_space(6.0);
                        ui.add(
                            Label::new(
                                "Everything with no activity for a year, except pinned tasks. \
                                 They come back if something happens on them.",
                            )
                            .wrap(),
                        );
                        ui.add_space(14.0);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if primary_button(ui, icons::ARCHIVE, &label).clicked() {
                                confirmed = true;
                            }
                            if ui.button("Cancel").clicked() {
                                cancel = true;
                            }
                        });
                    });
                if !fresh && ctx.input(|i| i.key_pressed(Key::Enter)) {
                    confirmed = true;
                }
                if confirmed {
                    let mut s = self.shared.lock().unwrap();
                    let archived = s.store.archive_many(&ids);
                    s.save_store();
                    s.push_notice(Notice::info(if archived == 1 {
                        "Archived 1 task".to_string()
                    } else {
                        format!("Archived {archived} tasks")
                    }));
                } else if !cancel && !resp.should_close() {
                    self.modal = Some(Modal::ConfirmArchiveStale { ids, fresh: false });
                }
            }
            Modal::EditTask {
                id,
                mut title,
                mut notes,
                mut link,
                manual,
                mut focused,
            } => {
                let mut submit = false;
                let mut cancel = false;
                let (icon, heading) = match (&id, manual) {
                    (None, _) => (icons::PLUS, "New task"),
                    (Some(_), true) => (icons::PENCIL_SIMPLE, "Edit task"),
                    (Some(_), false) => (icons::PENCIL_SIMPLE, "Rename task"),
                };
                let resp = egui::Modal::new(Id::new("edit-task"))
                    .frame(frame)
                    .show(ctx, |ui| {
                        ui.set_width(400.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(icon).size(22.0).color(p.text));
                            ui.label(RichText::new(heading).text_style(TextStyle::Heading));
                        });
                        ui.add_space(8.0);
                        let title_edit = ui.add(
                            egui::TextEdit::singleline(&mut title)
                                .hint_text("Title")
                                .desired_width(f32::INFINITY)
                                .margin(Margin::symmetric(10, 7)),
                        );
                        if !focused {
                            title_edit.request_focus();
                            focused = true;
                        }
                        if title_edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                            submit = true;
                        }
                        if manual {
                            ui.add_space(6.0);
                            ui.add(
                                egui::TextEdit::singleline(&mut link)
                                    .hint_text("Link, optional")
                                    .desired_width(f32::INFINITY)
                                    .margin(Margin::symmetric(10, 7)),
                            );
                            ui.add_space(6.0);
                            ui.add(
                                egui::TextEdit::multiline(&mut notes)
                                    .hint_text("Notes, optional")
                                    .desired_rows(3)
                                    .desired_width(f32::INFINITY)
                                    .margin(Margin::symmetric(10, 7)),
                            );
                        }
                        ui.add_space(14.0);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let (ok_icon, ok) = if id.is_none() {
                                (icons::PLUS, "Add task")
                            } else {
                                (icons::CHECK, "Save")
                            };
                            if primary_button(ui, ok_icon, ok).clicked() {
                                submit = true;
                            }
                            if ui.button("Cancel").clicked() {
                                cancel = true;
                            }
                        });
                    });
                if submit {
                    if !self.save_task(id.clone(), &title, &notes, &link) {
                        self.modal = Some(Modal::EditTask {
                            id,
                            title,
                            notes,
                            link,
                            manual,
                            focused,
                        });
                    }
                } else if !cancel && !resp.should_close() {
                    self.modal = Some(Modal::EditTask {
                        id,
                        title,
                        notes,
                        link,
                        manual,
                        focused,
                    });
                }
            }
        }
        ctx.request_repaint();
    }
}

/// A compartment's label: caret, name, and a quiet count. Returns true
/// when clicked.
fn section_header(
    ui: &mut egui::Ui,
    p: &Palette,
    name: &str,
    count: usize,
    collapsed: bool,
) -> bool {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(width, 30.0), Sense::click());
    let mut inner = ui.new_child(
        UiBuilder::new()
            .max_rect(rect.shrink2(vec2(8.0, 0.0)))
            .layout(Layout::left_to_right(Align::Center))
            .id_salt(("section", name)),
    );
    let caret = if collapsed {
        icons::CARET_RIGHT
    } else {
        icons::CARET_DOWN
    };
    let color = match (name, resp.hovered()) {
        ("Pinned", _) => p.accent,
        (_, true) => p.text,
        (_, false) => p.text_weak,
    };
    inner.label(RichText::new(caret).size(12.0).color(p.text_weak));
    inner.label(RichText::new(name).text_style(section_style()).color(color));
    soft_count(&mut inner, count);
    resp.clicked()
}

fn ago(t: DateTime<Utc>) -> String {
    ago_from(t, Utc::now())
}

fn ago_from(t: DateTime<Utc>, now: DateTime<Utc>) -> String {
    const DAY: i64 = 86_400;
    let secs = (now - t).num_seconds().max(0);
    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < DAY {
        format!("{}h ago", secs / 3600)
    } else if secs < 30 * DAY {
        format!("{}d ago", secs / DAY)
    } else if secs < 365 * DAY {
        format!("{}mo ago", secs / (30 * DAY))
    } else {
        format!("{}y ago", secs / (365 * DAY))
    }
}

/// Leading glyph and its color for a task.
fn task_glyph(p: &Palette, t: &Task) -> (&'static str, Color32) {
    match &t.origin {
        Origin::Remote(r) => match (r.kind, r.state) {
            (RemoteKind::PullRequest, RemoteState::Open) => (icons::GIT_PULL_REQUEST, p.open),
            (RemoteKind::PullRequest, RemoteState::Merged) => (icons::GIT_MERGE, p.merged),
            (RemoteKind::PullRequest, RemoteState::Closed) => (icons::GIT_PULL_REQUEST, p.closed),
            (RemoteKind::Issue, RemoteState::Open) => (icons::CIRCLE_DASHED, p.open),
            (RemoteKind::Issue, _) => (icons::CHECK_CIRCLE, p.merged),
        },
        Origin::Manual => (icons::NOTE_PENCIL, p.note),
    }
}

/// One plain sentence under the title. The compact form fits the popover.
fn subtitle(t: &Task, compact: bool) -> String {
    match &t.origin {
        Origin::Remote(r) => {
            let what = match r.state {
                RemoteState::Merged => "merged",
                RemoteState::Closed => "closed",
                RemoteState::Open => "updated",
            };
            let when = ago(r.remote_updated_at);
            if compact {
                return format!("{} #{}, {what} {when}", r.repo, r.number);
            }
            let who = if r.author.is_empty() {
                String::new()
            } else {
                format!(" by {}", r.author)
            };
            format!("{}{who}, {what} {when}", r.label())
        }
        Origin::Manual => {
            if !t.notes.is_empty() {
                t.notes.lines().next().unwrap_or_default().to_string()
            } else if let Some(link) = t.link.as_deref() {
                link.trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .to_string()
            } else {
                format!("Added {}", ago(t.created_at))
            }
        }
    }
}

/// Per-row view state.
struct RowCtx {
    compact: bool,
    menu_open: bool,
    force_actions: bool,
}

/// One task row. Emits actions instead of mutating anything. Returns true
/// while the row's overflow menu is open.
fn row(ui: &mut egui::Ui, p: &Palette, t: &Task, ctx: RowCtx, actions: &mut Vec<Action>) -> bool {
    let RowCtx {
        compact,
        menu_open,
        force_actions,
    } = ctx;
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(width, ROW_HEIGHT), Sense::click());
    let hovered = ui.rect_contains_pointer(rect) || force_actions;
    let show_actions = hovered || menu_open;

    if hovered {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(RADIUS), p.bg);
    }
    // A click on the row opens it: the link in the browser, or the editor
    // for a note without a link.
    if resp.clicked() {
        actions.push(Action::OpenLink(t.id.clone()));
    }

    // Leading glyph in its state color, with a mustard badge for new activity.
    let (glyph, color) = task_glyph(p, t);
    let glyph_center = pos2(rect.min.x + 24.0, rect.center().y);
    ui.painter().text(
        glyph_center,
        Align2::CENTER_CENTER,
        glyph,
        egui::FontId::proportional(19.0),
        color,
    );
    if t.unseen && t.state == TaskState::Active {
        let dot = glyph_center + vec2(9.0, -9.0);
        ui.painter()
            .circle_filled(dot, 5.5, if hovered { p.bg } else { p.well });
        ui.painter().circle_filled(dot, 4.0, p.accent);
    }

    // Right side: action buttons, laid out first so the text can truncate.
    let buttons_w = if compact { 150.0 } else { 190.0 };
    let buttons_rect = Rect::from_min_max(
        pos2(rect.max.x - buttons_w, rect.min.y),
        pos2(rect.max.x - 6.0, rect.max.y),
    );
    let mut right = ui.new_child(
        UiBuilder::new()
            .max_rect(buttons_rect)
            .layout(Layout::right_to_left(Align::Center))
            .id_salt(("row-buttons", &t.id)),
    );
    right.spacing_mut().item_spacing.x = 2.0;
    let mut used_w = 0.0;
    let mut menu_is_open = false;
    if show_actions {
        let (chosen, open) = overflow_menu(&mut right, p, t);
        menu_is_open = open;
        if let Some(action) = chosen {
            actions.push(match action {
                MenuAction::Rename => Action::Rename(t.id.clone()),
                MenuAction::Pin => Action::Pin(t.id.clone(), true),
                MenuAction::Unpin => Action::Pin(t.id.clone(), false),
                MenuAction::Settle => Action::Settle(t.id.clone()),
                MenuAction::Reopen => Action::Reopen(t.id.clone()),
                MenuAction::Archive => Action::Archive(t.id.clone()),
                MenuAction::Delete => Action::Delete(t.id.clone()),
            });
        }
        let (icon, label, action) = match t.rung() {
            Rung::Active => (icons::CHECK, "Settle", Action::Settle(t.id.clone())),
            Rung::Settled => (icons::ARCHIVE, "Archive", Action::Archive(t.id.clone())),
            Rung::Archived => (icons::TRASH, "Delete", Action::Delete(t.id.clone())),
        };
        let primary = right.add(
            Button::new(RichText::new(format!("{icon}  {label}")).text_style(TextStyle::Small))
                .min_size(vec2(0.0, 26.0)),
        );
        if primary.clicked() {
            actions.push(action);
        }
        if t.url().is_some()
            && icon_button(&mut right, icons::ARROW_SQUARE_OUT, "Open in browser").clicked()
        {
            actions.push(Action::OpenLink(t.id.clone()));
        }
        used_w = buttons_rect.max.x - right.min_rect().min.x + 8.0;
    }

    // Title and meta line.
    let text_rect = Rect::from_min_max(
        pos2(rect.min.x + 46.0, rect.min.y),
        pos2(rect.max.x - used_w.max(8.0), rect.max.y),
    );
    let mut left = ui.new_child(
        UiBuilder::new()
            .max_rect(text_rect)
            .layout(Layout::top_down(Align::Min))
            .id_salt(("row-text", &t.id)),
    );
    left.add_space((ROW_HEIGHT - 36.0) / 2.0);
    left.spacing_mut().item_spacing = vec2(6.0, 2.0);
    left.horizontal(|ui| {
        if t.pinned && t.rung() == Rung::Active {
            ui.label(RichText::new(icons::PUSH_PIN).size(13.0).color(p.accent));
        }
        ui.add(
            Label::new(
                RichText::new(&t.title)
                    .text_style(title_style())
                    .color(p.text),
            )
            .truncate(),
        );
    });
    left.add(
        Label::new(
            RichText::new(subtitle(t, compact))
                .text_style(TextStyle::Small)
                .color(p.text_weak),
        )
        .truncate(),
    );
    menu_is_open
}

/// The overflow menu. Every item is closed from one place, here, before the
/// chosen action is returned to the caller.
fn overflow_menu(ui: &mut egui::Ui, p: &Palette, t: &Task) -> (Option<MenuAction>, bool) {
    let mut chosen = None;
    let button = Button::new(RichText::new(icons::DOTS_THREE_VERTICAL).size(16.0))
        .frame_when_inactive(false)
        .min_size(Vec2::splat(28.0));
    let (_, inner) = MenuButton::from_button(button)
        .config(MenuConfig::new().close_behavior(PopupCloseBehavior::CloseOnClickOutside))
        .ui(ui, |ui| {
            ui.set_min_width(170.0);
            let mut item = |ui: &mut egui::Ui, icon: &str, label: &str, action: MenuAction| {
                let text = RichText::new(format!("{icon}  {label}"));
                let text = if action == MenuAction::Delete {
                    text.color(p.danger)
                } else {
                    text
                };
                if ui.button(text).clicked() {
                    chosen = Some(action);
                }
            };
            item(ui, icons::PENCIL_SIMPLE, "Rename", MenuAction::Rename);
            match t.rung() {
                Rung::Active => {
                    if t.pinned {
                        item(ui, icons::PUSH_PIN_SLASH, "Unpin", MenuAction::Unpin);
                    } else {
                        item(ui, icons::PUSH_PIN, "Pin", MenuAction::Pin);
                    }
                    item(ui, icons::CHECK, "Settle", MenuAction::Settle);
                    item(ui, icons::ARCHIVE, "Archive", MenuAction::Archive);
                    item(ui, icons::TRASH, "Delete", MenuAction::Delete);
                }
                Rung::Settled => {
                    item(
                        ui,
                        icons::ARROW_COUNTER_CLOCKWISE,
                        "Reopen",
                        MenuAction::Reopen,
                    );
                    item(ui, icons::ARCHIVE, "Archive", MenuAction::Archive);
                    item(ui, icons::TRASH, "Delete", MenuAction::Delete);
                }
                Rung::Archived => {
                    item(
                        ui,
                        icons::ARROW_COUNTER_CLOCKWISE,
                        "Reopen",
                        MenuAction::Reopen,
                    );
                    item(ui, icons::TRASH, "Delete", MenuAction::Delete);
                }
            }
            if chosen.is_some() {
                ui.close();
            }
        });
    (chosen, inner.is_some() && chosen.is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_dates_roll_up_to_months_and_years() {
        let now = Utc::now();
        let d = |days: i64| ago_from(now - chrono::Duration::days(days), now);
        assert_eq!(d(0), "just now");
        assert_eq!(d(3), "3d ago");
        assert_eq!(d(29), "29d ago");
        assert_eq!(d(45), "1mo ago");
        assert_eq!(d(364), "12mo ago");
        assert_eq!(d(365), "1y ago");
        assert_eq!(d(1647), "4y ago");
    }
}
