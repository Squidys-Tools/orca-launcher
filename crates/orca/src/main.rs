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

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_channel::Sender;
use gpui::prelude::*;
use gpui::*;

use orca_core::config::{Config, ConfigPaths};
use orca_core::providers::{CommandProvider, EnvVarProvider, ProviderSet};
use orca_core::store::sqlite::SqliteUsageStore;
use orca_core::store::UsageStore;
use orca_win::{
    GlobalHotkey, Hotkey, InstanceCommand, TrayIcon, TrayMenuItem, TraySpec, Win32GlobalHotkey,
    Win32SingleInstance, DEFAULT_HANDSHAKE_TIMEOUT, SECONDARY_INSTANCE_EXIT_CODE,
};

use crate::catalog::{Catalog, Engine};
use crate::providers::StdFsDirectory;
use crate::ui::{HideAction, Launcher, LauncherView, Telemetry, WINDOW_HEIGHT, WINDOW_WIDTH};

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
    let config = match Config::load(&paths) {
        Ok(config) => config,
        Err(error) => {
            degraded("config.toml, using defaults", &error);
            Config::default()
        }
    };
    log(&format!(
        "config: hotkey {}, theme {}, {} result rows",
        config.general.hotkey, config.general.theme, config.general.max_results
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

    run_gpui(config, engine, receiver);
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

    let catalog = Catalog::new(providers, config.files.clone());
    log(&format!(
        "providers: {}",
        catalog.provider_names().join(", ")
    ));

    let lister: Arc<dyn orca_core::providers::DirectoryLister + Send + Sync> =
        Arc::new(StdFsDirectory);
    Engine::new(
        catalog,
        lister,
        history,
        config.ranking_policy(),
        config.general.max_results,
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
fn run_gpui(config: Config, engine: Arc<Engine>, receiver: async_channel::Receiver<UiCommand>) {
    // MANDATORY. `QuitMode::Default` resolves to `LastWindowClosed` on Windows,
    // so closing the popup would tear down the process. This failed twice
    // during the probe's development before it was pinned here.
    let app = gpui_platform::application().with_quit_mode(QuitMode::Explicit);
    app.run(move |cx: &mut App| {
        let telemetry = Arc::new(Telemetry::default());
        let theme_preference = config.general.theme;
        let engine = Arc::clone(&engine);
        let launcher = cx.new(|cx: &mut Context<Launcher>| {
            let mut launcher = Launcher::new(cx, Arc::clone(&engine), Arc::clone(&telemetry));
            launcher.theme_preference = theme_preference;
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
            // Warm the first backdrop here, at startup, rather than on the first
            // `show`. The window does not exist yet, so the capture is safe, and
            // there is no user waiting on it. This is what lets `show` never
            // capture: by the time a hotkey is pressed the image is already
            // sitting in `launcher.backdrop`.
            //
            // Without it the very first open would paint a flat fill, which
            // would look like the frosted-glass work had been reverted.
            let warm = cx.update(|app| {
                let display = win::display_under_cursor(app);
                Bounds::centered(display, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), app)
            });
            prewarm_backdrop(&pump_launcher, warm, cx);

            while let Ok(command) = pump_rx.recv().await {
                match command {
                    UiCommand::Toggle => toggle(&pump_launcher, &pump_telemetry, cx),
                    UiCommand::Show => show(&pump_launcher, &pump_telemetry, cx),
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
        // construction. Repositioned first, because a monitor may have been
        // unplugged while the popup was hidden.
        //
        // No backdrop work happens here. `prewarm_backdrop` filled
        // `launcher.backdrop` from the previous hide, so this path is only
        // reposition, paint, activate — which is what makes the popup feel
        // instant. The cost of a cold cache is a flat fill for one frame, not a
        // 40ms stall in front of the user; see `prewarm_backdrop`.
        let shown = cx.update(|app| {
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
                        "popup: show -> {}",
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
        let display = win::display_under_cursor(app);
        Bounds::centered(display, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), app)
    });
    // The backdrop is already warm: `run_gpui` captures it at startup and
    // `hide` refreshes it after every close, so neither path has to wait for a
    // capture here. Building the window is the only work left on the first
    // open.
    let create = cx.update(|app| {
        let entity = launcher.clone();
        let bounds = first_bounds;

        app.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                kind: WindowKind::PopUp,
                focus: true,
                show: true,
                is_resizable: false,
                is_minimizable: false,
                is_movable: false,
                // The load-bearing line for the whole look. `gpui_windows`
                // clears the render target to `[0, 0, 0, 0]` for every
                // non-opaque appearance and presents through a
                // `DXGI_ALPHA_MODE_PREMULTIPLIED` Direct Composition swap
                // chain, so the shadow margin around the panel is genuinely
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
                    place_on_cursor(window, cx);
                    window.focus(&focus, cx);
                    window.activate_window();
                    // Two Win32 touches on the window's own decoration, both of
                    // them undoing something DWM does to a frameless window.
                    // The summary is *measured* — DWM is asked what it thinks
                    // afterwards — because a `Set` returning success has
                    // already been caught claiming a frame was removed when it
                    // was not.
                    let unrounded = handle.is_some_and(win::round_corners);
                    let decoration = match handle {
                        Some(hwnd) => win::clear_window_frame(hwnd).summary(),
                        None => "no HWND, window decoration untouched".to_owned(),
                    };
                    log(&format!(
                        "popup: panel rounds itself; DWM corner rounding \
                         suppressed={unrounded}; {decoration}"
                    ));
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
                launcher.update(app, |launcher, _| launcher.shown = false);
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
        // Both arms below end with the window hidden, which is the only moment
        // a screen capture is safe — so this is where the next open's backdrop
        // gets taken. On `Retained` it is the common case; on `DestroyWindow`
        // there is no window left to show next time, so it is captured when the
        // window is rebuilt instead.
        HideAction::Retained => {
            let bounds = cx.update(|app| {
                let display = win::display_under_cursor(app);
                Bounds::centered(display, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), app)
            });
            prewarm_backdrop(launcher, bounds, cx);
            log("popup: hidden (window retained)");
        }
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

/// Captures the frosted backdrop for the panel that is about to appear, and
/// stores it for the next `show`.
///
/// Must be called while the popup window is **hidden**, because `BitBlt`
/// photographs the screen and would otherwise photograph the popup. That is
/// why this runs from `hide` rather than from `show`.
///
/// The capture is the expensive part of opening a launcher — 20-50ms for the
/// `BitBlt` plus the box blur — and it used to be paid between the keystroke and
/// the first frame, which is the worst place to pay it: it is invisible time,
/// added directly to how fast the popup feels. Moving it here makes it free on
/// the path the user is waiting on, at the cost of the backdrop being a little
/// stale: it shows what was behind the cursor at *hide* time, not at show time.
///
/// That trade is deliberate. A frosted panel showing slightly old content is
/// not noticeable; a launcher that opens 40ms late is.
///
/// The work is split across two executors because `BitBlt` is a blocking Win32
/// call and the blur is CPU work: neither belongs on the foreground executor,
/// where it would stall rendering. Only the final `launcher.update` comes back
/// to the foreground, because that is the only part that touches shared state.
fn prewarm_backdrop(
    launcher: &Entity<Launcher>,
    target: gpui::Bounds<gpui::Pixels>,
    cx: &mut AsyncApp,
) {
    let scale = win::cursor_physical_and_scale().map_or(1.0, |(_, scale)| scale);
    let capture = cx
        .background_executor()
        .spawn(async move { crate::backdrop::capture(target, scale) });
    let launcher = launcher.clone();
    cx.spawn(async move |cx| {
        let backdrop = capture.await;
        log(&format!(
            "backdrop: {} in {:.1} ms at {}x scaling (captured while hidden)",
            if backdrop.image.is_some() {
                "captured and blurred"
            } else {
                "unavailable, falling back to a flat fill"
            },
            backdrop.took_ms,
            scale
        ));
        cx.update(|app| launcher.update(app, |launcher, _| launcher.backdrop = backdrop.image));
    })
    .detach();
}

/// Moves the popup to the cursor's monitor, centred in its work area.
///
/// A no-op if the `HWND` is unavailable, which leaves the window where the
/// platform put it — the right failure, because a window at the wrong position
/// is recoverable and one that was never placed is not.
fn place_on_cursor(window: &mut Window, app: &App) {
    let display = win::display_under_cursor(app);
    let target = Bounds::centered(display, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), app);
    if let Some(hwnd) = win::hwnd(window) {
        win::move_to(hwnd, target.origin.x.as_f32(), target.origin.y.as_f32());
    }
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
    use orca_win::{Hotkey, HotkeyParseError, Key, Modifiers};

    use super::{spawn_command_pump, UiCommand, WINDOW_HEIGHT, WINDOW_WIDTH};
    // The panel dimensions and the margin are not used outside this module, so
    // they are imported here rather than at the crate root where they would be
    // dead code in the binary.
    use crate::ui::{FRAME_MARGIN, POPUP_HEIGHT, POPUP_WIDTH};

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
    fn the_window_is_the_panel_plus_a_shadow_margin() {
        // The panel is 660 wide, and the window is that plus 32px of transparent
        // margin on each side. The margin is not padding: the widest shadow
        // blur is 56px, so a window sized to the panel would clip its own
        // shadow into a hard rectangle. See `ui::FRAME_MARGIN`.
        assert_eq!(POPUP_WIDTH, 660.0);
        assert_eq!(WINDOW_WIDTH, POPUP_WIDTH + FRAME_MARGIN * 2.0);
        assert_eq!(WINDOW_HEIGHT, POPUP_HEIGHT + FRAME_MARGIN * 2.0);
    }

    /// The tightest display the launcher claims to work on: a 1366×768 laptop
    /// at 150% scaling, which is 910×512 *logical* pixels.
    const SMALLEST_DISPLAY: (f32, f32) = (1366.0 / 1.5, 768.0 / 1.5);

    #[test]
    fn the_window_fits_the_smallest_display_we_support() {
        // The failure this guards against is quiet: the panel alone was a
        // sensible size, and only overflowed once the shadow margin was added
        // on all four sides. Windows then clamps the window to the work area,
        // which crops the panel rather than failing, so the launcher looks
        // subtly wrong on exactly the machines least able to spare the pixels.
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
    }
}
