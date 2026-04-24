//! The `Event` type — everything `update` can react to.
//!
//! Only `Key(Key)` is populated in this commit. Async result events
//! (SessionsRefreshed / RefreshFailed / AttachExited / KillFinished)
//! land in a subsequent commit, at which point the main loop grows
//! the cascade that feeds executor results back into update.

use super::keymap::Key;

#[derive(Debug)]
pub enum Event {
    /// A decoded keystroke from the parser. `update` pattern-matches
    /// on the Key to drive mode transitions and emit Commands.
    Key(Key),
}
