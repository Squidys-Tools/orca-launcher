//! What happens when the user presses <kbd>Enter</kbd>.
//!
//! Two steps, in this order, and the order is the design:
//!
//! 1. Credit the launch in the frecency store.
//! 2. Ask the platform to open it.
//!
//! Crediting first means the frecency that ranks the *next* popup reflects what
//! the user just did even if the process that gets spawned dies immediately.
//! Opening first would leave a window where a launch that crashed on startup
//! has silently failed.
//!
//! Neither step reports failure to the user, and both are safe to ignore:
//! a launch that cannot be recorded is a ranking refinement lost, and a launch
//! that cannot be opened is something the user already sees. Making either one
//! an error path would mean a locked database or a missing program turned
//! <kbd>Enter</kbd> into a dialog.

use orca_core::ResultItem;

use crate::catalog::Engine;
use crate::win;

/// Credits `item` and opens it.
///
/// Returns `false` when the platform refused to open it, which is worth logging
/// and nothing more: there is no UI for it, and the popup is about to close.
pub fn activate(item: &ResultItem, engine: &Engine) -> bool {
    engine.record_launch(item);
    win::shell_open(&item.target)
}
