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

    /// Ensure a shpool daemon is running, then refetch the session
    /// list. Equivalent to `shpool --daemonize list --json`: idempotent
    /// (no-op if the daemon is already up). Result comes back as
    /// Event::SessionsRefreshed or Event::RefreshFailed.
    EnsureDaemon,

    /// Spawn `shpool attach [-f] <name>` as a child process. `force`
    /// passes `-f` through — reached either from a plain attach or
    /// from the ConfirmForce prompt.
    Attach { name: String, force: bool },

    /// Spawn `shpool attach <new-name>`, which create-or-attaches on
    /// the daemon side. Distinct from Attach so the executor can skip
    /// the "session must already exist" pre-flight check.
    ///
    /// Detect step for the create-time variable prompt: the executor
    /// runs `var list` + `unknown_template_vars` at the top, *before*
    /// tearing down the alt-screen. Unknowns come back as
    /// Event::CreateNeedsVars (no teardown, stays in alt-screen); none
    /// falls through to teardown -> attach.
    Create(String),

    /// Apply step for the create-time variable prompt: set each
    /// collected `(name, value)` pair via `shpool var set` in order,
    /// then teardown -> attach exactly like Create. A set failure aborts
    /// (no attach) and comes back as Event::CreateVarsFailed. Partial
    /// sets linger (no rollback). The loop runs in the executor, not via
    /// VarSetFinished.
    CreateWithVars {
        name: String,
        set_vars: Vec<(String, String)>,
    },

    /// Kill the named session via `shpool kill`.
    Kill(String),

    /// Fetch the daemon's template variables via `shpool var list`.
    /// Result comes back as Event::VarsFetched or Event::VarsFetchFailed.
    FetchVars,

    /// Set a template variable via `shpool var set <name> <value>`, then
    /// (on success) refetch the list. Result comes back as
    /// Event::VarSetFinished.
    SetVar { name: String, value: String },

    /// Stop the main loop.
    Quit,
}
