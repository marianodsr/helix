//! Method implementations, one file per area so later steps touch disjoint
//! files (see the module layout in bb-bridge/ARCHITECTURE.md).

pub mod annotations;
pub mod command;
pub mod diagnostics;
pub mod editor;
pub mod edits;
pub mod interrupt;
pub mod locations;
pub mod lsp;
pub mod reload;
pub mod reveal;
pub mod send;
pub mod views;

use crate::commands::typed::TypableCommand;

/// Every typed command the bridge adds, for Helix's command map.
pub fn typable_commands() -> impl Iterator<Item = &'static TypableCommand> {
    send::COMMANDS
        .iter()
        .chain(locations::COMMANDS)
        .chain(interrupt::COMMANDS)
        .chain(annotations::COMMANDS)
}

/// Hooks of the later-step modules, registered when the bridge starts.
pub fn register_hooks() {
    reload::register_hooks();
    locations::register_hooks();
    diagnostics::register_hooks();
    lsp::register_hooks();
    interrupt::register_hooks();
    annotations::register_hooks();
    reveal::register_hooks();
}
