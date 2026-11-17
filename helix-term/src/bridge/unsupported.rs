//! The bridge needs Unix domain sockets; elsewhere it is absent and its
//! hooks do nothing.

use helix_view::{DocumentId, Editor};

use crate::commands::typed::TypableCommand;

pub fn start(_editor: &Editor) {}

pub fn stop() {}

pub fn document_saved(_editor: &Editor, _doc: DocumentId) {}

pub fn decorate<'d>(
    _doc: &'d helix_view::Document,
    _view: &helix_view::View,
    _theme: &helix_view::Theme,
    _decorations: &mut crate::ui::text_decorations::DecorationManager<'d>,
) {
}
pub fn diagnostics_published(_uri: &helix_core::Uri) {}

pub fn typable_commands() -> impl Iterator<Item = &'static TypableCommand> {
    std::iter::empty()
}
