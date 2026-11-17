//! The bridge needs Unix domain sockets; elsewhere it is absent and its
//! hooks do nothing.

use helix_view::{DocumentId, Editor};

use crate::commands::typed::TypableCommand;

pub fn start(_editor: &Editor) {}

pub fn stop() {}

pub fn document_saved(_editor: &Editor, _doc: DocumentId) {}

pub fn focus_gained(_editor: &mut Editor) {}

pub fn typable_commands() -> impl Iterator<Item = &'static TypableCommand> {
    std::iter::empty()
}
