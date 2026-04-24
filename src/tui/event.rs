//! The `Event` type — everything `update` can react to.

use crate::session::Session;

use super::keymap::Key;

#[derive(Debug)]
pub enum Event {
    /// A decoded keystroke from the parser. `update` pattern-matches
    /// on the Key to drive mode transitions and emit Commands.
    Key(Key),

    /// The `shpool list --json` shell-out returned a fresh session
    /// list. `update` applies it to the model and emits no Command.
    SessionsRefreshed(Vec<Session>),

    /// The `shpool list --json` shell-out failed. The string is a
    /// display-ready error; `update` surfaces it in the footer.
    RefreshFailed(String),

    /// A child `shpool attach` process returned control. `ok` is
    /// whether it exited cleanly. `update` surfaces errors and
    /// reselects the target session by name, then emits
    /// Command::Refresh so the list reflects any state changes that
    /// happened while we were suspended.
    AttachExited { ok: bool, name: String },

    /// A `shpool kill` shell-out finished. `ok` reflects exit status;
    /// `err` carries a display-ready message if it failed. `update`
    /// surfaces the error and emits Command::Refresh so the list
    /// reflects the (potentially now-missing) session.
    KillFinished { ok: bool, name: String, err: Option<String> },
}
