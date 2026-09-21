//! Lunch Tray: a system tray todo list that follows your pull requests and
//! issues on GitHub and Forgejo.

mod brand;
mod config;
mod model;
mod shared;
mod style;
mod sync;
mod systemd;
mod theme;
mod tray;
mod ui;

use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;

use crate::config::{Config, store_path};
use crate::model::Store;
use crate::shared::{MainCmd, Notice, Shared, WindowMode};
use crate::sync::SyncCmd;
use crate::tray::TrayLink;

const USAGE: &str = "\
lunch-tray [--hidden]

  --hidden   Start with only the tray icon; open the window from the tray.
  --popover  Start with the compact popover instead of the full window.
  --export-icons DIR
             Write launcher icons into DIR, a hicolor theme directory such
             as ~/.local/share/icons/hicolor, then exit.
  --help     Show this text.

Config:  ~/.config/lunch-tray/config.toml
Tasks:   ~/.local/share/lunch-tray/tasks.json

Signals: SIGTERM or SIGINT quit cleanly, SIGHUP syncs now.
Under systemd the unit can use Type=notify; the app reports READY=1 once the
tray and sync threads are up.
";

fn main() -> anyhow::Result<()> {
    let mut logger = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(
        // Our own messages at info; the D-Bus and GPU stacks only when
        // they have something to say. RUST_LOG overrides this.
        "info,zbus=warn,tracing=warn,wgpu_core=warn,wgpu_hal=warn,egui_wgpu=warn,naga=warn,sctk_adwaita=warn",
    ));
    if systemd::under_journal() {
        logger.format_timestamp(None);
    }
    logger.init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return Ok(());
    }
    if let Some(i) = args.iter().position(|a| a == "--export-icons") {
        let dir = args
            .get(i + 1)
            .context("--export-icons needs a directory")?;
        brand::export_icons(std::path::Path::new(dir))?;
        println!("wrote icons under {dir}");
        return Ok(());
    }
    let mut show_window = !args.iter().any(|a| a == "--hidden");

    let (main_tx, main_rx) = channel::<MainCmd>();
    let (sync_tx, sync_rx) = channel::<SyncCmd>();

    let (cfg, config_error) = match Config::load() {
        Ok(c) => (c, None),
        Err(e) => (Config::default(), Some(format!("{e:#}"))),
    };
    let store = Store::load(&store_path()).context("load tasks")?;

    let shared = Arc::new(Mutex::new(Shared {
        store,
        notices: Vec::new(),
        sync: Default::default(),
        system_theme: theme::current(),
        ctx: None,
        window_mode: None,
        ui_requests: Vec::new(),
        tray: TrayLink::new(),
        main_tx: main_tx.clone(),
        sync_tx: sync_tx.clone(),
    }));
    if let Some(e) = config_error {
        shared
            .lock()
            .unwrap()
            .push_notice(Notice::error(format!("Config error, using defaults: {e}")));
    }

    // System theme watcher.
    {
        let (ttx, trx) = channel();
        std::thread::Builder::new()
            .name("theme-watch".into())
            .spawn(move || theme::watch(ttx))?;
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("theme-apply".into())
            .spawn(move || {
                for t in trx {
                    let mut s = shared.lock().unwrap();
                    if s.system_theme != t {
                        s.system_theme = t;
                        s.notify();
                    }
                }
            })?;
    }

    // Tray icon.
    if let Err(e) = tray::start(shared.clone()) {
        log::warn!("tray unavailable: {e}");
        shared.lock().unwrap().push_notice(Notice::error(format!(
            "No system tray found ({e}). Quit from the window menu."
        )));
        show_window = true;
    }

    // Signals: quit on TERM or INT, sync on HUP.
    {
        use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGTERM};
        let shared = shared.clone();
        let mut signals = signal_hook::iterator::Signals::new([SIGTERM, SIGINT, SIGHUP])?;
        std::thread::Builder::new()
            .name("signals".into())
            .spawn(move || {
                for sig in signals.forever() {
                    let s = shared.lock().unwrap_or_else(|e| e.into_inner());
                    if sig == SIGHUP {
                        log::info!("SIGHUP: syncing now");
                        s.request_sync();
                    } else {
                        log::info!("signal {sig}: quitting");
                        s.quit();
                    }
                }
            })?;
    }

    // Remote sync.
    let forges = sync::build_forges(&cfg, &shared);
    let interval = Duration::from_secs(cfg.poll_interval_secs.max(15));
    let on_close = cfg.on_close;
    {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("sync".into())
            .spawn(move || sync::run_loop(forges, shared, sync_rx, interval, on_close))?;
    }

    systemd::notify("READY=1\n");

    // The main thread owns the window. Closing the window returns here; the
    // tray reopens it.
    let start_mode = if args.iter().any(|a| a == "--popover") {
        WindowMode::Popover
    } else {
        WindowMode::Full
    };
    let mut next: Option<(WindowMode, bool)> = show_window.then_some((start_mode, false));
    loop {
        if let Some((mode, open_add)) = next.take() {
            log::info!("open {mode:?} window");
            if let Err(e) = ui::run_window(shared.clone(), mode, open_add) {
                log::error!("window: {e}");
            }
            log::info!("{mode:?} window closed");
            {
                let mut s = shared.lock().unwrap();
                s.ctx = None;
                s.window_mode = None;
            }
            // Requests that arrived while the window was open: keep the last
            // one. For a short while after the popover closes, a popover
            // request is the tray click that closed it, so drop those.
            let deadline = Instant::now() + Duration::from_millis(400);
            loop {
                let wait = if mode == WindowMode::Popover {
                    deadline.saturating_duration_since(Instant::now())
                } else {
                    Duration::ZERO
                };
                match main_rx.recv_timeout(wait) {
                    Ok(MainCmd::Quit) => return shutdown(&sync_tx),
                    Ok(MainCmd::OpenWindow(WindowMode::Popover))
                        if mode == WindowMode::Popover && next.is_none() => {}
                    Ok(MainCmd::OpenWindow(m)) => next = Some((m, false)),
                    Ok(MainCmd::OpenAddDialog) => next = Some((WindowMode::Full, true)),
                    Err(_) => break,
                }
            }
            if next.is_some() {
                continue;
            }
        }
        match main_rx.recv() {
            Ok(MainCmd::OpenWindow(mode)) => next = Some((mode, false)),
            Ok(MainCmd::OpenAddDialog) => next = Some((WindowMode::Full, true)),
            Ok(MainCmd::Quit) | Err(_) => return shutdown(&sync_tx),
        }
    }
}

fn shutdown(sync_tx: &std::sync::mpsc::Sender<SyncCmd>) -> anyhow::Result<()> {
    systemd::notify("STOPPING=1\n");
    let _ = sync_tx.send(SyncCmd::Quit);
    log::info!("bye");
    Ok(())
}
