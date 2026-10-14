//! orca — a Windows launcher.
//!
//! This binary is the composition root. It owns the wiring and nothing else:
//! every decision about *what* a result is, *how* it is ranked, and *how* the
//! platform is talked to already lives in `orca-core` and `orca-win`. What is
//! left here is the part neither of them can hold — an application, a window,
//! and the seams between them.
//!
//! # The shape of startup
//!
//! ```text
//!   single instance ──► secondary: notify primary, exit 2
//!          │
//!          ▼ primary
//!   config ──► providers ──► Catalog ──► Engine ──┐
//!   hotkey ──► Win32GlobalHotkey ─────────────────┤
//!   tray   ──► TrayIcon ─────────────────────────┤
//!                                                 ▼
//!                                        gpui app, QuitMode::Explicit
//!                                                 │
//!                                                 ▼
//!                                          Launcher entity (one, resident)
//!                                                 │
//!                            ┌────────────────────┴────────────────────┐
//!                            ▼                                         ▼
//!                   retained popup window                  BackgroundExecutor
//!                   (hidden, not destroyed)                 collect + rank
//! ```
//!
//! # The two decisions worth explaining
//!
//! **The popup is retained, not recreated.** Creating and destroying a window
//! per toggle measured 260–820 ms hotkey-to-first-paint, which is most of a
//! second. The window is now created once and hidden with `ShowWindow(SW_HIDE)`;
//! `docs/ARCHITECTURE.md`'s "Adding GPUI later" section recorded this as
//! blocked, and the blocking reason turned out to be one missing public
//! function. `src/win.rs` has the full account.
//!
//! **The process outlives its window.** `QuitMode::Explicit` is mandatory, not
//! stylistic: the default resolves to `LastWindowClosed` on Windows, so
//! <kbd>Esc</kbd> would quit the process. It failed twice during the probe's
//! development. What keeps the process meaningful with zero windows open is the
//! hotkey thread and the tray icon.

mod backdrop;
mod catalog;
mod launch;
mod providers;
mod text;
mod theme;
mod ui;
mod win;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_channel::Sender;
use gpui::prelude::*;
use gpui::*;

use orca_core::config::{Config, ConfigPaths, Files};
use orca_core::providers::{
    CalculatorProvider, CommandProvider, EnvVarProvider, ProviderSet, QueryProviders,
};
use orca_core::store::sqlite::SqliteUsageStore;
use orca_core::store::UsageStore;
use orca_win::{
    GlobalHotkey, Hotkey, InstanceCommand, TrayIcon, TrayMenuItem, TraySpec, Win32GlobalHotkey,
    Win32SingleInstance, DEFAULT_HANDSHAKE_TIMEOUT, SECONDARY_INSTANCE_EXIT_CODE,
};

use crate::catalog::{Catalog, Engine};
use crate::providers::StdFsDirectory;
use crate::ui::{
    HideAction, Launcher, LauncherView, Telemetry, WINDOW_COMPACT_HEIGHT, WINDOW_HEIGHT,
    WINDOW_WIDTH,
};

/// Tray command: show the popup.
const TRAY_SHOW: &str = "show";
/// Tray command: quit the process.
const TRAY_QUIT: &str = "quit";

/// A request from a background thread to the UI thread.
///
/// Hotkey and tray events arrive on their own threads, so they cannot touch
/// GPUI state directly. They become a value and a message, and the app's own
/// `cx.spawn` loop turns it back into a state change on the right thread. The
/// same channel carries the second-instance `Show`, so there is exactly one
/// path into the UI and no way for two of them to disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiCommand {
    /// The hotkey was pressed, or the tray icon was clicked.
    Toggle,
    /// A later launch asked the primary to show itself.
    Show,
    /// The tray's Quit entry was chosen.
    Quit,
    /// A popup hide completed; refresh its desktop sample while it is hidden.
    RefreshBackdrop,
}

enum BackdropTarget {
    PredictedWindow(Bounds<Pixels>, f32),
    HiddenWindow((i32, i32, u32, u32)),
}

/// Writes a line to stdout *and* flushes it.
///
/// Plain `println!` is block-buffered when stdout is redirected to a file, so
/// without this the diagnostic lines never reach a log while the process is
/// still alive — which reads as "the code never ran" when it did.
fn log(message: &str) {
    use std::io::Write;
    println!("[orca] {message}");
    let _ = std::io::stdout().flush();
}

/// Logs a failure without pretending it did not happen.
///
/// Every fallible startup step goes through here. The rule from
/// `docs/ARCHITECTURE.md` is that a failed operation never leaves the app in a
/// state that reads as success, so each of these reports and continues with a
/// degraded capability rather than exiting or, worse, proceeding quietly.
fn degraded(what: &str, error: impl std::fmt::Display) {
    log(&format!("{what} unavailable: {error}"));
}

fn main() {
    // ---------------------------------------------------------------- primary
    // Before anything else: two launchers both binding the hotkey and both
    // showing a tray icon is worse than the second one exiting.
    let mut instance = Win32SingleInstance::new();
    match instance.acquire() {
        Ok(true) => {}
        Ok(false) => {
            // A later launch tells the primary to come forward, then leaves.
            // The retry loop inside `notify_primary` covers the window where the
            // primary has won the mutex but not yet started listening.
            if let Err(error) =
                instance.notify_primary(InstanceCommand::Show, DEFAULT_HANDSHAKE_TIMEOUT)
            {
                log(&format!("could not reach the running launcher: {error}"));
            }
            std::process::exit(SECONDARY_INSTANCE_EXIT_CODE);
        }
        Err(error) => {
            // Not being able to *decide* is not a reason to refuse to start. A
            // second launcher is annoying; no launcher is worse.
            degraded("single-instance check", error);
        }
    }

    // ----------------------------------------------------------------- config
    let paths = ConfigPaths::default();
    let roots = orca_win::user_file_search_roots();
    let default_files = Files {
        enabled: !roots.is_empty(),
        roots,
        ..Files::default()
    };
    let config = match Config::load_with_file_defaults(&paths, &default_files) {
        Ok(config) => config,
        Err(error) => {
            degraded("config.toml, using defaults", &error);
            Config {
                files: default_files.clone(),
                ..Config::default()
            }
        }
    };
    log(&format!(
        "config: hotkey {}, theme {}, {} result rows",
        config.general.hotkey, config.general.theme, config.general.max_results
    ));
    log(&format!(
        "file search: {} ({} roots, depth {}, cap {})",
        if config.files.enabled {
            "enabled"
        } else {
            "disabled"
        },
        config.files.roots.len(),
        config.files.max_depth,
        config.files.max_results
    ));

    // ---------------------------------------------------------------- history
    let history = open_history(&paths);
    let engine = build_engine(&config, history);

    // ----------------------------------------------------------------- hotkey
    let (commands, receiver) = async_channel::unbounded::<UiCommand>();
    let mut hotkey = Win32GlobalHotkey::new();
    register_hotkey(&mut hotkey, config.general.hotkey.as_str(), &commands);

    // ------------------------------------------------------------------- tray
    install_tray(&commands);

    // A second launch, arriving from anywhere, raises this one.
    let ui_commands = commands.clone();
    if let Err(error) = instance.start_listener(Box::new(move |command| match command {
        InstanceCommand::Show => {
            let _ = commands.send_blocking(UiCommand::Show);
        }
    })) {
        degraded("second-launch listener", error);
    }
    // The mutex must be held for the process's life; releasing it here would
    // let the next launch become a second primary.
    std::mem::forget(instance);

    run_gpui(config, engine, receiver, ui_commands);
}

/// Opens the launch-history store, falling back to an in-memory one.
///
/// A history that cannot be opened is a ranking refinement gone, not a reason
/// to refuse to launch. The in-memory store still ranks correctly for this
/// session; only the persistence across restarts is lost.
fn open_history(paths: &ConfigPaths) -> Arc<Mutex<Box<dyn UsageStore + Send>>> {
    if let Some(parent) = paths.dir().parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            degraded("config directory", error);
        }
    }
    let store: Box<dyn UsageStore + Send> = match SqliteUsageStore::open(paths.database_file()) {
        Ok(store) => Box::new(store),
        Err(error) => {
            degraded("launch history database, using memory only", &error);
            Box::new(orca_core::store::MemoryUsageStore::new())
        }
    };
    Arc::new(Mutex::new(store))
}

/// Builds the provider set and wraps it in the search engine.
///
/// Two sets, because there are two seams. [`ProviderSet`] is query-blind and
/// collected once per process; [`QueryProviders`] answers the query itself and
/// is asked per keystroke. Wiring them separately means a build without a
/// calculator never mentions one.
fn build_engine(config: &Config, history: Arc<Mutex<Box<dyn UsageStore + Send>>>) -> Arc<Engine> {
    // Registration order is de-duplication order: the first provider to offer an
    // id wins. Installed applications come first because their ids are
    // source-prefixed and cannot collide, and because a Start Menu entry is a
    // better answer than a file of the same name.
    let providers = ProviderSet::new()
        .with("installed-apps", providers::InstalledAppProvider)
        .with("commands", CommandProvider::from_config(&config.commands))
        .with(
            "env",
            // The environment is read here, at the composition root, and handed
            // over as a list — which is the whole reason `EnvVarProvider` takes
            // one rather than reading it.
            EnvVarProvider::new(std::env::vars()),
        );

    // Answers, not candidates. Each of these is a function of the query, so
    // there is nothing to collect and nothing to cache.
    let responders = QueryProviders::new().with("calculator", CalculatorProvider);

    let catalog = Catalog::new(providers, config.files.clone());
    log(&format!(
        "providers: {} | answers: {}",
        catalog.provider_names().join(", "),
        responders.names().join(", ")
    ));

    let lister: Arc<dyn orca_core::providers::DirectoryLister + Send + Sync> =
        Arc::new(StdFsDirectory);
    Engine::new(
        catalog,
        lister,
        history,
        config.ranking_policy(),
        config.general.max_results,
        responders,
    )
}

/// Binds the global hotkey.
///
/// Reports a failure and carries on. A launcher that cannot be summoned by
/// keyboard is still usable from the tray, and exiting would leave the user
/// with nothing at all.
fn register_hotkey(hotkey: &mut Win32GlobalHotkey, spec: &str, commands: &Sender<UiCommand>) {
    let sender = commands.clone();
    hotkey.set_press_handler(Box::new(move || {
        // `send_blocking` rather than `try_send`: the channel is unbounded, so
        // this cannot block, and it does not silently drop a keypress if the
        // reader is momentarily busy.
        let _ = sender.send_blocking(UiCommand::Toggle);
    }));

    match Hotkey::parse(spec) {
        Ok(parsed) => match hotkey.register(&parsed) {
            Ok(()) => log(&format!("hotkey: {spec} registered")),
            // `register` does not mark itself bound on failure, so this
            // registrar is honestly unbound and the app keeps running without a
            // hotkey rather than believing it has one.
            Err(error) => degraded(&format!("hotkey {spec}"), error),
        },
        Err(error) => degraded(&format!("hotkey spec {spec:?}"), error),
    }
}

/// Installs the tray icon and starts draining its events.
fn install_tray(commands: &Sender<UiCommand>) {
    let spec = TraySpec::new(
        "orca",
        vec![
            TrayMenuItem::new("Show orca", TRAY_SHOW),
            TrayMenuItem::separator(),
            TrayMenuItem::new("Quit", TRAY_QUIT),
        ],
    );
    let tray = match TrayIcon::install(spec) {
        Ok(tray) => tray,
        Err(error) => {
            degraded("tray icon", error);
            return;
        }
    };

    // The tray is polled on its own thread. `try_next_event` never blocks, so a
    // 50 ms sleep is a poll interval and not a busy loop, and the thread exits
    // when the icon goes away.
    let sender = commands.clone();
    std::thread::Builder::new()
        .name("orca-tray-events".into())
        .spawn(move || {
            while tray.is_live() {
                for event in std::iter::from_fn(|| tray.try_next_event()) {
                    let command = match &event {
                        orca_win::TrayEvent::LeftClick | orca_win::TrayEvent::DoubleClick => {
                            Some(UiCommand::Show)
                        }
                        orca_win::TrayEvent::MenuCommand(command) if command == TRAY_SHOW => {
                            Some(UiCommand::Show)
                        }
                        orca_win::TrayEvent::MenuCommand(command) if command == TRAY_QUIT => {
                            Some(UiCommand::Quit)
                        }
                        _ => None,
                    };
                    if let Some(command) = command {
                        if sender.send_blocking(command).is_err() {
                            return;
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        })
        .map(|_| ())
        .unwrap_or_else(|error| degraded("tray event thread", error));
}

/// Starts the GPUI application and pumps [`UiCommand`]s into it.
fn run_gpui(
    config: Config,
    engine: Arc<Engine>,
    receiver: async_channel::Receiver<UiCommand>,
    commands: Sender<UiCommand>,
) {
    // MANDATORY. `QuitMode::Default` resolves to `LastWindowClosed` on Windows,
    // so closing the popup would tear down the process. This failed twice
    // during the probe's development before it was pinned here.
    let app = gpui_platform::application().with_quit_mode(QuitMode::Explicit);
    app.run(move |cx: &mut App| {
        let telemetry = Arc::new(Telemetry::default());
        let theme_preference = config.general.theme;
        let engine = Arc::clone(&engine);
        let hide_sender = commands.clone();
        let launcher = cx.new(|cx: &mut Context<Launcher>| {
            let mut launcher = Launcher::new(cx, Arc::clone(&engine), Arc::clone(&telemetry));
            launcher.theme_preference = theme_preference;
            launcher.on_hidden = Some(Arc::new(move || {
                let _ = hide_sender.send_blocking(UiCommand::RefreshBackdrop);
            }));
            launcher
        });

        // The single door into the UI. Hotkey, tray, and second-instance events
        // all arrive here and nowhere else.
        //
        // TWO TASKS, deliberately.
        //
        // The receive loop runs on the BACKGROUND executor. It has to: a loop
        // parked on the foreground executor is never woken once no window is
        // visible, and a launcher is hidden most of the time. With the loop on
        // the foreground, the first hotkey press after hiding queued a `Toggle`
        // that nothing ever read, so the launcher could not be reopened without
        // a restart — the log just went silent after `popup: hidden`, with no
        // error anywhere, which is exactly the symptom reported.
        //
        // The background future is `Send`, so it may not hold any GPUI type
        // (`AsyncApp` is `!Send` and would not compile). It therefore sees only
        // a channel, and forwards to a foreground task that owns the app handle.
        // Each hop is a bounded send on an unbounded channel, so the background
        // loop never blocks and never stops draining.
        let (pump_tx, pump_rx) = async_channel::unbounded::<UiCommand>();
        spawn_command_pump(receiver, pump_tx);

        let pump_launcher = launcher.clone();
        let pump_telemetry = Arc::clone(&telemetry);
        cx.spawn(async move |cx| {
            let warm = cx.update(|app| {
                let (display, _) = win::display_under_cursor(app);
                compact_bounds(display, app)
            });
            let scale = win::cursor_physical_and_scale().map_or(1.0, |(_, scale)| scale);
            prewarm_backdrop(
                &pump_launcher,
                BackdropTarget::PredictedWindow(warm, scale),
                cx,
            );

            while let Ok(command) = pump_rx.recv().await {
                match command {
                    UiCommand::Toggle => toggle(&pump_launcher, &pump_telemetry, cx),
                    UiCommand::Show => show(&pump_launcher, &pump_telemetry, cx),
                    UiCommand::RefreshBackdrop => refresh_hidden_backdrop(&pump_launcher, cx),
                    UiCommand::Quit => {
                        cx.update(|cx| cx.quit());
                        return;
                    }
                }
            }
        })
        .detach();

        cx.bind_keys([
            KeyBinding::new("escape", ui::Hide, None),
            KeyBinding::new("up", ui::SelectPrevious, None),
            KeyBinding::new("down", ui::SelectNext, None),
            KeyBinding::new("enter", ui::Activate, None),
            KeyBinding::new("home", ui::CaretHome, None),
            KeyBinding::new("end", ui::CaretEnd, None),
            KeyBinding::new("left", ui::CaretLeft, None),
            KeyBinding::new("right", ui::CaretRight, None),
            // Backspace and Delete are bound as actions rather than left to the
            // text input handler, because on Windows they arrive as control
            // characters and `parse_char_message` filters every control character
            // out before the input handler sees it. See docs/ARCHITECTURE.md.
            KeyBinding::new("backspace", ui::DeleteBack, None),
            KeyBinding::new("delete", ui::DeleteForward, None),
            // Exit without the tray. The tray is the intended way out, but it is
            // currently broken, and a resident launcher with no way to quit is
            // worse than one extra keybinding. Ctrl+Esc cannot collide with
            // plain text input the way a bare Escape would.
            KeyBinding::new("ctrl-esc", ui::Quit, None),
        ]);

        log("ready: resident, waiting for the hotkey");
    });
}

/// Shows the popup, creating the window on first use.
///
/// The closure passed to `open_window` is *stored*, so it must not capture the
/// outer `&mut AsyncApp`; naming the parameter `cx` shadows the outer binding
/// and only the inner `Context` is used. Without that, `*cx` stays borrowed past
/// `cx.update(..)` and the borrow checker rejects it (E0505).
fn show(launcher: &Entity<Launcher>, telemetry: &Arc<Telemetry>, cx: &mut AsyncApp) {
    let existing = cx.update(|app| {
        let focus = launcher.read(app).focus.clone();
        launcher.update(app, |launcher, cx| {
            launcher.reset();
            ui::request_search(launcher, cx);
            launcher.clamp_selection();
        });
        // `WindowHandle` is `Copy`, so the handle is read by value out of the
        // read borrow rather than cloned.
        launcher.read(app).window.map(|window| (window, focus))
    });

    if let Some((window, focus)) = existing {
        // The retained path: the window already exists, so this is a show, not a
        // construction. Repositioned and collapsed first, because a monitor may
        // have been unplugged while the popup was hidden.
        //
        // The capture was prepared while the window was hidden, so showing is
        // reposition, paint, activate and nothing else.
        let shown = cx.update(|app| {
            launcher.update(app, |launcher, _| launcher.set_backdrop_visible(true));
            let outcome = window.update(app, |_, window, cx| {
                place_on_cursor(window, cx);
                // No frame is requested by the visibility callback GPUI sends
                // when the window is shown, so one is asked for explicitly.
                // Without this the popup reappears showing its last frame.
                window.refresh();
                // GPUI focus id only; makes no platform call. First, so the
                // handle is resolved when the real activation lands.
                window.focus(&focus, cx);
                if let Some(hwnd) = win::hwnd(window) {
                    let decoration = win::clear_window_frame(hwnd).summary();
                    // Synchronously, on this thread, and *after* everything
                    // else. `Window::activate_window` cannot be used here: it
                    // spawns its work onto the window's executor, so it returns
                    // before it has done anything, and everything after it
                    // races a task that has not started. That was the cause of
                    // the launcher appearing "not to open" on the third toggle:
                    // the window was shown but the activation had not happened
                    // yet, so it sat behind the foreground window while the log
                    // cheerfully printed `popup: shown`.
                    win::set_shown(hwnd, true);
                    win::raise(hwnd);
                    let foreground = win::activate(hwnd);
                    // Report the measured state, not the intent. A launcher that
                    // is visible but behind another window is indistinguishable
                    // from one that never opened, and that ambiguity is what
                    // made this bug survive two rounds of guessing.
                    log(&format!(
                        "popup: show -> {}; {decoration}",
                        win::describe(hwnd)
                            + if foreground {
                                ""
                            } else {
                                "  [NOT in foreground: it may be behind another window]"
                            }
                    ));
                }
            });
            if outcome.is_ok() {
                launcher.update(app, |launcher, _| {
                    launcher.shown = true;
                    launcher.error = None;
                });
            }
            outcome.is_ok()
        });

        if !shown {
            // The handle outlived its window. Forgetting it is what makes the
            // next toggle rebuild instead of failing silently forever — and
            // saying so is what keeps the log line below trustworthy, because
            // `docs/MANUAL-CHECKS.md` tells a human to read it to decide whether
            // the window is really being retained.
            cx.update(|app| {
                launcher.update(app, |launcher, _| {
                    launcher.set_backdrop_visible(false);
                    launcher.forget_window();
                    launcher.shown = false;
                });
            });
            log("popup: the retained window was gone; a new one is built on the next toggle");
            return;
        }
        telemetry.arm();
        // Report what was *measured*, not what was intended. The previous
        // version of this line was printed unconditionally after a successful
        // `Entity::update`, which says nothing about whether a window reached
        // the screen. That is how a launcher that was opening behind another
        // window still logged `popup: shown` on every press.
        log(if shown {
            "popup: shown (retained window)"
        } else {
            "popup: show failed (retained window)"
        });
        return;
    }

    telemetry.arm();
    // Read once, outside both `update` calls: the focus handle is the same for
    // the life of the entity, and computing it inside the window-building
    // closure would leave it out of scope for the activation that follows.
    let focus = cx.update(|app| launcher.read(app).focus.clone());
    let first_bounds = cx.update(|app| {
        let (display, _) = win::display_under_cursor(app);
        compact_bounds(display, app)
    });
    // Building the window is the only work left on the first open.
    let create = cx.update(|app| {
        launcher.update(app, |launcher, _| launcher.set_backdrop_visible(true));
        let entity = launcher.clone();
        let bounds = first_bounds;

        app.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                kind: WindowKind::PopUp,
                focus: true,
                show: false,
                is_resizable: false,
                is_minimizable: false,
                is_movable: false,
                // The load-bearing line for the whole look. `gpui_windows`
                // clears the render target to `[0, 0, 0, 0]` for every
                // non-opaque appearance and presents through a
                // `DXGI_ALPHA_MODE_PREMULTIPLIED` Direct Composition swap
                // chain, so the rounded panel corners are genuinely
                // see-through instead of black. See `ui`'s module docs.
                window_background: WindowBackgroundAppearance::Transparent,
                ..Default::default()
            },
            // Deliberately named `cx`: see the doc comment above.
            move |_, cx| cx.new(|_cx| LauncherView::new(entity)),
        )
    });

    match create {
        Ok(window) => cx.update(|app| {
            // `WindowHandle::update` is fallible — the window may already be
            // gone — so the handle is captured inside the closure and the
            // `Result` flattened here. A `None` HWND is not fatal; `hide`
            // falls back to destroying the window, and says so in the log.
            let hwnd = window
                .update(app, |_, window, cx| {
                    let handle = win::hwnd(window);
                    window.focus(&focus, cx);
                    // Two Win32 touches on the window's own decoration, both of
                    // them undoing something DWM does to a frameless window.
                    // Do both while the HWND is hidden: GPUI applies the
                    // transparent appearance during open_window, so showing it
                    // first exposes the native frame for at least one frame.
                    let unrounded = handle.is_some_and(win::round_corners);
                    let decoration = match handle {
                        Some(hwnd) => win::clear_window_frame(hwnd).summary(),
                        None => "no HWND, window decoration untouched".to_owned(),
                    };
                    log(&format!(
                        "popup: panel rounds itself; DWM corner rounding \
                         suppressed={unrounded}; {decoration}"
                    ));
                    window.activate_window();
                    handle
                })
                .unwrap_or(None);
            launcher.update(app, |launcher, _| {
                launcher.window = Some(window);
                launcher.hwnd = hwnd;
                launcher.shown = true;
                launcher.error = None;
            });
            log(if hwnd.is_some() {
                "popup: created (retained for later toggles)"
            } else {
                "popup: created, but the HWND was not reachable; \
                 this launcher will destroy and recreate the window per toggle"
            });
        }),
        Err(error) => {
            log(&format!("popup: open failed: {error:?}"));
            cx.update(|app| {
                launcher.update(app, |launcher, _| {
                    launcher.set_backdrop_visible(false);
                    launcher.shown = false;
                });
            });
        }
    }
}

/// Hides the popup, or falls back to destroying the window.
///
/// The fallback is the interesting case: if the `HWND` was never captured, there
/// is no way to hide, and the only correct behaviour is the probe's
/// create-and-destroy. It costs the latency and it is honest about why.
fn hide(launcher: &Entity<Launcher>, cx: &mut AsyncApp) {
    // The decision comes from `Launcher::hide`, which is also what the
    // <kbd>Esc</kbd> handler calls. Keeping one definition is what stops the two
    // paths drifting into the state this used to get into: the action handler
    // destroyed the retained window while `main` still believed it existed.
    let action = cx.update(|app| launcher.update(app, |launcher, _| launcher.hide()));

    match action {
        HideAction::Retained => log("popup: hidden (window retained)"),
        HideAction::NothingShown => log("popup: hide requested while nothing was shown"),
        HideAction::DestroyWindow => {
            // Taken and destroyed in separate `update` calls so the handle is
            // cleared before the window goes: a `WindowHandle` outliving its
            // window turns the next show into a silent no-op.
            let taken =
                cx.update(|app| launcher.update(app, |launcher, _| launcher.forget_window()));
            if let Some(window) = taken {
                cx.update(|app| {
                    let _ = window.update(app, |_, window, _| window.remove_window());
                });
                log("popup: hidden (window destroyed; no HWND was available)");
            } else {
                log("popup: hidden (window was already gone)");
            }
        }
    }
}

/// Hotkey / tray / second-instance entry point.
fn toggle(launcher: &Entity<Launcher>, telemetry: &Arc<Telemetry>, cx: &mut AsyncApp) {
    // A read, then a dispatch — deliberately not a `launcher.update` whose
    // borrow is held across `show`/`hide`. `open_window` renders the new root
    // synchronously and that render updates `Launcher`, so a write borrow
    // spanning the call becomes "cannot read while it is already being
    // updated". `show` and `hide` set the flag themselves.
    let shown = cx.update(|app| launcher.read(app).shown);
    if shown {
        hide(launcher, cx);
    } else {
        show(launcher, telemetry, cx);
    }
}

/// Moves every incoming command from `source` to `sink`, forever.
///
/// This exists as its own function, and on its own thread, because of one fact
/// about GPUI that is invisible in the type signatures: the *foreground*
/// executor on Windows is driven by window activity, so a task parked on it is
/// not scheduled again while no window is visible. A launcher is hidden most of
/// the time, so a command pump living there is effectively dead between toggles.
///
/// The symptom was silent: the hotkey was still registered and still firing, the
/// command was still queued, but nothing woke to drain it — so the log simply
/// stopped after `popup: hidden`, with no error line to explain it.
///
/// A thread rather than `background_executor().spawn`, because that requires
/// `Send` and any future holding a GPUI type is `!Send`. `recv_blocking` parks
/// without polling, and the channel is unbounded so this can never block a
/// hotkey thread.
fn spawn_command_pump(
    source: async_channel::Receiver<UiCommand>,
    sink: async_channel::Sender<UiCommand>,
) {
    std::thread::Builder::new()
        .name("orca-command-pump".into())
        .spawn(move || {
            while let Ok(command) = source.recv_blocking() {
                if sink.send_blocking(command).is_err() {
                    // The consumer is gone, so the app is shutting down.
                    break;
                }
            }
        })
        .map_err(|error| degraded("command pump thread", error))
        .ok();
}

fn prewarm_backdrop(launcher: &Entity<Launcher>, target: BackdropTarget, cx: &mut AsyncApp) {
    let generation = cx.update(|app| {
        launcher.update(app, |launcher, _| {
            launcher.backdrop_generation = launcher.backdrop_generation.wrapping_add(1);
            launcher.backdrop_generation
        })
    });
    let target_name = match &target {
        BackdropTarget::PredictedWindow(_, _) => "startup region",
        BackdropTarget::HiddenWindow(_) => "hidden window region",
    };
    let visible = cx.update(|app| launcher.read(app).backdrop_visible.clone());
    let capture = cx.background_executor().spawn(async move {
        if visible.load(Ordering::Acquire) {
            return None;
        }
        let backdrop = match target {
            BackdropTarget::PredictedWindow(bounds, scale) => {
                crate::backdrop::capture(bounds, scale)
            }
            BackdropTarget::HiddenWindow(rect) => crate::backdrop::capture_hidden_window(rect),
        };
        if visible.load(Ordering::Acquire) {
            None
        } else {
            Some(backdrop)
        }
    });
    let launcher = launcher.clone();
    cx.spawn(async move |cx| {
        let Some(backdrop) = capture.await else {
            log("backdrop: capture skipped because the popup is visible");
            return;
        };
        let captured = backdrop.image.is_some();
        let took_ms = backdrop.took_ms;
        let installed = cx.update(|app| {
            launcher.update(app, |launcher, cx| {
                if launcher.backdrop_generation != generation {
                    return false;
                }
                launcher.backdrop = backdrop.image;
                cx.notify();
                true
            })
        });
        if installed {
            log(&format!(
                "backdrop: {} in {took_ms:.1} ms from {target_name}",
                if captured {
                    "captured and blurred"
                } else {
                    "unavailable, using the tinted panel fallback"
                },
            ));
        } else {
            log("backdrop: stale capture discarded after a newer request");
        }
    })
    .detach();
}

fn refresh_hidden_backdrop(launcher: &Entity<Launcher>, cx: &mut AsyncApp) {
    let rect = cx.update(|app| {
        launcher.read(app).hwnd.and_then(|hwnd| {
            (!win::is_visible(hwnd))
                .then(|| win::physical_window_rect(hwnd))
                .flatten()
        })
    });
    if let Some(rect) = rect {
        prewarm_backdrop(launcher, BackdropTarget::HiddenWindow(rect), cx);
    } else {
        log("backdrop: hidden window bounds unavailable; keeping the previous image");
    }
}

/// Moves and collapses the popup to its fixed vertical anchor on the cursor's monitor.
///
/// If the `HWND` is unavailable, GPUI still resizes the window but its position
/// stays where the platform put it.
fn place_on_cursor(window: &mut Window, app: &App) {
    let (display, scale_factor) = win::display_under_cursor(app);
    let target = compact_bounds(display, app);
    if let Some(hwnd) = win::hwnd(window) {
        win::move_to(
            hwnd,
            target.origin.x.as_f32(),
            target.origin.y.as_f32(),
            scale_factor,
        );
    }
    window.resize(target.size);
}

fn compact_bounds(display_id: Option<DisplayId>, app: &App) -> Bounds<Pixels> {
    let fallback = size(px(WINDOW_WIDTH), px(WINDOW_COMPACT_HEIGHT));
    let display = display_id
        .and_then(|id| app.find_display(id))
        .or_else(|| app.primary_display());
    let Some(display) = display else {
        return Bounds::centered(display_id, fallback, app);
    };

    let work_area = display.visible_bounds();
    let window_size = fallback.min(&work_area.size);
    let origin = point(
        px(work_area.origin.x.as_f32()
            + (work_area.size.width.as_f32() - window_size.width.as_f32()) / 2.0),
        px(compact_anchor_y(
            work_area.origin.y.as_f32(),
            work_area.size.height.as_f32(),
        )),
    );
    Bounds::new(origin, window_size)
}

fn compact_anchor_y(work_area_top: f32, work_area_height: f32) -> f32 {
    let preferred = work_area_top + work_area_height * 0.25 - WINDOW_COMPACT_HEIGHT / 2.0;
    let maximum = (work_area_top + work_area_height - WINDOW_HEIGHT).max(work_area_top);
    preferred.clamp(work_area_top, maximum)
}

#[cfg(test)]
mod tests {
    // Explicit imports rather than `use super::*`, and the reason is worth
    // recording because it cost a confusing debugging cycle.
    //
    // A binary crate root glob-imports both `gpui::*` and `gpui::prelude::*` —
    // together well over a thousand names. A test module that then does
    // `use super::*` inherits both, and expanding the built-in `#[test]`
    // attribute under that many glob candidates exhausts the macro expansion
    // recursion limit. The reported error is `recursion limit reached while
    // expanding #[test]`, pointing at the *first* test in the module, which
    // sends you looking at the tests rather than at the imports.
    //
    // Naming what the tests actually use is better hygiene anyway: it makes the
    // dependency on `orca-win`'s hotkey types visible at the top of the module
    // rather than inherited by accident.
    use orca_core::config::HotkeySpec;
    use orca_core::store::MemoryUsageStore;
    use orca_core::Source;
    use orca_win::{Hotkey, HotkeyParseError, Key, Modifiers};
    use std::sync::{Arc, Mutex};

    use super::{
        build_engine, compact_anchor_y, spawn_command_pump, UiCommand, WINDOW_COMPACT_HEIGHT,
        WINDOW_HEIGHT, WINDOW_WIDTH,
    };
    use crate::ui::{POPUP_HEIGHT, POPUP_WIDTH};

    #[test]
    fn the_command_pump_keeps_draining_across_a_long_idle_period() {
        // Regression test for the bug where the launcher could not be reopened
        // after being hidden. The pump used to live on the foreground executor,
        // which Windows stops scheduling when no window is visible. A human
        // cannot see this from a screenshot: the hotkey still fired, the command
        // was still queued, and the log just stopped.
        //
        // What is verified here is the property that actually matters: after a
        // long idle stretch — longer than any real gap between toggles — a
        // command sent later still gets through. That is exactly the sequence
        // that used to fail: show, hide, wait, hotkey.
        let (from_producer, source) = async_channel::unbounded::<UiCommand>();
        let (to_consumer, sink) = async_channel::unbounded::<UiCommand>();
        spawn_command_pump(source, to_consumer);

        from_producer.send_blocking(UiCommand::Toggle).unwrap();
        assert!(matches!(sink.recv_blocking().unwrap(), UiCommand::Toggle));

        // The popup is now hidden and nothing is happening. The pump must not
        // have gone away during this.
        std::thread::sleep(std::time::Duration::from_millis(250));

        from_producer.send_blocking(UiCommand::Toggle).unwrap();
        from_producer.send_blocking(UiCommand::Show).unwrap();
        assert!(matches!(sink.recv_blocking().unwrap(), UiCommand::Toggle));
        assert!(matches!(sink.recv_blocking().unwrap(), UiCommand::Show));
    }

    #[test]
    fn the_command_pump_exits_when_its_consumer_is_gone() {
        // Quit drops the foreground task, so the sink closes. The pump must
        // notice and stop rather than spinning on a dead channel.
        let (from_producer, source) = async_channel::unbounded::<UiCommand>();
        let (to_consumer, _sink) = async_channel::unbounded::<UiCommand>();
        spawn_command_pump(source, to_consumer);
        // `_sink` is still alive here, so drop it to close the channel the pump
        // is writing to.
        drop(_sink);

        // With no consumer, sends fail. The pump should already have exited or
        // exit on this; either way nothing panics and the thread is reclaimed.
        let _ = from_producer.send_blocking(UiCommand::Quit);
        let _ = &from_producer;
    }

    #[test]
    fn the_config_default_hotkey_parses_the_normal_way() {
        let spec = HotkeySpec::DEFAULT_SPEC;
        let hotkey = Hotkey::parse(spec).expect("the shipped default must bind");
        assert_eq!(hotkey.key, Key::Space);
        assert!(hotkey.modifiers.contains(Modifiers::CTRL));
        assert!(hotkey.modifiers.contains(Modifiers::SHIFT));
    }

    /// The one link no other test covers: that the composition root actually
    /// registers the query-shaped seam. Every unit test builds its own
    /// `Engine`, so a deleted `.with("calculator", …)` line in `build_engine`
    /// would pass all of them and the feature would silently stop answering.
    ///
    /// This goes through the real `build_engine`, which means it enumerates the
    /// real Start Menu and reads the real environment. That is the same class of
    /// test `crates/orca/src/providers.rs` already runs against the real
    /// filesystem, and the cost is a second or two of collection.
    #[test]
    fn the_engine_the_composition_root_builds_answers_an_expression() {
        let engine = build_engine(
            &orca_core::config::Config::default(),
            Arc::new(Mutex::new(
                Box::new(MemoryUsageStore::new()) as Box<dyn orca_core::store::UsageStore + Send>
            )),
        );

        let outcome = engine.search("2*(3+4)", 1);
        assert_eq!(outcome.error, None);
        let answer = outcome
            .rows
            .iter()
            .find(|row| row.item.source == Source::Calculator)
            .unwrap_or_else(|| {
                panic!(
                    "no calculator row: build_engine does not register the seam. rows: {:?}",
                    outcome
                        .rows
                        .iter()
                        .map(|r| r.item.title.clone())
                        .collect::<Vec<_>>()
                )
            });
        assert_eq!(answer.item.title, "14");
    }

    #[test]
    fn the_default_spec_is_accepted_by_the_single_parser() {
        // Guards the seam between the two crates: the default the config layer
        // ships must be one the platform parser accepts, or the launcher starts
        // with no hotkey and only a log line to say so. A human cannot catch
        // this from a screenshot.
        let spec = HotkeySpec::DEFAULT_SPEC;
        let hotkey = Hotkey::parse(spec)
            .unwrap_or_else(|e| panic!("the shipped default {spec:?} must parse: {e:?}"));
        assert!(orca_win::virtual_key(hotkey.key).is_some(), "{spec}");
    }

    #[test]
    fn ctrl_alt_combinations_parse_without_a_workaround() {
        // Regression test. `orca_win::Hotkey::parse` used to reject every
        // `Ctrl+Alt` combination as unbindable, and `orca` carried a second,
        // laxer parser to work around it. The claim was false: the probe bound
        // `Ctrl+Alt+Space` through the real `RegisterHotKey` and drove ten
        // open/hide cycles with it. There is now one parser, and it must accept
        // the combination the launcher actually ships.
        for spec in ["Ctrl+Alt+Space", "Ctrl+Alt+K", "Alt+Ctrl+Space"] {
            let hotkey = Hotkey::parse(spec).unwrap_or_else(|e| panic!("{spec} must parse: {e}"));
            assert!(hotkey.modifiers.contains(Modifiers::CTRL), "{spec}");
            assert!(hotkey.modifiers.contains(Modifiers::ALT), "{spec}");
        }
        let hotkey = Hotkey::parse("Ctrl+Alt+Space").expect("the conventional hotkey must bind");
        assert_eq!(hotkey.key, Key::Space);
    }

    #[test]
    fn a_key_with_no_virtual_key_code_is_still_refused() {
        // The rule that was always correct stays in force: `/` has no
        // layout-independent virtual-key code, so no global binding can be
        // expressed for it. This is a fact about layouts, not about the API.
        assert_eq!(
            Hotkey::parse("Ctrl+/").expect_err("must still be refused"),
            HotkeyParseError::UnsupportedCombination
        );
    }

    #[test]
    fn a_bare_alphanumeric_key_is_refused_so_it_cannot_swallow_typing() {
        // Binding an unmodified letter globally would intercept that key in
        // every other application on the desktop.
        for spec in ["A", "k", "7"] {
            assert_eq!(
                Hotkey::parse(spec).expect_err("a bare key must be refused"),
                HotkeyParseError::UnsupportedCombination,
                "for spec {spec}"
            );
        }
    }

    #[test]
    fn a_genuinely_malformed_spec_is_still_reported_as_malformed() {
        // The types are spelled out because an `UnknownKey(String)` written as
        // `.into()` inside an array literal leaves the target type ambiguous,
        // and an ambiguous `Into` in a const context is a known way to send
        // inference into a recursion.
        let cases: [(&str, HotkeyParseError); 6] = [
            ("Ctrl", HotkeyParseError::MissingKey),
            ("", HotkeyParseError::EmptySegment),
            ("Ctrl+", HotkeyParseError::EmptySegment),
            ("Ctrl+K+L", HotkeyParseError::MultipleKeys),
            (
                "Ctrl+F99",
                HotkeyParseError::UnknownKey(String::from("f99")),
            ),
            (
                "Ctrl+Nonsense",
                HotkeyParseError::UnknownKey(String::from("nonsense")),
            ),
        ];
        for (spec, expected) in cases {
            assert_eq!(
                Hotkey::parse(spec).expect_err("must not parse"),
                expected,
                "for spec {spec:?}"
            );
        }
    }

    #[test]
    fn every_key_token_parses_to_something_bindable() {
        for (spec, key) in [
            ("Ctrl+Space", Key::Space),
            ("Ctrl+Enter", Key::Enter),
            ("Ctrl+Tab", Key::Tab),
            ("Ctrl+Esc", Key::Escape),
            ("Ctrl+Backspace", Key::Backspace),
            ("Ctrl+Del", Key::Delete),
            ("Ctrl+F1", Key::Function(1)),
            ("Ctrl+F24", Key::Function(24)),
            ("Ctrl+K", Key::Char('k')),
            ("Ctrl+4", Key::Char('4')),
        ] {
            let hotkey = Hotkey::parse(spec).expect("should parse");
            assert_eq!(hotkey.key, key, "for spec {spec}");
            assert!(
                orca_win::virtual_key(hotkey.key).is_some(),
                "for spec {spec}"
            );
        }
    }

    #[test]
    fn the_window_matches_the_panel_bounds() {
        assert_eq!(POPUP_WIDTH, 660.0);
        assert_eq!(WINDOW_WIDTH, POPUP_WIDTH);
        assert_eq!(WINDOW_HEIGHT, POPUP_HEIGHT);
        assert_eq!(WINDOW_COMPACT_HEIGHT, 64.0);
    }

    #[test]
    fn the_compact_anchor_is_three_quarters_up_when_the_full_panel_fits() {
        let top = 24.0;
        let height = 720.0;
        let anchor = compact_anchor_y(top, height);

        assert_eq!(anchor, top + height * 0.25 - WINDOW_COMPACT_HEIGHT / 2.0);
        assert!(anchor + WINDOW_HEIGHT <= top + height);
    }

    #[test]
    fn the_compact_anchor_clamps_up_to_fit_the_full_panel() {
        let top = 12.0;
        let height = 512.0;

        assert_eq!(compact_anchor_y(top, height), top + height - WINDOW_HEIGHT);
    }

    /// The tightest display the launcher claims to work on: a 1366×768 laptop
    /// at 150% scaling, which is 910×512 *logical* pixels.
    const SMALLEST_DISPLAY: (f32, f32) = (1366.0 / 1.5, 768.0 / 1.5);

    #[test]
    fn the_window_fits_the_smallest_display_we_support() {
        // Windows silently clamps oversized windows to the work area, which
        // crops the panel rather than failing on exactly the smallest displays.
        let (width, height) = SMALLEST_DISPLAY;
        assert!(
            WINDOW_WIDTH <= width,
            "the window is {WINDOW_WIDTH} logical px wide, more than the {width} \
             available on a 1366-wide display at 150% scaling"
        );
        assert!(
            WINDOW_HEIGHT <= height,
            "the window is {WINDOW_HEIGHT} logical px tall, more than the {height} \
             available on a 768-tall display at 150% scaling"
        );
        assert!(
            WINDOW_COMPACT_HEIGHT <= height,
            "the compact window is {WINDOW_COMPACT_HEIGHT} logical px tall, more than the \
             {height} available on a 768-tall display at 150% scaling"
        );
    }
}
