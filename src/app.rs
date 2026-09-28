//! Application object, single-instance dispatch, and shared state.

use crate::capture::{overlay, CaptureMode, Frame, Rect};
use crate::config::{Config, ConfigHandle};
use crate::history::History;
use crate::quickaccess::QuickAccessPanel;
use crate::{Cli, Command};
use clap::Parser;
use gtk::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::rc::Rc;

pub struct Omacapture {
    pub app: adw::Application,
    pub config: ConfigHandle,
    pub history: RefCell<History>,
    pub quick_access: RefCell<QuickAccessPanel>,
    pub last_area: RefCell<Option<Rect>>,
    /// `--wait`: print a JSON result line to stdout when the capture completes, then quit.
    pub wait_mode: std::cell::Cell<bool>,
    _config_monitor: RefCell<Option<gio::FileMonitor>>,
    _hold: RefCell<Option<gio::ApplicationHoldGuard>>,
}

thread_local! {
    static INSTANCE: RefCell<Option<Rc<Omacapture>>> = const { RefCell::new(None) };
}

pub fn instance() -> Rc<Omacapture> {
    INSTANCE.with(|i| i.borrow().clone().expect("app not started"))
}

pub fn run() -> glib::ExitCode {
    crate::paths::ensure_dirs();
    let wait = std::env::args().any(|a| a == "--wait");
    let mut flags = gio::ApplicationFlags::HANDLES_COMMAND_LINE;
    if wait {
        flags |= gio::ApplicationFlags::NON_UNIQUE;
    }
    let app = adw::Application::new(Some(crate::paths::APP_ID), flags);

    app.connect_startup(|app| {
        crate::theme::install();
        let config = Config::load();
        let history = History::open().expect("history db");
        let _ = history.prune(config.history.retention_days, config.history.max_entries);
        crate::paths::sweep_temp(config.history.retention_days.max(1));
        let config_handle = ConfigHandle::new(config);
        let config_monitor = config_handle.watch();
        let gb = Rc::new(Omacapture {
            app: app.clone(),
            config: config_handle,
            history: RefCell::new(history),
            quick_access: RefCell::new(QuickAccessPanel::new()),
            last_area: RefCell::new(None),
            wait_mode: std::cell::Cell::new(false),
            _config_monitor: RefCell::new(config_monitor),
            _hold: RefCell::new(None),
        });
        INSTANCE.with(|i| *i.borrow_mut() = Some(gb));
    });

    app.connect_command_line(|app, cmdline| {
        let args = crate::normalize_args(cmdline.arguments());
        let cli = match Cli::try_parse_from(&args) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{e}");
                return glib::ExitCode::from(2);
            }
        };
        let gb = instance();
        if cli.wait {
            gb.wait_mode.set(true);
        }
        dispatch(&gb, cli.command.unwrap_or(Command::Area { annotate: false }));
        let _ = app;
        glib::ExitCode::SUCCESS
    });

    app.run()
}

pub fn dispatch(gb: &Rc<Omacapture>, cmd: Command) {
    tracing::debug!("dispatch {cmd:?}");
    match cmd {
        Command::Daemon => {
            *gb._hold.borrow_mut() = Some(gb.app.hold());
            match crate::keybinds::restore_from_config(true) {
                Ok(true) => tracing::info!("restored saved Hyprland keybind preset"),
                Ok(false) => {}
                Err(e) => tracing::warn!("keybinds restore: {e}"),
            }
            tracing::info!("daemon running");
        }
        Command::Full => capture_fullscreen(gb),
        Command::Area { annotate } => capture_area(gb, annotate),
        Command::Window => capture_window(gb),
        Command::Ocr => capture_ocr(gb),
        Command::Annotate { file } => crate::annotate::open_file(gb, &file),
        Command::History => crate::history::browser::open(gb),
        Command::Settings => crate::annotate::preferences::open(gb),
        Command::Qa { action } => {
            if !gb.quick_access.borrow().act(action) {
                tracing::info!("no Quick Access card to act on");
            }
        }
        Command::Mcp { .. } | Command::Keybinds { .. } => {}
    }
}

/// In `--wait` mode, report a finished capture on stdout and exit.
pub fn report_wait_result(gb: &Rc<Omacapture>, path: Option<&std::path::Path>, width: u32, height: u32) {
    if !gb.wait_mode.get() {
        return;
    }
    let v = match path {
        Some(p) => serde_json::json!({"path": p, "width": width, "height": height}),
        None => serde_json::json!({"cancelled": true}),
    };
    println!("{v}");
    let app = gb.app.clone();
    glib::idle_add_local_once(move || app.quit());
}

fn with_delay(gb: &Rc<Omacapture>, f: impl FnOnce() + 'static) {
    let delay = gb.config.get().general.delay_ms;
    if delay == 0 {
        f();
    } else {
        let hold = gb.app.hold();
        glib::timeout_add_local_once(std::time::Duration::from_millis(delay as u64), move || {
            f();
            drop(hold);
        });
    }
}

fn capture_fullscreen(gb: &Rc<Omacapture>) {
    let gb = gb.clone();
    with_delay(&gb.clone(), move || {
        let cfg = gb.config.get();
        let monitors = crate::capture::hypr::monitors().unwrap_or_default();
        if monitors.is_empty() {
            match crate::capture::grim::capture_region(Rect::new(0, 0, 0, 0), 1.0, cfg.general.include_cursor) {
                Ok(frame) => {
                    crate::postcapture::handle(&gb, frame, CaptureMode::Fullscreen);
                }
                Err(e) => tracing::error!("{e}"),
            }
            return;
        }
        // One file per monitor, like the original.
        for m in monitors {
            match crate::capture::grim::capture_output(&m.name, m.scale, cfg.general.include_cursor) {
                Ok(frame) => {
                    crate::postcapture::handle(&gb, frame, CaptureMode::Fullscreen);
                }
                Err(e) => tracing::error!("{e}"),
            }
        }
    });
}

fn start_pick(gb: &Rc<Omacapture>, mode: overlay::PickMode, on_frame: impl FnOnce(&Rc<Omacapture>, Frame, Rect) + 'static) {
    let cfg = gb.config.get();
    let remembered = if cfg.general.remember_last_area { *gb.last_area.borrow() } else { None };
    let hold = gb.app.hold();
    let gb2 = gb.clone();
    overlay::pick(&gb.app, mode, cfg.general.include_cursor, remembered, move |sel| {
        match sel {
            Some(sel) => {
                *gb2.last_area.borrow_mut() = Some(sel.rect);
                on_frame(&gb2, sel.frame, sel.rect);
            }
            None => report_wait_result(&gb2, None, 0, 0),
        }
        drop(hold);
    });
}

fn capture_area(gb: &Rc<Omacapture>, inline_annotate: bool) {
    let gb = gb.clone();
    with_delay(&gb.clone(), move || {
        start_pick(&gb, overlay::PickMode::Region, move |gb, frame, _| {
            if inline_annotate {
                crate::annotate::open(gb, frame, None);
            } else {
                crate::postcapture::handle(gb, frame, CaptureMode::Area);
            }
        });
    });
}

fn capture_window(gb: &Rc<Omacapture>) {
    let gb = gb.clone();
    with_delay(&gb.clone(), move || {
        start_pick(&gb, overlay::PickMode::Window, |gb, frame, _| {
            crate::postcapture::handle(gb, frame, CaptureMode::Window);
        });
    });
}

fn capture_ocr(gb: &Rc<Omacapture>) {
    let gb = gb.clone();
    start_pick(&gb.clone(), overlay::PickMode::Region, move |gb, frame, _| {
        let cfg = gb.config.get();
        let app = gb.app.clone();
        let png = match crate::export::encode_png(&frame.image) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("{e}");
                return;
            }
        };
        let hold = RefCell::new(Some(app.hold()));
        let (tx, rx) = std::sync::mpsc::channel();
        let langs = cfg.ocr.languages.clone();
        std::thread::spawn(move || {
            let _ = tx.send(crate::ocr::recognize(&png, &langs));
        });
        glib::timeout_add_local(std::time::Duration::from_millis(50), move || match rx.try_recv() {
            Ok(Ok(text)) => {
                let preview: String = text.chars().take(160).collect();
                if cfg.ocr.copy_to_clipboard {
                    let _ = crate::clipboard::copy_text(&text);
                }
                let title = if text.is_empty() { "No text found" } else { "Text copied to clipboard" };
                crate::notify::send(app.upcast_ref::<gtk::Application>(), "ocr", title, &preview, None);
                hold.borrow_mut().take();
                glib::ControlFlow::Break
            }
            Ok(Err(e)) => {
                crate::notify::send(app.upcast_ref::<gtk::Application>(), "ocr", "OCR failed", &e.to_string(), None);
                hold.borrow_mut().take();
                glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(_) => glib::ControlFlow::Break,
        });
    });
}
