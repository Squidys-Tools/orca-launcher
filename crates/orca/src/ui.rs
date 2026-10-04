//! The popup: a floating rounded panel over the desktop, a query field, a
//! result list, and a status line.
//!
//! # The frame
//!
//! The window is **larger than the panel** and transparent outside it. That one
//! decision is what makes the launcher look like the thing it is imitating
//! rather than like a dialog, and it is load-bearing in three places:
//!
//! * `WindowOptions::window_background` is [`WindowBackgroundAppearance::Transparent`].
//!   `gpui_windows` clears the render target to `[0, 0, 0, 0]` for every
//!   non-opaque appearance and presents through a `DXGI_ALPHA_MODE_PREMULTIPLIED`
//!   Direct Composition swap chain, so anything the scene does not paint is
//!   genuinely see-through rather than black.
//! * The panel is a rounded rect inset by [`FRAME_MARGIN`], so the corners are
//!   rounded *by the paint* and the margin is where the drop shadow lives. The
//!   alternative — `DWMWA_WINDOW_CORNER_PREFERENCE` — is Windows 11 only, has
//!   no radius worth having, and would round the outside of the shadow. See
//!   `win.rs`.
//! * Because the window is transparent, the panel's fill is the only opaque
//!   thing in it, and every colour inside it is composited against that fill
//!   rather than against the desktop. See [`crate::theme`].
//!
//! The panel is **opaque**, not frosted. It used to capture the desktop behind
//! itself, blur it, and paint it under a translucent fill; that is gone until
//! the feature work is further along, and `docs/ARCHITECTURE.md` records what
//! was removed and what restoring it costs.
//!
//! Everything in this file that looks like GPUI folklore is load-bearing, and
//! each of the four items below cost a debugging cycle to find. They are
//! recorded in `docs/ARCHITECTURE.md`; they are repeated here because the next
//! person to edit this file needs them next to the code, and because deleting
//! one of them fails *silently* rather than loudly:
//!
//! 1. **`QuitMode::Explicit`** is set in `main`, not here. Under the default,
//!    closing the last window quits the process, so <kbd>Esc</kbd> would end
//!    the launcher. It failed twice before it was fixed.
//! 2. **`Window::activate_window()`**, never `cx.activate(true)`. The latter is
//!    a literal no-op on Windows (`gpui_windows/src/platform.rs`, the `activate`
//!    body is a comment). It compiles, it does nothing, and the popup does not
//!    take focus.
//! 3. **`.track_focus(&focus)` on the dispatch node** that owns the query field.
//!    Without it the focus handle is not in the dispatch tree, `window.focus()`
//!    resolves to the window root instead, and typed characters are dropped
//!    with no error anywhere. It must stay on the node that *contains* the
//!    input, which after the restyle is the search field's own row.
//! 4. **`handle_input` is called from `paint`**, not `prepaint`. Verified in
//!    `gpui/src/window.rs`; calling it in `prepaint` does nothing at all.
//!
//! The escaping-closure idiom at the `open_window` call site is also not a
//! style choice: the closure handed to `open_window` is stored, so capturing the
//! outer `&mut AsyncApp` keeps `*cx` borrowed past `cx.update(..)` and fails to
//! compile. The parameter deliberately shadows the outer `cx`.

use std::ops::Range;
use std::sync::{Arc, Mutex};

use gpui::prelude::*;
use gpui::*;

use orca_core::config::ThemePreference;
use orca_core::RankedItem;
use orca_core::ResultItem;

use crate::catalog::{Engine, Generation};
use crate::text;
use crate::theme::{source_name, Theme};

actions!(
    orca,
    [
        /// Hide the popup without exiting the process.
        Hide,
        /// Move the selection up one row.
        SelectPrevious,
        /// Move the selection down one row.
        SelectNext,
        /// Activate the selected row.
        Activate,
        /// Move the caret to the start of the query.
        CaretHome,
        /// Move the caret to the end of the query.
        CaretEnd,
        /// Move the caret one character left.
        CaretLeft,
        /// Move the caret one character right.
        CaretRight,
        /// Delete the character before the caret.
        DeleteBack,
        /// Delete the character after the caret.
        DeleteForward,
        /// Exit the process, hiding the popup first.
        ///
        /// The tray menu is the intended way to quit, but it is currently broken
        /// (`LoadImageW` on the stock app icon fails), and a resident launcher
        /// with no way to exit is worse than one extra keybinding. Bound to
        /// <kbd>Ctrl</kbd>+<kbd>Esc</kbd> so it cannot collide with typing.
        Quit,
    ]
);

/// The panel's width, in logical pixels, excluding the shadow margin.
pub const POPUP_WIDTH: f32 = 660.0;
/// The panel's height, in logical pixels, excluding the shadow margin.
///
/// 430 is not a free choice. Windows clamps a window that is larger than the
/// display, and the tightest display still in use is a 1366×768 laptop at 150%
/// scaling, which is 512 logical pixels tall. Panel + margin has to fit inside
/// that with room to spare, which is what caps this number; the row height and
/// the two fixed bars below spend what is left.
pub const POPUP_HEIGHT: f32 = 430.0;

/// Transparent margin on every side of the panel, and therefore the difference
/// between the panel and the window.
///
/// Not decoration. The widest shadow in [`crate::theme::Theme::panel_shadows`]
/// is a 56px blur offset 24px down, so it needs roughly 40px of room before it
/// reaches the window edge; a window sized to the panel would clip its own
/// shadow into a hard rectangle, which is the exact artefact the margin exists
/// to avoid. The window is [`WINDOW_WIDTH`] × [`WINDOW_HEIGHT`].
pub const FRAME_MARGIN: f32 = 32.0;

/// The window's width: the panel plus [`FRAME_MARGIN`] on both sides.
pub const WINDOW_WIDTH: f32 = POPUP_WIDTH + FRAME_MARGIN * 2.0;
/// The window's height: the panel plus [`FRAME_MARGIN`] top and bottom.
pub const WINDOW_HEIGHT: f32 = POPUP_HEIGHT + FRAME_MARGIN * 2.0;

/// The panel's corner radius.
///
/// Painted rather than delegated to DWM; see the module docs. Large enough to
/// read as a floating card and small enough that the footer pills inside it do
/// not look like they are competing with it.
const PANEL_RADIUS: f32 = 16.0;

/// One row of the result list.
const ROW_HEIGHT: f32 = 38.0;
/// The search field's height. Taller than a row so the query reads as the
/// panel's subject rather than as another list item.
const SEARCH_HEIGHT: f32 = 60.0;
/// The status bar's height.
const STATUS_HEIGHT: f32 = 44.0;
/// Height of a footer pill, and of the keycap inside it.
const PILL_HEIGHT: f32 = 30.0;
const KEYCAP_HEIGHT: f32 = 20.0;

/// The placeholder shown when the query is empty.
const PLACEHOLDER: &str = "Search for apps and commands…";

/// The magnifier drawn at the left of the search field.
///
/// Inline SVG rather than an asset, and the stroke colour is irrelevant:
/// `gpui` rasterises an `Svg` built from `data()` into an **alpha mask** and
/// tints it with the element's text colour (`Svg::paint` →
/// `Window::paint_svg` → `render_alpha_mask`). So the shape is what this
/// constant carries and [`crate::theme::Theme::dim`] carries the colour.
const MAGNIFIER: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#000" stroke-width="2" stroke-linecap="round"><circle cx="11" cy="11" r="7"/><path d="M16.5 16.5 L21 21"/></svg>"##;

/// The keyboard hints in the footer.
///
/// Both name bindings that exist. This is the reason there is no "Actions"
/// pill: there is no actions menu at this GPUI rev, and a pill that advertises
/// a shortcut nothing handles is worse than no pill.
const HINT_OPEN: &str = "Open";
const HINT_HIDE: &str = "Hide";
const KEY_ENTER: &str = "Enter";
const KEY_ESC: &str = "Esc";

/// Diagnostics shared with the background threads, and read once per frame.
///
/// Kept off the entity on purpose: recording the hotkey-to-first-paint latency
/// happens inside `paint`, and mutating an entity from `paint` is exactly the
/// re-entrancy the probe's `toggle_window` had to be written around.
#[derive(Default)]
pub struct Telemetry {
    hotkey_at: Mutex<Option<std::time::Instant>>,
    first_paint_ms: Mutex<Option<f64>>,
}

impl Telemetry {
    /// Records when the hotkey was pressed.
    pub fn arm(&self) {
        *lock(&self.hotkey_at) = Some(std::time::Instant::now());
    }

    /// Records and returns the hotkey-to-first-paint time, once per popup.
    ///
    /// Called from the query bar's `paint`, so it fires when there are pixels
    /// rather than when the window object exists.
    pub fn take_first_paint(&self) -> Option<f64> {
        let at = lock(&self.hotkey_at).take()?;
        let ms = at.elapsed().as_secs_f64() * 1000.0;
        *lock(&self.first_paint_ms) = Some(ms);
        Some(ms)
    }

    /// The last recorded hotkey-to-first-paint time, for the status line.
    pub fn last_first_paint(&self) -> Option<f64> {
        *lock(&self.first_paint_ms)
    }
}

/// Locks, recovering from poisoning rather than panicking.
///
/// These two locks are only ever held across a `Copy` write, so a poison means
/// something *else* panicked while a frame was in flight. Propagating that into
/// the next `paint` would turn one bad frame into a permanently dead popup.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The launcher's state. One for the life of the process.
///
/// The popup window is created once and then hidden, not recreated, so this
/// outlives the window rather than the other way round. That is what makes the
/// second toggle fast, and it is why `query`, `rows` and `caret` are here and
/// not in the root view.
pub struct Launcher {
    /// The query bar's focus handle. Must be `track_focus`ed by the dispatch
    /// node that owns the bar.
    pub focus: FocusHandle,

    /// The query, as UTF-8. `caret` and `composing` are byte offsets into it.
    pub query: String,
    /// Caret position, a UTF-8 byte offset. See [`crate::text`].
    pub caret: usize,
    /// The IME's in-progress range, in UTF-8 byte offsets.
    pub composing: Option<Range<usize>>,
    /// Index into `rows`.
    pub selected: usize,
    /// The last completed search's rows.
    pub rows: Vec<RankedItem>,
    /// How many candidates the last search matched, before truncation.
    pub matches: usize,
    /// Bumped on every query change; stale background results are dropped.
    pub generation: Generation,
    /// A search is in flight.
    pub searching: bool,
    /// The last collection or search error, shown in the status line.
    pub error: Option<String>,

    /// The popup window, once created. Retained across toggles — see
    /// [`crate::win`] for why it is hidden rather than destroyed.
    pub window: Option<WindowHandle<LauncherView>>,
    /// The popup's raw handle, captured once when the window is created so
    /// hide/show does not have to go back through GPUI.
    pub hwnd: Option<windows::Win32::Foundation::HWND>,
    /// Whether the popup is on screen.
    pub shown: bool,

    /// The last shaped query line, for the caret and for click-to-position.
    pub last_line: Option<ShapedLine>,
    /// The bounds `last_line` was shaped for.
    pub last_bounds: Option<Bounds<Pixels>>,

    /// The theme preference from config; the appearance is read per frame.
    pub theme_preference: ThemePreference,
    /// What `theme_preference` resolved to last frame.
    pub theme: Theme,

    /// Shared with the background threads.
    pub engine: Arc<Engine>,
    /// Shared with the frame that measures the popup's latency.
    pub telemetry: Arc<Telemetry>,
}

/// What [`Launcher::hide`] did, so the caller can finish the job.
///
/// The model change and the platform call are inseparable, but the platform call
/// is not always possible from where the decision is made, so the decision is
/// returned rather than completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HideAction {
    /// Hidden with `ShowWindow(SW_HIDE)`. The window is still there.
    Retained,
    /// No `HWND` was ever captured, so the caller must destroy the window.
    DestroyWindow,
    /// The popup was not showing. Nothing to do.
    NothingShown,
}

/// Which hide mechanism applies, as a pure decision.
///
/// Split out from [`Launcher::hide`] because the two callers of that method
/// cannot be reached from a test — they need a `Window` or an `AsyncApp` — and
/// because the rule "a popup that is already hidden is not hidden again" is the
/// one thing worth pinning down. `Launcher::hide` performs it; this decides it.
#[must_use]
pub fn hide_plan(shown: bool, has_hwnd: bool) -> HideAction {
    match (shown, has_hwnd) {
        (false, _) => HideAction::NothingShown,
        (true, true) => HideAction::Retained,
        (true, false) => HideAction::DestroyWindow,
    }
}

impl Launcher {
    /// Builds the launcher around an already-wired engine.
    pub fn new(cx: &mut Context<Self>, engine: Arc<Engine>, telemetry: Arc<Telemetry>) -> Launcher {
        let (theme_preference, resolved) = (ThemePreference::System, Theme::DARK);
        Launcher {
            focus: cx.focus_handle(),
            query: String::new(),
            caret: 0,
            composing: None,
            selected: 0,
            rows: Vec::new(),
            matches: 0,
            generation: Generation::default(),
            searching: false,
            error: None,
            window: None,
            hwnd: None,
            shown: false,
            last_line: None,
            last_bounds: None,
            theme_preference,
            theme: resolved,
            engine,
            telemetry,
        }
    }

    /// Clears the query and returns to the top of the list.
    ///
    /// Called on every show, so reopening the popup never shows the previous
    /// query's results with an empty bar above them.
    pub fn reset(&mut self) {
        self.query.clear();
        self.caret = 0;
        self.composing = None;
        self.selected = 0;
    }

    /// Re-resolves the theme against the window's current appearance.
    ///
    /// Read per frame rather than cached, because the user can change their
    /// Windows theme while the launcher is resident and the next popup should
    /// match. `WindowAppearance` has no `is_dark`, so the match is explicit
    /// about the two vibrant variants rather than defaulting them to light.
    pub fn refresh_theme(&mut self, window: &Window) {
        let system_dark = matches!(
            window.appearance(),
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        );
        self.theme = Theme::resolve(self.theme_preference, system_dark);
    }

    /// The text the query bar draws: the query, or a placeholder.
    pub fn display_text(&self) -> SharedString {
        if self.query.is_empty() {
            PLACEHOLDER.into()
        } else {
            self.query.clone().into()
        }
    }

    /// Clamps the selection into the current rows.
    ///
    /// Called after every result change. A selection index of 3 with two rows
    /// would index out of bounds on Enter and render no highlighted row at all,
    /// which reads as "the keyboard stopped working".
    pub fn clamp_selection(&mut self) {
        if self.rows.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.rows.len() {
            self.selected = self.rows.len() - 1;
        }
    }

    /// Moves the selection by `delta` rows, saturating at both ends.
    ///
    /// Saturating rather than wrapping: wrapping means holding <kbd>Down</kbd>
    /// jumps from the last row to the top, and on a list that is reordered by
    /// typing that reads as the list reshuffling under the cursor.
    pub fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, last as isize) as usize;
    }

    /// Deletes the character before the caret.
    ///
    /// A no-op while the IME holds a non-empty marked range; the rule and its
    /// reason live with [`text::backspace`].
    pub fn backspace(&mut self) {
        text::backspace(&mut self.query, &mut self.caret, self.composing.as_ref());
    }

    /// Deletes the character after the caret.
    pub fn delete_forward(&mut self) {
        text::delete_forward(&mut self.query, &mut self.caret);
    }

    /// The row the user has selected, if there is one.
    pub fn activated(&self) -> Option<&ResultItem> {
        self.rows.get(self.selected).map(|row| &row.item)
    }

    /// Takes the popup off screen, keeping the window alive.
    ///
    /// The single definition of "hide", shared by the <kbd>Esc</kbd> action
    /// handler in this module and by `main`'s hotkey/tray path. They used to be
    /// two implementations and they disagreed: the action handler called
    /// `Window::remove_window`, which *destroys* the window the whole retained-
    /// window design depends on. `main` kept its `Some(WindowHandle)` and its
    /// `Some(HWND)`, so the next toggle hid a window that no longer existed and
    /// the launcher could never be shown again without a restart. Returning the
    /// decision instead of performing it is what lets the caller — which is the
    /// only holder of a `&mut Window` or an `AsyncApp` — carry it out.
    pub fn hide(&mut self) -> HideAction {
        let plan = hide_plan(self.shown, self.hwnd.is_some());
        if plan == HideAction::NothingShown {
            return plan;
        }
        self.shown = false;
        self.reset();
        if let Some(hwnd) = self.hwnd {
            crate::win::set_shown(hwnd, false);
        }
        plan
    }

    /// Forgets the window, after the caller has destroyed it.
    ///
    /// Only reached on the no-`HWND` fallback, where the window really is gone
    /// and a stale handle would make the next show a silent no-op.
    pub fn forget_window(&mut self) -> Option<WindowHandle<LauncherView>> {
        self.hwnd = None;
        self.window.take()
    }

    /// The text the status line shows.
    pub fn status(&self) -> String {
        if let Some(error) = &self.error {
            return error.clone();
        }
        if self.searching {
            return "searching...".to_owned();
        }
        if self.matches == 0 {
            return "no matches".to_owned();
        }
        let shown = self.rows.len();
        if shown < self.matches {
            format!("{shown} of {} matches", self.matches)
        } else {
            format!("{shown} match{}", if shown == 1 { "" } else { "es" })
        }
    }
}

// `EntityInputHandler` is the path IME composition travels through.
//
// Every range in these signatures is UTF-16 code units. Every field it touches
// is a UTF-8 byte offset. That is the whole reason `crate::text` exists, and
// getting it backwards is a panic in `str::replace_range`, not a wrong pixel.
impl EntityInputHandler for Launcher {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let start = text::u16_to_byte(&self.query, range.start);
        let end = text::u16_to_byte(&self.query, range.end);
        self.query.get(start..end).map(str::to_owned)
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        // UTF-16 out, because that is the space this trait speaks. The probe
        // returned the raw byte offset here, which is only right for ASCII.
        let caret = text::byte_to_u16(&self.query, self.caret);
        Some(UTF16Selection {
            range: caret..caret,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.composing.as_ref().map(|range| {
            let start = text::byte_to_u16(&self.query, range.start);
            let end = text::byte_to_u16(&self.query, range.end);
            start..end
        })
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
        // The transition itself is in `crate::text` because that is where it can
        // be tested without a `Window`; see that module's docs.
        let edit = text::apply(
            &self.query,
            self.caret,
            self.composing.as_ref(),
            range,
            new_text,
        );
        self.query = edit.query;
        self.caret = edit.caret;
        self.composing = edit.composing;
        request_search(self, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_sel: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let edit = text::apply_and_mark(
            &self.query,
            self.caret,
            self.composing.as_ref(),
            range,
            new_text,
            new_sel,
        );
        self.query = edit.query;
        self.caret = edit.caret;
        self.composing = edit.composing;
        // Not searched here. The marked text is uncommitted — the user can still
        // backspace it away — and a list that refilters on every intermediate
        // composition step is the flicker that makes an IME feel broken.
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let line = self.last_line.as_ref()?;
        // The range is UTF-16; `x_for_index` is UTF-8 bytes. With no selection
        // in this model the two agree, but converting is the only correct thing
        // to do and costs nothing.
        let byte = text::u16_to_byte(&self.query, range.start);
        let x = bounds.left() + line.x_for_index(byte);
        Some(Bounds::new(
            point(x, bounds.top()),
            size(px(1.), bounds.size.height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let bounds = self.last_bounds?;
        let line = self.last_line.as_ref()?;
        // `index_for_x` answers in UTF-8 bytes and this trait wants UTF-16, so
        // the conversion has to go the other way from every other method here.
        let byte = line.index_for_x(position.x - bounds.left())?;
        Some(text::byte_to_u16(&self.query, byte))
    }
}

// ---------------------------------------------------------------- the input

/// The query bar.
///
/// A hand-rolled `Element` rather than one of GPUI's text inputs, because this
/// is the probe's shape and it is proven: it is the only one of the two that has
/// ever actually received a keystroke on this platform.
struct QueryInput {
    launcher: Entity<Launcher>,
    telemetry: Arc<Telemetry>,
}

/// What `prepaint` computes and `paint` consumes.
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
        let launcher = self.launcher.read(cx);
        let query = launcher.query.clone();
        let caret = launcher.caret;
        let composing = launcher.composing.clone();
        let empty = query.is_empty();

        let text = launcher.display_text();
        let base = TextRun {
            len: text.len(),
            font: window.text_style().font(),
            color: if empty {
                launcher.theme.dim.into()
            } else {
                launcher.theme.text.into()
            },
            background_color: None,
            underline: None,
            strikethrough: None,
        };

        // The composition range splits the line into three runs so the
        // uncommitted text is underlined. `len` is in bytes and the range is in
        // bytes, so they line up.
        let runs: Vec<TextRun> = match composing.filter(|_| !empty) {
            Some(marked) => {
                let mut runs = vec![
                    TextRun {
                        len: marked.start,
                        ..base.clone()
                    },
                    TextRun {
                        len: marked.end - marked.start,
                        underline: Some(UnderlineStyle {
                            color: Some(base.color),
                            thickness: px(1.),
                            wavy: false,
                        }),
                        ..base.clone()
                    },
                ];
                if text.len() > marked.end {
                    runs.push(TextRun {
                        len: text.len() - marked.end,
                        ..base
                    });
                }
                runs
            }
            None => vec![base],
        };

        let line = window.text_system().shape_line(
            text,
            window.text_style().font_size.to_pixels(window.rem_size()),
            &runs,
            None,
        );

        // `x_for_index` takes a UTF-8 byte offset, which is what `caret` is.
        let caret_quad = fill(
            Bounds::new(
                point(bounds.left() + line.x_for_index(caret), bounds.top()),
                size(px(2.), bounds.size.height),
            ),
            launcher.theme.accent,
        );

        QueryLayout {
            line: Some(line),
            caret: Some(caret_quad),
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
        // From `paint`, not `prepaint`. See the module docs.
        window.handle_input(&focus, ElementInputHandler::new(bounds, entity), cx);

        // First frame with pixels on screen: this is the number that says whether
        // the retained-window design fixed the latency, so it is measured here
        // rather than where the window object is created.
        self.telemetry.take_first_paint();

        if let Some(line) = prepaint.line.take() {
            self.launcher.update(cx, |launcher, _| {
                launcher.last_line = Some(line.clone());
                launcher.last_bounds = Some(bounds);
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

// -------------------------------------------------------------- window root

/// The root view. One per popup window.
pub struct LauncherView {
    launcher: Entity<Launcher>,
}

impl LauncherView {
    /// The root for a launcher.
    ///
    /// A constructor rather than a `pub` field: the composition root needs to
    /// build the root inside the `open_window` closure, and nothing outside
    /// this module should be reading or writing the entity it holds.
    #[must_use]
    pub fn new(launcher: Entity<Launcher>) -> LauncherView {
        LauncherView { launcher }
    }
}

/// Which way a caret movement goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaretMove {
    /// To the start of the query.
    Home,
    /// To the end of the query.
    End,
    /// One character left.
    Left,
    /// One character right.
    Right,
}

impl Launcher {
    /// Moves the caret. Saturating at both ends, and always landing on a
    /// character boundary.
    pub fn move_caret(&mut self, to: CaretMove) {
        self.caret = match to {
            CaretMove::Home => 0,
            CaretMove::End => self.query.len(),
            CaretMove::Left => text::prev_boundary(&self.query, self.caret),
            CaretMove::Right => text::next_boundary(&self.query, self.caret),
        };
    }
}

impl LauncherView {
    /// Carries out whatever [`Launcher::hide`] decided.
    ///
    /// The `&mut Window` needed for the destroy fallback is only here, which is
    /// why `hide` returns an action instead of taking one.
    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.launcher.update(cx, |launcher, _| launcher.hide()) {
            HideAction::DestroyWindow => {
                self.launcher.update(cx, |launcher, _| {
                    launcher.forget_window();
                });
                window.remove_window();
            }
            HideAction::Retained | HideAction::NothingShown => {}
        }
    }

    fn on_hide(&mut self, _: &Hide, window: &mut Window, cx: &mut Context<Self>) {
        // Under `QuitMode::Explicit` the process survives either way; what has
        // to survive is the *window*, so this hides rather than removes. See
        // `Launcher::hide` for what went wrong when it removed.
        self.dismiss(window, cx);
    }

    fn on_quit(&mut self, _: &Quit, window: &mut Window, cx: &mut Context<Self>) {
        // Hide before quitting, so the process does not leave a visible window
        // behind on its way out. Under `QuitMode::Explicit` this is the only
        // thing keeping the window alive, and without it the popup would be
        // destroyed rather than hidden, which is the path that used to break the
        // retained-window design.
        self.dismiss(window, cx);
        cx.quit();
    }

    fn on_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }

    fn on_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    fn on_activate(&mut self, _: &Activate, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self
            .launcher
            .update(cx, |launcher, _| launcher.activated().cloned())
        else {
            return;
        };
        let engine = self.launcher.read(cx).engine.clone();
        crate::launch::activate(&item, &engine);
        // Activation is the one interaction that must not leave the popup on
        // screen covering the thing it just opened. It goes through the same
        // hide as <kbd>Esc</kbd> for the same reason: removing the window here
        // would strand the retained-window state in `main`.
        self.dismiss(window, cx);
    }

    fn on_home(&mut self, _: &CaretHome, _: &mut Window, cx: &mut Context<Self>) {
        self.move_caret(CaretMove::Home, cx);
    }

    fn on_end(&mut self, _: &CaretEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.move_caret(CaretMove::End, cx);
    }

    fn on_left(&mut self, _: &CaretLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.move_caret(CaretMove::Left, cx);
    }

    fn on_right(&mut self, _: &CaretRight, _: &mut Window, cx: &mut Context<Self>) {
        self.move_caret(CaretMove::Right, cx);
    }

    fn on_backspace(&mut self, _: &DeleteBack, _: &mut Window, cx: &mut Context<Self>) {
        self.delete(false, cx);
    }

    fn on_delete(&mut self, _: &DeleteForward, _: &mut Window, cx: &mut Context<Self>) {
        self.delete(true, cx);
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.launcher.update(cx, |launcher, cx| {
            launcher.move_selection(delta);
            cx.notify();
        });
    }

    fn move_caret(&mut self, to: CaretMove, cx: &mut Context<Self>) {
        self.launcher.update(cx, |launcher, cx| {
            launcher.move_caret(to);
            cx.notify();
        });
    }

    fn delete(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.launcher.update(cx, |launcher, cx| {
            if forward {
                launcher.delete_forward();
            } else {
                launcher.backspace();
            }
            launcher.caret = text::clamp_boundary(&launcher.query, launcher.caret);
            request_search(launcher, cx);
        });
    }
}

impl Render for LauncherView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Never `Entity::update` a root entity from `render`. `open_window`
        // renders the new root synchronously, so an update here re-enters the
        // borrow that whoever called `open_window` is holding. Everything that
        // changes state is an action handler or a background effect.
        //
        // `refresh_theme` is the one exception and it is safe: it writes a
        // `Copy` palette, and nothing in the window-creation path is on the
        // stack.
        self.launcher
            .update(cx, |launcher, _| launcher.refresh_theme(window));

        let theme = self.launcher.read(cx).theme;
        let composing = self.launcher.read(cx).composing.is_some();
        let rows = self.launcher.read(cx).rows.clone();
        let selected = self.launcher.read(cx).selected;
        let searching = self.launcher.read(cx).searching;
        let error = self.launcher.read(cx).error.is_some();
        let status = self.launcher.read(cx).status();
        let open = self.launcher.read(cx).activated().is_some();
        let latency = self
            .launcher
            .read(cx)
            .telemetry
            .last_first_paint()
            .map(|ms| format!("paint {ms:.0} ms"))
            .unwrap_or_else(|| "paint --".to_owned());

        let focus = self.launcher.read(cx).focus.clone();
        let entity = self.launcher.clone();
        let telemetry = self.launcher.read(cx).telemetry.clone();

        // The root is deliberately *not* painted. It is the shadow margin, and
        // the window behind it is transparent — see the module docs. Giving it
        // `theme.background` here is the single change that turns the launcher
        // back into a dialog.
        div()
            .flex()
            .flex_col()
            .size_full()
            .p(px(FRAME_MARGIN))
            .text_color(theme.text)
            .on_action(cx.listener(Self::on_hide))
            .on_action(cx.listener(Self::on_quit))
            .on_action(cx.listener(Self::on_previous))
            .on_action(cx.listener(Self::on_next))
            .on_action(cx.listener(Self::on_activate))
            .on_action(cx.listener(Self::on_home))
            .on_action(cx.listener(Self::on_end))
            .on_action(cx.listener(Self::on_left))
            .on_action(cx.listener(Self::on_right))
            .on_action(cx.listener(Self::on_backspace))
            .on_action(cx.listener(Self::on_delete))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .flex_col()
                    .relative()
                    .rounded(px(PANEL_RADIUS))
                    .bg(theme.background)
                    .border_1()
                    .border_color(theme.border)
                    .shadow(theme.panel_shadows())
                    // Clips the rows to the panel's radius. Without it the scrolled
                    // list paints square corners over the rounded ones.
                    .overflow_hidden()
                    .child(self.search_field(theme, &focus, entity, telemetry))
                    .child(self.result_list(&rows, selected, theme, searching, error))
                    .child(Self::status_bar(
                        status, latency, composing, error, open, theme,
                    )),
            )
    }
}

impl LauncherView {
    /// The query field: a magnifier and the text input, on one row.
    ///
    /// This node is also the focus-tracking one. Item 3 of the module docs is
    /// about *this* element specifically — it is the nearest ancestor of the
    /// [`QueryInput`], so if the input is ever re-parented this row has to move
    /// with it or typing stops working with no error anywhere.
    fn search_field(
        &mut self,
        theme: Theme,
        focus: &FocusHandle,
        entity: Entity<Launcher>,
        telemetry: Arc<Telemetry>,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .flex_shrink_0()
            .px_5()
            .h(px(SEARCH_HEIGHT))
            .text_size(px(16.))
            .track_focus(focus)
            .child(
                svg()
                    .data(MAGNIFIER.as_bytes())
                    .size(px(18.))
                    .flex_shrink_0()
                    .text_color(theme.dim),
            )
            .child(QueryInput {
                launcher: entity,
                telemetry,
            })
    }

    /// A footer pill: a label and the key that presses it.
    ///
    /// Takes no `&mut self` — it reads no view state — so it is an associated
    /// function rather than a method. That is not a style preference: `&mut
    /// self` would borrow the view for the whole build of a leaf that cannot
    /// possibly want it, and it is called twice in one expression.
    fn pill(theme: Theme, label: &'static str, key: &'static str) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .h(px(PILL_HEIGHT))
            .px_3()
            .rounded(px(8.))
            .bg(theme.surface)
            .border_1()
            .border_color(theme.border)
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.text)
                    .child(SharedString::from(label)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(KEYCAP_HEIGHT))
                    .px_2()
                    .rounded(px(5.))
                    .bg(theme.selection)
                    .text_size(px(10.))
                    .text_color(theme.dim)
                    .child(SharedString::from(key)),
            )
    }

    /// The status bar: what the search is doing on the left, what the keyboard
    /// does on the right.
    ///
    /// Also reads no view state, and takes its arguments by value because
    /// `render` already built every one of them as an owned `String` for this
    /// one call.
    fn status_bar(
        status: String,
        latency: String,
        composing: bool,
        error: bool,
        open: bool,
        theme: Theme,
    ) -> impl IntoElement {
        let left = if composing {
            "IME composing".to_owned()
        } else {
            status
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap_4()
            .flex_shrink_0()
            .px_4()
            .h(px(STATUS_HEIGHT))
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .text_size(px(11.))
                    .text_color(if error { theme.accent } else { theme.dim })
                    .child(SharedString::from(left))
                    .child(SharedString::from(latency)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    // Dimmed rather than hidden when nothing is selected: the
                    // pill still names the binding, which is its job, and a
                    // layout that reflows as the selection moves would make the
                    // bar twitch on every arrow key.
                    .opacity(if open { 1.0 } else { 0.45 })
                    .child(Self::pill(theme, HINT_OPEN, KEY_ENTER))
                    .child(Self::pill(theme, HINT_HIDE, KEY_ESC)),
            )
    }

    /// The scrollable result rows.
    fn result_list(
        &mut self,
        rows: &[RankedItem],
        selected: usize,
        theme: Theme,
        searching: bool,
        error: bool,
    ) -> impl IntoElement {
        // A stable id, which is also what makes the element *stateful* — and
        // `overflow_y_scroll` lives on `StatefulInteractiveElement`, so the id
        // is required rather than decorative. The useful side effect is that
        // the scroll offset survives a keystroke, so refining a long list does
        // not throw the user back to the top.
        let list = div()
            .id(SharedString::from("orca-results"))
            .flex()
            .flex_col()
            .flex_1()
            .overflow_y_scroll()
            .px_2()
            .py_2();

        if rows.is_empty() {
            let message = if error {
                "collection failed"
            } else if searching {
                "searching..."
            } else {
                "no matches"
            };
            return list.child(
                div()
                    .flex()
                    .px_3()
                    .h(px(ROW_HEIGHT))
                    .items_center()
                    .text_size(px(13.))
                    .text_color(theme.dim)
                    .child(message),
            );
        }

        list.children(rows.iter().enumerate().map(|(index, row)| {
            let is_selected = index == selected;
            // The source name sits on the right rather than in a left gutter.
            // A fixed-width gutter existed to keep titles aligned across rows;
            // a right-aligned column needs no such reservation, which is where
            // the ~44px it was holding went — straight into the title.
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .px_3()
                .h(px(ROW_HEIGHT))
                .rounded(px(9.))
                .when(is_selected, |style| style.bg(theme.selection))
                .child(
                    div()
                        .flex_1()
                        .flex_col()
                        .overflow_hidden()
                        .child(
                            div()
                                .truncate()
                                .text_size(px(15.))
                                .text_color(if is_selected {
                                    theme.on_selection
                                } else {
                                    theme.text
                                })
                                .child(row.item.title.clone()),
                        )
                        // The subtitle appears only on the selected row. Showing
                        // it on every row meant 12 rows of metadata competing
                        // with the one title the user is actually reading, and
                        // the selection was easier to lose than to find.
                        //
                        // `ROW_HEIGHT` is fixed, so the row does not resize when
                        // the subtitle comes and goes and the list does not
                        // shift under the cursor as the selection moves.
                        .when_some(
                            if is_selected {
                                row.item.subtitle.clone()
                            } else {
                                None
                            },
                            |column, subtitle| {
                                column.child(
                                    div()
                                        .truncate()
                                        .text_size(px(11.))
                                        .text_color(theme.dim)
                                        .child(subtitle),
                                )
                            },
                        ),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(12.))
                        .text_color(theme.dim)
                        .child(SharedString::from(source_name(row.item.source))),
                )
        }))
    }
}

/// Starts a background search for the current query.
///
/// The only place results are produced. `render` never ranks, never collects,
/// and never touches a file.
///
/// The generation check on the way back is what makes typing feel right: a
/// search for `"note"` that finishes after the user has typed `"notepad"`
/// arrives with a stale generation and is dropped, so the list never flickers
/// back through results the user has already rejected.
pub fn request_search(launcher: &mut Launcher, cx: &mut Context<Launcher>) {
    let query = launcher.query.clone();
    let generation = launcher.generation.next();
    launcher.searching = true;
    let engine = Arc::clone(&launcher.engine);
    let weak = cx.weak_entity();

    cx.spawn(async move |_window, cx| {
        // Cloned so the borrow of `cx` ends here: the task must be `'static` and
        // `cx` is still needed after the await.
        let background = cx.background_executor().clone();
        let outcome = background
            .spawn(async move { engine.search(&query, generation) })
            .await;
        let _ = weak.update(cx, |launcher, cx| {
            if outcome.generation != launcher.generation.get() {
                return;
            }
            launcher.rows = outcome.rows;
            launcher.matches = outcome.matches;
            launcher.error = outcome.error;
            launcher.searching = false;
            launcher.clamp_selection();
            cx.notify();
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    // Named imports, not `use super::*`: this module glob-imports `gpui::*` and
    // `gpui::prelude::*`, and a test module that inherits both exhausts the
    // macro expansion limit on `#[test]` itself. See `main.rs`.
    use super::{hide_plan, HideAction};

    #[test]
    fn a_showing_popup_with_a_handle_is_hidden_not_destroyed() {
        // The regression: <kbd>Esc</kbd> used to reach `Window::remove_window`,
        // which destroys the window the retained design depends on. The launcher
        // then could not be shown again for the rest of the process.
        assert_eq!(hide_plan(true, true), HideAction::Retained);
    }

    #[test]
    fn a_showing_popup_without_a_handle_falls_back_to_destruction() {
        // No HWND means no way to hide. The cost is the latency the retained
        // window exists to avoid, and it is better than a window that never goes
        // away.
        assert_eq!(hide_plan(true, false), HideAction::DestroyWindow);
    }

    #[test]
    fn a_popup_that_is_already_hidden_is_not_hidden_again() {
        // The reset and the platform call both have to be skipped, or a stray
        // hide would clear a query the user is still typing into.
        for has_hwnd in [true, false] {
            assert_eq!(
                hide_plan(false, has_hwnd),
                HideAction::NothingShown,
                "with a handle: {has_hwnd}"
            );
        }
    }

    #[test]
    fn the_fallback_never_reaches_the_two_callers_unprepared() {
        // Both `main::hide` and the action handler must handle every variant.
        // A new variant that they do not match would be a silent no-op, so the
        // set is asserted rather than assumed.
        let all = [
            hide_plan(true, true),
            hide_plan(true, false),
            hide_plan(false, true),
        ];
        for action in all {
            assert!(
                matches!(
                    action,
                    HideAction::Retained | HideAction::DestroyWindow | HideAction::NothingShown
                ),
                "unhandled variant {action:?}"
            );
        }
    }
}
