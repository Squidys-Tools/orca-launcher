#[path = "../../orca/src/catalog.rs"]
pub mod catalog;
#[path = "../../orca/src/text.rs"]
pub mod text;
#[path = "../../orca/src/theme.rs"]
pub mod theme;
#[path = "../../orca/src/ui.rs"]
pub mod ui;

mod launch {
    use crate::catalog::Engine;
    use orca_core::ResultItem;

    pub fn activate(_: &ResultItem, _: &Engine) -> bool {
        false
    }
}

mod win {
    pub type Hwnd = ();

    pub fn set_shown(_: Hwnd, _: bool) {}
}

use std::sync::{Arc, Mutex};

use gpui::prelude::*;
use gpui::*;
use gpui_platform::application;
use orca_core::config::Files;
use orca_core::providers::files::InMemoryDirectory;
use orca_core::providers::{Alias, CommandProvider, ProviderSet, QueryProviders};
use orca_core::store::{MemoryUsageStore, UsageStore};
use orca_core::RankingPolicy;

use crate::catalog::{Catalog, Engine};
use crate::ui::{Launcher, LauncherView, Telemetry, WINDOW_HEIGHT, WINDOW_WIDTH};

fn main() {
    let engine = preview_engine();

    application().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("escape", ui::Hide, None),
            KeyBinding::new("up", ui::SelectPrevious, None),
            KeyBinding::new("down", ui::SelectNext, None),
            KeyBinding::new("enter", ui::Activate, None),
            KeyBinding::new("home", ui::CaretHome, None),
            KeyBinding::new("end", ui::CaretEnd, None),
            KeyBinding::new("left", ui::CaretLeft, None),
            KeyBinding::new("right", ui::CaretRight, None),
            KeyBinding::new("backspace", ui::DeleteBack, None),
            KeyBinding::new("delete", ui::DeleteForward, None),
        ]);

        let telemetry = Arc::new(Telemetry::default());
        let launcher = cx.new(|cx| Launcher::new(cx, Arc::clone(&engine), telemetry));
        let focus = launcher.read(cx).focus.clone();
        let bounds = Bounds::centered(None, size(px(WINDOW_WIDTH), px(WINDOW_HEIGHT)), cx);
        let root = launcher.clone();
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Orca UI Preview".into()),
                        ..Default::default()
                    }),
                    kind: WindowKind::Normal,
                    window_background: WindowBackgroundAppearance::Opaque,
                    ..Default::default()
                },
                move |_, cx| cx.new(|_| LauncherView::new(root)),
            )
            .expect("open preview window");

        let _ = window.update(cx, |_, window, cx| window.focus(&focus, cx));
        launcher.update(cx, |launcher, cx| {
            launcher.window = Some(window);
            launcher.shown = true;
            ui::request_search(launcher, cx);
        });
    });
}

fn preview_engine() -> Arc<Engine> {
    let provider = CommandProvider::new([
        Alias::new("Calendar", "calendar").with_subtitle("Your schedule and reminders"),
        Alias::new("File Explorer", "explorer.exe").with_subtitle("Browse files and folders"),
        Alias::new("Orca Launcher", "orca.exe").with_subtitle("A fast launcher for Windows"),
        Alias::new("Settings", "ms-settings:").with_subtitle("System preferences"),
        Alias::new("Terminal", "wt.exe").with_subtitle("Command line tools"),
        Alias::new("Visual Studio Code", "code").with_subtitle("Code editor"),
    ]);
    let catalog = Catalog::new(
        ProviderSet::new().with("preview", provider),
        Files::default(),
    );
    let history: Box<dyn UsageStore + Send> = Box::new(MemoryUsageStore::new());

    Engine::new(
        catalog,
        Arc::new(InMemoryDirectory::new()),
        Arc::new(Mutex::new(history)),
        RankingPolicy::default(),
        50,
        QueryProviders::new(),
    )
}
