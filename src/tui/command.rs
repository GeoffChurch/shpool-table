//! `Command` — side effects the executor (main.rs) carries out.
//!
//! Produced by `update`, consumed by the main loop. Named variants
//! rather than `dyn FnOnce` so the main-loop match is exhaustive and
//! each action's intent is visible at the call site.

#[derive(Debug, PartialEq)]
pub enum Command {
    /// Refetch the session list. Result comes back as
    /// Event::SessionsRefreshed or Event::RefreshFailed.
    Refresh,

    /// Spawn `shpool attach [-f] <name>` as a child process. `force`
    /// passes `-f` through — reached either from a plain attach or
    /// from the ConfirmForce prompt.
    Attach { name: String, force: bool },

    /// Spawn `shpool attach <new-name>`, which create-or-attaches on
    /// the daemon side. Distinct from Attach so the executor can skip
    /// the "session must already exist" pre-flight check.
    Create(String),

    /// Kill the named session via `shpool kill`.
    Kill(String),

    /// Stop the main loop.
    Quit,
}
