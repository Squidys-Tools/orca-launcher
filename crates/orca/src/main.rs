//! orca — a Windows launcher.
//!
//! This binary is the composition root: it wires `orca-win` (platform) to
//! `orca-core` (domain) and, once the UI lands, to GPUI.
//!
//! It does nothing real yet. Startup prints what the workspace is made of and
//! exercises both library crates end to end so that a green build actually
//! means the crate boundaries hold.

use orca_core::{rank, LaunchTarget, ResultItem, Source};
use orca_win::{GlobalHotkey, Hotkey, Win32GlobalHotkey};

/// Hotkey the launcher will bind once the Win32 implementation exists.
const DEFAULT_HOTKEY: &str = "Ctrl+Shift+Space";

fn main() {
    println!("orca 0.1.0 — launcher scaffold");
    println!("gpuui: not linked yet (see docs/ARCHITECTURE.md)");

    check_core();
    check_win();
}

/// Exercises `orca-core` through the same path the real ranking path will use.
fn check_core() {
    let catalog = vec![
        ResultItem::new(
            "app:notepad",
            "Notepad",
            Source::Application,
            exe("notepad.exe"),
        )
        .with_subtitle("C:\\Windows\\System32\\notepad.exe")
        .with_score(0.42),
        ResultItem::new(
            "app:calc",
            "Calculator",
            Source::Application,
            exe("calc.exe"),
        )
        .with_score(0.10),
        ResultItem::new(
            "web:search",
            "Web search",
            Source::WebSearch,
            LaunchTarget::Uri("https://duckduckgo.com".into()),
        )
        .with_score(0.0),
    ];

    let ranked = rank("note", &catalog);
    println!(
        "core: {} item(s) in the test catalog, {} match \"note\" (top: {:?})",
        catalog.len(),
        ranked.len(),
        ranked.first().map(|r| r.item.title.as_str())
    );
}

/// Exercises `orca-win` far enough to prove the failure path is honest.
fn check_win() {
    match Hotkey::parse(DEFAULT_HOTKEY) {
        Ok(hotkey) => {
            let mut registrar = Win32GlobalHotkey::new();
            match registrar.register(&hotkey) {
                Ok(()) => println!("win: hotkey {DEFAULT_HOTKEY} registered"),
                Err(error) => println!("win: hotkey {DEFAULT_HOTKEY} not bound: {error}"),
            }
        }
        Err(error) => println!("win: {DEFAULT_HOTKEY} is an invalid spec: {error}"),
    }
}

/// Builds a [`LaunchTarget`] for a bare executable name.
fn exe(program: &str) -> LaunchTarget {
    LaunchTarget::Command {
        program: program.to_owned(),
        args: Vec::new(),
    }
}
