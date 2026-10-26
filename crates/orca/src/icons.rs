//! Loading result-row icons: `orca-win` reads the pixels, this wraps them for
//! the renderer.
//!
//! Which icon a row shows, and which have already been asked for, is decided in
//! `orca_core::icon`. This file is only the conversion, kept out of `ui.rs` so
//! the Linux preview can substitute a loader that has no shell to ask.

use std::sync::Arc;
use std::time::Instant;

use gpui::RenderImage;
use image::{Delay, Frame, RgbaImage};
use orca_core::IconKey;
use orca_win::{shell_icon, IconError, ShellIcon};

/// Loads every key, in order. Blocking; runs on the background executor.
///
/// A failed load yields `None` for its key rather than an error: a row without
/// an icon is a cosmetic gap, never a reason to drop the row. One summary line
/// is logged per batch so a run that shows blank slots can be told apart from
/// one that never asked.
pub fn load_all(keys: Vec<IconKey>) -> Vec<(IconKey, Option<Arc<RenderImage>>)> {
    let started = Instant::now();
    let mut first_error = None;
    let loaded = keys
        .into_iter()
        .map(|key| {
            let icon = match load(&key) {
                Ok(icon) => Some(icon),
                Err(error) => {
                    first_error.get_or_insert_with(|| format!("{key:?}: {error}"));
                    None
                }
            };
            (key, icon)
        })
        .collect::<Vec<_>>();

    let failed = loaded.iter().filter(|(_, icon)| icon.is_none()).count();
    let mut line = format!(
        "icons: loaded {} of {} in {:.1} ms",
        loaded.len() - failed,
        loaded.len(),
        started.elapsed().as_secs_f64() * 1000.0
    );
    if let Some(error) = first_error {
        line.push_str(&format!(", first failure {error}"));
    }
    crate::log(&line);
    loaded
}

fn load(key: &IconKey) -> Result<Arc<RenderImage>, IconError> {
    let request = match key {
        IconKey::Path(path) => ShellIcon::File(path),
        IconKey::Extension(extension) => ShellIcon::Extension(extension),
        IconKey::Folder => ShellIcon::Folder,
    };
    let icon = shell_icon(request)?;
    // BGRA into a type named `RgbaImage` is correct, not a slip: a
    // `RenderImage` stores BGRA whatever its frame type says. See
    // `backdrop::frame_of`.
    let buffer =
        RgbaImage::from_raw(icon.width, icon.height, icon.bgra).ok_or(IconError::Unsupported {
            what: "pixel buffer does not match its dimensions",
        })?;
    let frame = Frame::from_parts(buffer, 0, 0, Delay::from_numer_denom_ms(100, 1));
    Ok(Arc::new(RenderImage::new([frame])))
}
