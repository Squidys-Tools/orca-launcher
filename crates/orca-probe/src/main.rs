use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_channel::Sender;
use gpui::prelude::*;
use gpui::*;

actions!(orca, [Hide, Backspace, Quit]);

fn bg() -> Rgba {
    rgb(0x16161a)
}
fn line_c() -> Rgba {
    rgb(0x2a2a30)
}
fn text_c() -> Rgba {
    rgb(0xf2f2f2)
}
fn dim() -> Rgba {
    rgb(0x6a6a75)
}
fn accent() -> Rgba {
    rgb(0x2b4c6f)
}

// ------------------------------------------------------------------ hotkey

fn spawn_hotkey_thread(tx: Sender<Instant>) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, VK_SPACE,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

    std::thread::spawn(move || unsafe {
        let mods = HOT_KEY_MODIFIERS(MOD_CONTROL.0 | MOD_ALT.0);
        if let Err(e) = RegisterHotKey(None, 0xBEEF, mods, VK_SPACE.0 as u32) {
            eprintln!("hotkey: RegisterHotKey failed: {e:?}");
            return;
        }
        log("hotkey: ctrl-alt-space registered");

        let mut msg = MSG::default();
        loop {
            if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                break;
            }
            if msg.message == WM_HOTKEY {
                let _ = tx.send_blocking(Instant::now());
            }
        }
    });
}

// ------------------------------------------------------------- text input

fn u16_at(s: &str, u16_idx: usize) -> usize {
    if u16_idx == 0 {
        return 0;
    }
    let mut n = 0;
    for (b, ch) in s.char_indices() {
        if n >= u16_idx {
            return b;
        }
        n += ch.len_utf16();
    }
    s.len()
}

const ITEMS: &[(&str, &str)] = &[
    ("Visual Studio Code", "app"),
    ("Windows Terminal", "app"),
    ("Google Chrome", "app"),
    ("Notepad", "app"),
    ("Calculator", "app"),
    ("Spotify", "app"),
    ("README.md", "file"),
    ("Cargo.toml", "file"),
    ("main.rs", "file"),
    ("design.md", "file"),
];

/// Write a line to stdout AND flush it.
///
/// Plain `println!` is block-buffered when stdout is redirected to a file, so
/// without this the diagnostic lines never reach `run*.log` while the process
/// is still alive -- which reads as "the code never ran" when it did.
fn log(msg: &str) {
    use std::io::Write;
    println!("{msg}");
    let _ = std::io::stdout().flush();
}

/// Hotkey timestamp plus the label the status bar shows. Shared rather than
/// stored on an entity so the first-paint measurement can be recorded from
/// inside `paint` without mutating an entity mid-frame.
#[derive(Default)]
struct Shared {
    pending: Mutex<Option<Instant>>,
    label: Mutex<String>,
}

impl Shared {
    fn arm(&self, at: Instant) {
        *self.pending.lock().unwrap() = Some(at);
    }

    /// Called once per popup, from the first `QueryInput::paint`.
    fn take_measurement(&self) -> Option<String> {
        let at = self.pending.lock().unwrap().take()?;
        let ms = at.elapsed().as_secs_f64() * 1000.0;
        let label = format!("{ms:.1} ms");
        log(&format!("PAINT hotkey->first-paint: {label}"));
        *self.label.lock().unwrap() = label.clone();
        Some(label)
    }

    fn label(&self) -> String {
        self.label.lock().unwrap().clone()
    }
}

struct Launcher {
    focus: FocusHandle,
    query: String,
    caret: usize,
    composing: Option<Range<usize>>,
    window: Option<WindowHandle<LauncherView>>,
    visible: bool,
    last_line: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
}

impl Launcher {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus: cx.focus_handle(),
            query: String::new(),
            caret: 0,
            composing: None,
            window: None,
            visible: false,
            last_line: None,
            last_bounds: None,
        }
    }

    fn results(&self) -> Vec<(&'static str, &'static str)> {
        let q = self.query.to_lowercase();
        if q.is_empty() {
            return ITEMS.to_vec();
        }
        ITEMS
            .iter()
            .filter(|(n, _)| n.to_lowercase().contains(&q))
            .copied()
            .collect()
    }

    fn backspace(&mut self, cx: &mut Context<Self>) {
        if self.caret > 0 {
            let prev = self.query[..self.caret]
                .char_indices()
                .next_back()
                .map(|(b, _)| b)
                .unwrap_or(0);
            self.query.replace_range(prev..self.caret, "");
            self.caret = prev;
            cx.notify();
        }
    }
}

// EntityInputHandler is the path IME composition travels through.
impl EntityInputHandler for Launcher {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let s = u16_at(&self.query, range.start);
        let e = u16_at(&self.query, range.end);
        self.query.get(s..e).map(str::to_string)
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.caret..self.caret,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.composing.clone()
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.composing = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = range
            .map(|r| u16_at(&self.query, r.start)..u16_at(&self.query, r.end))
            .or_else(|| self.composing.clone())
            .unwrap_or(self.caret..self.caret);
        self.query.replace_range(target.clone(), new_text);
        self.caret = target.start + new_text.len();
        self.composing = None;
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_sel: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = range
            .map(|r| u16_at(&self.query, r.start)..u16_at(&self.query, r.end))
            .or_else(|| self.composing.clone())
            .unwrap_or(self.caret..self.caret);
        self.query.replace_range(target.clone(), new_text);
        self.caret = target.start + new_text.len();
        self.composing = if new_text.is_empty() {
            None
        } else {
            Some(target.start..target.start + new_text.len())
        };
        if let Some(s) = new_sel {
            self.caret = u16_at(&self.query, s.start);
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let line = self.last_line.as_ref()?;
        let x = bounds.left() + line.x_for_index(self.caret);
        Some(Bounds::new(
            point(x, bounds.top()),
            size(px(1.), bounds.size.height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        p: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let b = self.last_bounds?;
        let line = self.last_line.as_ref()?;
        let i = line.index_for_x(p.x - b.left())?;
        Some(u16_at(&self.query, i))
    }
}

// ------------------------------------------------------- the input element

struct QueryInput {
    launcher: Entity<Launcher>,
    shared: Arc<Shared>,
}

struct QueryLayout {
    line: Option<ShapedLine>,
    caret: Option<PaintQuad>,
}

impl IntoElement for QueryInput {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for QueryInput {
    type RequestLayoutState = ();
    type PrepaintState = QueryLayout;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _insp: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _insp: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _req: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> QueryLayout {
        let l = self.launcher.read(cx);
        let query = l.query.clone();
        let caret = l.caret;
        let composing = l.composing.clone();
        let empty = query.is_empty();

        let text: SharedString = if empty {
            "search...".into()
        } else {
            query.into()
        };

        let base = TextRun {
            len: text.len(),
            font: window.text_style().font(),
            color: if empty { dim().into() } else { text_c().into() },
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs: Vec<TextRun> = match &composing {
            Some(m) => {
                let mut v = vec![
                    TextRun {
                        len: m.start,
                        ..base.clone()
                    },
                    TextRun {
                        len: m.end - m.start,
                        underline: Some(UnderlineStyle {
                            color: Some(base.color),
                            thickness: px(1.),
                            wavy: false,
                        }),
                        ..base.clone()
                    },
                ];
                if text.len() > m.end {
                    v.push(TextRun {
                        len: text.len() - m.end,
                        ..base
                    });
                }
                v
            }
            None => vec![base],
        };

        let line = window.text_system().shape_line(
            text.clone(),
            window.text_style().font_size.to_pixels(window.rem_size()),
            &runs,
            None,
        );

        let caret_quad = Some(fill(
            Bounds::new(
                point(bounds.left() + line.x_for_index(caret), bounds.top()),
                size(px(2.), bounds.size.height),
            ),
            accent(),
        ));

        QueryLayout {
            line: Some(line),
            caret: caret_quad,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _insp: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _req: &mut (),
        prepaint: &mut QueryLayout,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.launcher.read(cx).focus.clone();
        let entity = self.launcher.clone();
        window.handle_input(&focus, ElementInputHandler::new(bounds, entity), cx);

        // first paint of this popup: report hotkey -> pixels on screen
        self.shared.take_measurement();

        if let Some(line) = prepaint.line.take() {
            self.launcher.update(cx, |st, _| {
                st.last_line = Some(line.clone());
                st.last_bounds = Some(bounds);
            });
            line.paint(
                bounds.origin,
                window.line_height(),
                TextAlign::Left,
                None,
                window,
                cx,
            )
            .ok();
        }

        let focused = self.launcher.read(cx).focus.is_focused(window);
        if focused {
            if let Some(caret) = prepaint.caret.take() {
                window.paint_quad(caret);
            }
        }
    }
}

// ------------------------------------------------------------- window root

/// Show/hide the popup.
///
/// Must NOT be called while `launcher` is being updated. `open_window` renders
/// the new root view synchronously, and that render reads `Launcher`; any write
/// borrow held across the call turns into "cannot read ... while it is already
/// being updated". Hence every `launcher.update` here is a short, completed
/// statement rather than a borrow held across `open_window`.
fn toggle_window(
    cx: &mut AsyncApp,
    launcher: &Entity<Launcher>,
    at: Instant,
    shared: &Arc<Shared>,
) {
    shared.arm(at);

    // `move` is required on the outer closure: the inner `move` closure below is
    // handed to `open_window`, which stores it. Without `move` the outer closure
    // borrows `cx` for as long as that stored closure lives, and `update` cannot
    // then take `&mut AsyncApp` by value (E0505).
    cx.update(move |app| {
        if launcher.read(app).visible {
            let mut taken = None;
            launcher.update(app, |l, _| {
                l.visible = false;
                l.query.clear();
                l.caret = 0;
                taken = l.window.take();
            });
            if let Some(w) = taken {
                let _ = w.update(app, |_, win, _| win.remove_window());
            }
            log("popup: hidden");
            return;
        }

        launcher.update(app, |l, _| {
            l.visible = true;
            l.query.clear();
            l.caret = 0;
        });

        let me = launcher.clone();
        let shared = shared.clone();
        let focus = launcher.read(app).focus.clone();
        let bounds = Bounds::centered(None, size(px(680.), px(420.)), app);

        match app.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                kind: WindowKind::PopUp,
                focus: true,
                show: true,
                is_resizable: false,
                ..Default::default()
            },
            // The param is named `cx` to shadow the outer `&mut AsyncApp`: this
            // closure is handed to `open_window`, which stores it, so capturing
            // the outer `cx` would keep `*cx` borrowed past `cx.update(..)` and
            // fail to compile (E0505). Only the inner Context is unused.
            move |_, cx| {
                cx.new(|_cx| LauncherView {
                    launcher: me,
                    shared,
                })
            },
        ) {
            Ok(w) => {
                let _ = w.update(app, move |_, win, cx| {
                    // GPUI focus id only; makes no platform call (window.rs:2301).
                    // Must come first so the handle is resolved when the
                    // foreground activation lands.
                    win.focus(&focus, cx);
                    // The real OS-level activation. `cx.activate(true)` is a
                    // literal no-op on Windows (gpui_windows/src/platform.rs:599);
                    // this path does SetActiveWindow/SetFocus + a SendInput Alt
                    // tap to defeat the foreground lock (gpui_windows/src/window.rs:812).
                    win.activate_window();
                });
                launcher.update(app, |l, _| l.window = Some(w));
                log("popup: shown");
            }
            Err(e) => {
                launcher.update(app, |l, _| l.visible = false);
                eprintln!("popup: open failed: {e:?}");
            }
        }
    });
}

struct LauncherView {
    launcher: Entity<Launcher>,
    shared: Arc<Shared>,
}

impl LauncherView {
    fn on_hide(&mut self, _: &Hide, window: &mut Window, cx: &mut Context<Self>) {
        self.launcher.update(cx, |l, _| {
            l.visible = false;
            l.window = None;
        });
        // close our own window directly: going through the stored handle would
        // re-enter Launcher while it is being updated
        window.remove_window();
    }

    fn on_backspace(&mut self, _: &Backspace, _: &mut Window, cx: &mut Context<Self>) {
        self.launcher.update(cx, |l, cx| l.backspace(cx));
    }
}

impl Render for LauncherView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // NOTE: never `Entity::update` a root entity from `render`. `open_window`
        // renders the new root synchronously, so an update here re-enters the
        // borrow that is already held by whoever called open_window. Anything
        // that changes state must be an action handler or an async effect.
        // `query` is deliberately absent: QueryInput renders it from its own
        // read of the launcher, so binding it here would be dead weight.
        let (results, latency, composing) = {
            let l = self.launcher.read(cx);
            (l.results(), self.shared.label(), l.composing.is_some())
        };
        let entity = self.launcher.clone();
        let focus = self.launcher.read(cx).focus.clone();

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(bg())
            .text_color(text_c())
            .on_action(cx.listener(Self::on_hide))
            .on_action(cx.listener(Self::on_backspace))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .px_4()
                    .h(px(56.))
                    .border_b_1()
                    .border_color(line_c())
                    .child(div().text_color(dim()).child(">")),
            )
            .child(
                div()
                    .flex_1()
                    .h(px(36.))
                    .flex()
                    .items_center()
                    .px_4()
                    // registers the FocusHandle in the dispatch tree for this
                    // frame; without it window.focus() resolves to the root and
                    // handle_input never sees WM_CHAR
                    .track_focus(&focus)
                    .child(QueryInput {
                        launcher: entity,
                        shared: self.shared.clone(),
                    }),
            )
            .child(div().flex().flex_col().flex_1().p_2().children(
                results.into_iter().enumerate().map(|(i, (name, kind))| {
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_3()
                        .px_3()
                        .h(px(40.))
                        .rounded(px(6.))
                        .when(i == 0, |s| s.bg(accent()))
                        .child(
                            div()
                                .w(px(36.))
                                .text_size(px(11.))
                                .text_color(dim())
                                .child(SharedString::from(kind)),
                        )
                        .child(div().text_size(px(15.)).child(name))
                }),
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap_4()
                    .px_4()
                    .h(px(34.))
                    .border_t_1()
                    .border_color(line_c())
                    .text_size(px(11.))
                    .text_color(dim())
                    .child(SharedString::from(format!("hotkey->paint {latency}")))
                    .child(SharedString::from(if composing {
                        "IME composing"
                    } else {
                        "esc hide"
                    }))
                    .child("ctrl-alt-space toggle"),
            )
    }
}

// -------------------------------------------------------------------- main

fn main() {
    let (tx, rx) = async_channel::unbounded::<Instant>();
    spawn_hotkey_thread(tx);

    // QuitMode::Default resolves to LastWindowClosed on Windows, so removing the
    // last window (Esc hide) tears down the whole process -- fatal for a
    // resident tray launcher. Explicit keeps the message loop alive with zero
    // windows open; the hotkey thread is what keeps the process meaningful.
    let app = gpui_platform::application().with_quit_mode(gpui::QuitMode::Explicit);
    app.run(|cx: &mut App| {
        let shared = Arc::new(Shared::default());
        let launcher = cx.new(Launcher::new);

        cx.spawn(async move |cx| {
            while let Ok(at) = rx.recv().await {
                toggle_window(cx, &launcher, at, &shared);
            }
        })
        .detach();

        cx.bind_keys([
            KeyBinding::new("escape", Hide, None),
            KeyBinding::new("backspace", Backspace, None),
            KeyBinding::new("ctrl-q", Quit, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        log("app: resident, waiting for ctrl-alt-space");
    });
}
