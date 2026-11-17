//! "Not yanking the user": idle tracking, the pending jump and `:bb-go`
//! (step 2c). Stub: focus-moving requests move at once until this exists.

use crate::commands::typed::TypableCommand;

/// `:bb-go`.
pub const COMMANDS: &[TypableCommand] = &[];

/// Hooks this module needs (registered when the bridge starts).
pub fn register_hooks() {}
