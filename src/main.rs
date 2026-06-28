mod events;
mod session;
mod tty;
mod tui;

use std::io::{self, BufWriter, Write};
use std::process;

use anyhow::{Context, Result};
use clap::Parser;

use crate::events::{Drain, EventsSub};
use crate::session::{ListReply, Session};
use crate::tui::template::unknown_template_vars;
use crate::tui::{Command, Event, Input, InputParser, Model, Var};

/// Top-level flags. `apply` re-emits the four below — in a fixed order,
/// ahead of the subcommand — onto every `shpool` shell-out (list, kill,
/// attach, events); it forwards exactly these, so a new shpool top-level
/// flag we want to pass through has to be added here too. Mirrors
/// shpool's own top-level flags, so `shpool-table --config-file foo.toml`
/// behaves like `shpool --config-file foo.toml list` etc.
///
/// `--daemonize` / `--no-daemonize` are deliberately not included —
/// auto-launching a daemon from under the TUI (especially mid-session)
/// is confusing UX. A future in-TUI "start daemon" action is the
/// planned way to address that need.
#[derive(Parser, Debug, Clone, Default)]
#[command(about = "A TUI session manager that wraps shpool.", version)]
struct Flags {
    /// Forwarded to every `shpool` invocation as `--config-file <path>`.
    #[arg(long, value_name = "PATH")]
    config_file: Option<String>,

    /// Forwarded as `--log-file <path>`.
    #[arg(long, value_name = "PATH")]
    log_file: Option<String>,

    /// Increase verbosity. Forwarded as `-v` (repeatable).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Forwarded as `--socket <path>`.
    #[arg(long, value_name = "PATH")]
    socket: Option<String>,
}

impl Flags {
    /// Prepend forwardable flags to a `shpool` Command, before its
    /// subcommand. Clap requires global flags to appear before the
    /// subcommand, so callers must apply these *before* `.arg("list")`
    /// / `.arg("attach")` / etc.
    fn apply(&self, cmd: &mut process::Command) {
        if let Some(p) = &self.config_file {
            cmd.args(["--config-file", p]);
        }
        if let Some(p) = &self.log_file {
            cmd.args(["--log-file", p]);
        }
        for _ in 0..self.verbose {
            cmd.arg("-v");
        }
        if let Some(p) = &self.socket {
            cmd.args(["--socket", p]);
        }
    }
}

/// Current wall-clock time in unix milliseconds. Passed into `render`
/// so the relative-age columns have a deterministic `now` (tests pass
/// a fixed value; production passes the current time).
fn now_unix_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn fetch_sessions(flags: &Flags) -> Result<Vec<Session>> {
    let mut cmd = process::Command::new("shpool");
    flags.apply(&mut cmd);
    list_sessions(cmd)
}

/// Same as `fetch_sessions`, but prepends `--daemonize` so shpool
/// auto-forks a daemon first if one isn't running. Idempotent — no
/// effect when the daemon is already up.
fn ensure_daemon_and_list(flags: &Flags) -> Result<Vec<Session>> {
    let mut cmd = process::Command::new("shpool");
    flags.apply(&mut cmd);
    cmd.arg("--daemonize");
    list_sessions(cmd)
}

/// Run `<cmd> list --json` and parse the reply. Caller is responsible
/// for constructing `cmd` with the shpool binary + any global flags
/// already applied.
fn list_sessions(mut cmd: process::Command) -> Result<Vec<Session>> {
    let out = cmd
        .args(["list", "--json"])
        .output()
        .context("running `shpool list --json`")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("`shpool list --json` failed: {}", stderr.trim());
    }
    let reply: ListReply =
        serde_json::from_slice(&out.stdout).context("parsing shpool list JSON")?;
    Ok(reply.sessions)
}

/// Fetch the daemon's template variables via `shpool var list`, which
/// emits one `name<TAB>value` line each (TSV, not JSON). Returns them
/// sorted by name, with the global flags applied exactly as
/// `fetch_sessions` does. Spawn/exit failure becomes an error.
fn list_vars(flags: &Flags) -> Result<Vec<Var>> {
    let mut cmd = process::Command::new("shpool");
    flags.apply(&mut cmd);
    let out = cmd
        .args(["var", "list"])
        .output()
        .context("running `shpool var list`")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("`shpool var list` failed: {}", stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut vars: Vec<Var> = stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            // Split on the first tab; a value-less line yields an empty
            // value, matching shperl's `split /\t/, $line, 2`.
            let (name, value) = line.split_once('\t').unwrap_or((line, ""));
            // A var from `var list` is by definition set, even with an
            // empty value — emptiness is never inferred as unset.
            Var {
                name: name.to_string(),
                value: value.to_string(),
                unset: false,
            }
        })
        .collect();
    vars.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(vars)
}

/// shpool command builders. Each inserts `--` before the
/// user-controlled positionals so a session name or value that begins
/// with a dash (e.g. "-sh") isn't parsed as a flag by shpool's argument
/// parser. Names are `\w+`, but variable values are unconstrained, so
/// var-set needs the guard too.
fn attach_cmd(name: &str, force: bool, flags: &Flags) -> process::Command {
    let mut cmd = process::Command::new("shpool");
    flags.apply(&mut cmd);
    cmd.arg("attach");
    if force {
        cmd.arg("-f");
    }
    cmd.args(["--", name]);
    cmd
}

fn kill_cmd(name: &str, flags: &Flags) -> process::Command {
    let mut cmd = process::Command::new("shpool");
    flags.apply(&mut cmd);
    cmd.args(["kill", "--", name]);
    cmd
}

fn var_set_cmd(name: &str, value: &str, flags: &Flags) -> process::Command {
    let mut cmd = process::Command::new("shpool");
    flags.apply(&mut cmd);
    cmd.args(["var", "set", "--", name, value]);
    cmd
}

/// Run `shpool var set <name> <value>` and collect its outcome + raw
/// stderr. Returns `(ok, raw_trimmed_stderr)`. Unlike `shell_kill`, the
/// stderr is returned verbatim (no `"var set ..."` prefix) — that prefix
/// is applied once, in update's VarSetFinished handler. Shell-out
/// failures (couldn't even spawn shpool) propagate.
fn set_var(name: &str, value: &str, flags: &Flags) -> Result<(bool, Option<String>)> {
    let output = var_set_cmd(name, value, flags)
        .output()
        .context("running `shpool var set`")?;
    let ok = output.status.success();
    let err = if ok {
        None
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        if detail.is_empty() {
            None
        } else {
            Some(detail.to_string())
        }
    };
    Ok((ok, err))
}

fn main() -> Result<()> {
    let flags = Flags::parse();
    if let Ok(inside) = std::env::var("SHPOOL_SESSION_NAME") {
        // Nested sessions get messy: a force-attach to the outer
        // session bumps us off, sessions created from here inherit
        // this env, and ^D leaves the user in the wrong layer.
        // Detach first, then fall through to `shpool list`.
        eprintln!(
            "shpool-table: inside shpool session \"{inside}\" — won't run here. Nested\n\
             sessions get messy (outer attach gets bumped on force, sessions created\n\
             here inherit this env, ^D leaves you in the wrong layer). Detach first\n\
             to manage sessions. Current list:\n"
        );
        use std::os::unix::process::CommandExt;
        let mut cmd = process::Command::new("shpool");
        flags.apply(&mut cmd);
        let err = cmd.arg("list").exec();
        anyhow::bail!("exec shpool list: {err}");
    }
    run_tui(&flags)
}

fn run_tui(flags: &Flags) -> Result<()> {
    tty::install_sigwinch_handler().context("installing SIGWINCH handler")?;
    // Install before constructing the guards so an error in
    // `AltScreen::enter` (unlikely) or any later code still resets
    // the terminal on panic. `panic = "abort"` builds rely on this
    // hook since Drop doesn't run in that mode.
    tty::install_panic_hook();

    let mut model = Model::new(Vec::new());
    let mut parser = InputParser::new();

    // Terminal state guards: both Drop on any exit path (normal
    // return, `?` error propagation, unwinding panic) so the user's
    // shell gets a clean tty back. `execute` toggles them via
    // suspend/resume when shelling out to `shpool attach`.
    let raw = tty::RawMode::enter().context("entering raw mode")?;
    let alt = tty::AltScreen::enter().context("entering alt-screen")?;
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    let result = main_loop(&mut model, &mut parser, &mut out, &raw, &alt, flags);
    let _ = out.flush();
    result
}

/// Run the cascade loop until `model.quit` is set.
///
/// Keeps the TUI in alt-screen + raw mode the whole time, except when
/// `execute` spawns a child `shpool attach` — that path toggles both
/// off, runs the child, and toggles them back on.
///
/// Multiplexes stdin with a `shpool events` subscription (when one can
/// be had) so changes from other clients refresh the table without a
/// keystroke. When the subscription is unavailable or drops, the loop
/// falls back to refreshing on keystrokes + focus events, and climbs
/// back to push mode the next time the user does something — see the
/// reconnect note below. The fallback is visible, not silent: an EOF
/// surfaces a footer notice so the user knows push updates have paused.
fn main_loop<W: Write>(
    model: &mut Model,
    parser: &mut InputParser,
    out: &mut W,
    raw: &tty::RawMode,
    alt: &tty::AltScreen,
    flags: &Flags,
) -> Result<()> {
    // Subscribe before the initial list so a change landing during the
    // list call still wakes us. The subscribe is also the capability
    // probe: if it can't run, `events` stays None and we open in
    // keystroke-refresh fallback.
    let mut events = EventsSub::spawn(flags);
    model.events_active = events.is_some();

    // Initial fetch: if the daemon is up, show its list immediately;
    // if not, the RefreshFailed event surfaces the error in the footer
    // and the user can retry (or quit).
    let initial = match fetch_sessions(flags) {
        Ok(s) => Event::SessionsRefreshed(s),
        Err(e) => Event::RefreshFailed(format!("{e}")),
    };
    run_cascade(model, initial, out, raw, alt, flags, &mut events)?;

    let mut buf = [0u8; 16];
    loop {
        let (w, h) = tty::tty_size().unwrap_or((80, 24));
        let now = now_unix_ms();
        tui::render(model, w, h, now, out)?;
        out.flush()?;

        // `quit` is checked AFTER the draw, not before. The final
        // frame is written but immediately wiped by AltScreen::drop
        // on exit, so the user never sees it. Saves one draw-on-exit
        // and keeps the model's visible state aligned with the last
        // drawn frame — inverting would subtly decouple them.
        if model.quit {
            return Ok(());
        }

        // Wake on input, a push event, or — so the relative-age columns
        // advance while the table is idle — when the soonest on-screen age
        // would next change. A pure-timeout wake just re-renders with a
        // fresh `now`; it sets neither ready flag, so it can't be mistaken
        // for user activity (no reconnect) or a push (no refresh).
        let timeout = tui::next_render_delay_ms(model, now);
        let ready = match tty::poll_readable(events.as_ref().map(EventsSub::fd), timeout) {
            Ok(r) => r,
            // SIGWINCH — loop back to re-query tty_size and redraw.
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("polling stdin + events"),
        };

        // Drain the event stream first, so a refresh it triggers is in
        // place before we react to a keystroke from the same wake — the
        // attached flag an Enter would pre-flight against, say.
        if ready.events {
            match events.as_mut().map(EventsSub::drain) {
                Some(Drain::Activity) => {
                    run_cascade(
                        model,
                        Event::EventsArrived,
                        out,
                        raw,
                        alt,
                        flags,
                        &mut events,
                    )?;
                }
                Some(Drain::Eof) => {
                    // Subscription ended: daemon down, slow-subscriber
                    // drop, or a daemon with no events socket. Tear it
                    // down, catch the list up once, and surface it — we
                    // don't auto-reconnect until the user acts, so they
                    // should know push has paused. A specific list error
                    // (daemon truly down) outranks the generic notice.
                    teardown_events(&mut events, model);
                    run_cascade(
                        model,
                        Event::EventsArrived,
                        out,
                        raw,
                        alt,
                        flags,
                        &mut events,
                    )?;
                    if model.error.is_none() {
                        model.set_error(
                            "events unavailable — refreshing on keypress (D to reconnect)",
                        );
                    }
                }
                None => {}
            }
        }

        if ready.stdin {
            match tty::read_stdin(&mut buf) {
                Ok(0) => model.quit = true, // EOF on stdin — exit cleanly.
                Ok(n) => {
                    // A single read can decode to multiple inputs
                    // (pastes, CSI sequences, jj\r typed fast, a
                    // focus-report next to a keystroke). Feed each
                    // through its own cascade so auto-refresh / attach /
                    // etc. fire per input.
                    let mut inputs = Vec::new();
                    parser.feed(&buf[..n], &mut inputs);
                    for input in inputs {
                        let event = match input {
                            Input::Key(k) => Event::Key(k),
                            Input::FocusGained => Event::FocusGained,
                        };
                        run_cascade(model, event, out, raw, alt, flags, &mut events)?;
                        if model.quit {
                            break;
                        }
                    }
                }
                // SIGWINCH landed between poll and read — re-render.
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e).context("reading stdin"),
            }

            // On-activity reconnect. A keystroke or focus event got us
            // here, so if we're unsubscribed, try to climb back to push
            // mode. The subscribe is cheap and self-correcting: if the
            // daemon still can't serve events, the next poll sees EOF
            // and we drop back. Gating on `ready.stdin` is what keeps an
            // event-driven EOF from spinning (teardown → resubscribe →
            // EOF → …) — we only retry when the user actually did
            // something, never off our own fallback refresh.
            if events.is_none() && !model.quit {
                events = EventsSub::spawn(flags);
                model.events_active = events.is_some();
            }
        }
    }
}

/// Feed `event` through `update`, execute any resulting Command, then
/// feed the executor's follow-up event back through `update`, repeat
/// until the cascade runs dry.
///
/// Load-bearing: the Create flow is
///   Key(Enter) → Create → AttachExited → Refresh → SessionsRefreshed
/// where the final Refresh step is how the newly-created session
/// enters `model.sessions`. Stopping after the first Command would
/// leave the new session invisible.
fn run_cascade<W: Write>(
    model: &mut Model,
    event: Event,
    out: &mut W,
    raw: &tty::RawMode,
    alt: &tty::AltScreen,
    flags: &Flags,
    events: &mut Option<EventsSub>,
) -> Result<()> {
    let mut next = tui::update(model, event);
    while let Some(cmd) = next.take() {
        let follow_up = execute(cmd, model, out, raw, alt, flags, events)?;
        let Some(ev) = follow_up else { break };
        next = tui::update(model, ev);
    }
    Ok(())
}

/// Side-effect executor. All I/O happens here: `shpool list --json`,
/// `shpool kill`, the attach subprocess + terminal suspend, and the
/// quit flag. Returns the follow-up Event for the cascade to feed
/// back through `update`, or None if there's nothing to cascade.
fn execute<W: Write>(
    cmd: Command,
    model: &mut Model,
    out: &mut W,
    raw: &tty::RawMode,
    alt: &tty::AltScreen,
    flags: &Flags,
    events: &mut Option<EventsSub>,
) -> Result<Option<Event>> {
    match cmd {
        Command::Quit => {
            model.quit = true;
            Ok(None)
        }
        Command::Refresh => {
            let ev = match fetch_sessions(flags) {
                Ok(sessions) => Event::SessionsRefreshed(sessions),
                Err(e) => Event::RefreshFailed(format!("{e}")),
            };
            Ok(Some(ev))
        }
        Command::EnsureDaemon => {
            // `--daemonize` makes shpool fork a daemon if one isn't
            // running, then run the list. Idempotent: if the daemon
            // is already up, the flag is a no-op. Result is the same
            // shape as a plain Refresh.
            let ev = match ensure_daemon_and_list(flags) {
                Ok(sessions) => Event::SessionsRefreshed(sessions),
                Err(e) => Event::RefreshFailed(format!("{e}")),
            };
            Ok(Some(ev))
        }
        Command::Attach { name, force } => match preflight_attach(&name, force, flags) {
            Preflight::RefreshFailed(e) => {
                // Route through Event::RefreshFailed so the
                // "shpool list:" prefix lives in exactly one place
                // (update's RefreshFailed handler).
                Ok(Some(Event::RefreshFailed(format!("{e}"))))
            }
            Preflight::Gone { sessions } => {
                model.set_error(format!("session '{name}' is gone"));
                Ok(Some(Event::SessionsRefreshed(sessions)))
            }
            Preflight::AttachedElsewhere { sessions } => {
                // Pop ConfirmForce rather than bumping the other
                // client silently. The user can press 'y' to re-issue
                // Attach with force=true, which skips this check.
                model.mode = crate::tui::Mode::ConfirmForce(name);
                Ok(Some(Event::SessionsRefreshed(sessions)))
            }
            Preflight::ClearToAttach => {
                // Drop the subscriber for the duration of the attached
                // session: we're about to block in the child and can't
                // drain its pipe, so the daemon would drop us as a slow
                // subscriber anyway. main_loop's on-activity reconnect
                // brings it back when the attach returns.
                teardown_events(events, model);
                let ok = with_tui_suspended(out, raw, alt, || shell_attach(&name, force, flags))?;
                Ok(Some(Event::AttachExited { ok, name }))
            }
        },
        Command::Create(name) => {
            // Create-time variable prompt detect, BEFORE any teardown:
            // list the daemon's vars and find the ones the new name
            // references that aren't set yet. If any, bounce straight back
            // as CreateNeedsVars — no teardown, no suspend, we stay in the
            // alt-screen so the prompt renders without a flicker. On a
            // `var list` failure, abort the create with the error surfaced
            // (don't attach blind). With every referenced var already set,
            // fall through to today's teardown -> attach unchanged.
            //
            // The duplicate-name check already ran in update (before
            // Command::Create was emitted), so detection never sees a
            // duplicate. `shpool attach` with a new name creates
            // atomically on the daemon side.
            match list_vars(flags) {
                Ok(set_list) => {
                    let known: std::collections::HashSet<&str> =
                        set_list.iter().map(|v| v.name.as_str()).collect();
                    let vars = unknown_template_vars(&name, &known);
                    if !vars.is_empty() {
                        let set_vars = set_list
                            .into_iter()
                            .map(|v| (v.name, v.value))
                            .collect::<Vec<_>>();
                        return Ok(Some(Event::CreateNeedsVars {
                            name,
                            vars,
                            set_vars,
                        }));
                    }
                }
                Err(e) => {
                    model.set_error(format!("shpool var list: {e}"));
                    return Ok(None);
                }
            }
            teardown_events(events, model);
            let ok = with_tui_suspended(out, raw, alt, || shell_attach(&name, false, flags))?;
            Ok(Some(Event::AttachExited { ok, name }))
        }
        Command::CreateWithVars { name, set_vars } => {
            // Apply step: set each collected pair in order
            // (last-write-wins against any concurrent daemon-side change),
            // then teardown -> attach exactly like Create. The loop runs
            // here (not via VarSetFinished) so one Command yields one
            // follow-up Event, matching run_cascade's shape. On a set
            // failure, abort with no attach and surface CreateVarsFailed;
            // partial sets linger (no rollback) — the next detect sees
            // them as known.
            for (var, value) in &set_vars {
                let (ok, err) = set_var(var, value, flags)?;
                if !ok {
                    return Ok(Some(Event::CreateVarsFailed {
                        var: var.clone(),
                        err,
                    }));
                }
            }
            teardown_events(events, model);
            let ok = with_tui_suspended(out, raw, alt, || shell_attach(&name, false, flags))?;
            Ok(Some(Event::AttachExited { ok, name }))
        }
        Command::Kill(name) => {
            let (ok, err) = shell_kill(&name, flags)?;
            Ok(Some(Event::KillFinished { ok, name, err }))
        }
        Command::FetchVars => {
            let ev = match list_vars(flags) {
                Ok(vars) => Event::VarsFetched(vars),
                Err(e) => Event::VarsFetchFailed(format!("{e}")),
            };
            Ok(Some(ev))
        }
        Command::SetVar { name, value } => {
            let (ok, err) = set_var(&name, &value, flags)?;
            // On success, refetch the list so the view reflects the new
            // value. `.ok()` keeps the old list silently if the refetch
            // itself fails (update leaves VarsState.vars untouched).
            let vars = if ok { list_vars(flags).ok() } else { None };
            Ok(Some(Event::VarSetFinished {
                name,
                ok,
                err,
                vars,
            }))
        }
    }
}

/// Result of the attach pre-flight — a fresh `shpool list --json`
/// query followed by a presence + attached-elsewhere check.
///
/// Split out of `execute` because the decision (which of these four
/// outcomes) is independent of the action (what to emit next). Also
/// keeps the match arms in execute readable.
enum Preflight {
    /// `fetch_sessions` itself failed — error is display-ready.
    RefreshFailed(anyhow::Error),
    /// Session no longer exists; some other client killed it since
    /// the user's last view. Fresh sessions are carried so the
    /// follow-up Event::SessionsRefreshed can update the model.
    Gone { sessions: Vec<Session> },
    /// Session exists but is attached from another terminal and
    /// `force` was not set. Caller transitions model to ConfirmForce.
    AttachedElsewhere { sessions: Vec<Session> },
    /// Session exists and is ready to attach. `sessions` is not
    /// carried — the AttachExited handler cascades into a fresh
    /// Refresh anyway.
    ClearToAttach,
}

fn preflight_attach(name: &str, force: bool, flags: &Flags) -> Preflight {
    let sessions = match fetch_sessions(flags) {
        Ok(s) => s,
        Err(e) => return Preflight::RefreshFailed(e),
    };
    if !sessions.iter().any(|s| s.name == name) {
        return Preflight::Gone { sessions };
    }
    let attached_elsewhere = !force
        && sessions
            .iter()
            .find(|s| s.name == name)
            .map(|s| s.attached)
            .unwrap_or(false);
    if attached_elsewhere {
        return Preflight::AttachedElsewhere { sessions };
    }
    Preflight::ClearToAttach
}

/// Spawn `shpool attach [-f] <name>` and block until it exits.
/// The caller (`with_tui_suspended`) is responsible for putting the
/// terminal into cooked mode + primary screen first.
fn shell_attach(name: &str, force: bool, flags: &Flags) -> Result<bool> {
    let status = attach_cmd(name, force, flags)
        .status()
        .context("spawning `shpool attach`")?;
    Ok(status.success())
}

/// Run `shpool kill <name>` and collect its outcome + stderr.
/// Returns (ok, err_message). Shell-out errors (couldn't even spawn
/// shpool) propagate; non-zero exit with a stderr payload comes back
/// as `ok=false` + `err=Some(detail)`.
fn shell_kill(name: &str, flags: &Flags) -> Result<(bool, Option<String>)> {
    let output = kill_cmd(name, flags)
        .output()
        .context("running `shpool kill`")?;
    let ok = output.status.success();
    let err = if ok {
        None
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        if detail.is_empty() {
            Some(format!("kill {name} failed"))
        } else {
            Some(format!("kill {name}: {detail}"))
        }
    };
    Ok((ok, err))
}

/// Drop the events subscriber and reflect that on the model. The
/// `EventsSub` Drop kills + reaps the child, so this won't block even
/// though the child sits reading the daemon socket.
fn teardown_events(events: &mut Option<EventsSub>, model: &mut Model) {
    *events = None;
    model.events_active = false;
}

/// Tear the TUI down (leave alt-screen, cooked mode), run `f`, then
/// put the TUI back up. Used for the attach subprocess, which needs a
/// clean cooked tty to take over.
///
/// Restores terminal state on both success and error return paths of
/// `f` so an attach that failed halfway still hands us back a usable
/// TUI.
fn with_tui_suspended<F, R, W: Write>(
    out: &mut W,
    raw: &tty::RawMode,
    alt: &tty::AltScreen,
    f: F,
) -> Result<R>
where
    F: FnOnce() -> Result<R>,
{
    alt.suspend()?;
    // Clear the primary screen before the child starts so its first
    // frame doesn't land on top of leftover shell prompt history.
    tty::clear_screen(out)?;
    out.flush()?;
    raw.suspend()?;

    // Capture the result so we still restore the terminal on error.
    let result = f();

    raw.resume()?;
    alt.resume()?;
    out.flush()?;

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(cmd: &process::Command) -> Vec<String> {
        cmd.get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn attach_cmd_guards_dash_led_name() {
        let flags = Flags::default();
        assert_eq!(
            args_of(&attach_cmd("-sh", false, &flags)),
            ["attach", "--", "-sh"]
        );
        assert_eq!(
            args_of(&attach_cmd("-sh", true, &flags)),
            ["attach", "-f", "--", "-sh"]
        );
    }

    #[test]
    fn kill_cmd_guards_dash_led_name() {
        let flags = Flags::default();
        assert_eq!(args_of(&kill_cmd("-sh", &flags)), ["kill", "--", "-sh"]);
    }

    #[test]
    fn var_set_cmd_guards_dash_led_value() {
        let flags = Flags::default();
        assert_eq!(
            args_of(&var_set_cmd("coin", "-xmr", &flags)),
            ["var", "set", "--", "coin", "-xmr"]
        );
    }

    #[test]
    fn flags_apply_nothing_when_default() {
        let flags = Flags::default();
        let mut cmd = process::Command::new("shpool");
        flags.apply(&mut cmd);
        assert!(args_of(&cmd).is_empty());
    }

    #[test]
    fn flags_apply_all_fields() {
        let flags = Flags {
            config_file: Some("/etc/shpool/custom.toml".into()),
            log_file: Some("/var/log/shpool.log".into()),
            verbose: 2,
            socket: Some("/tmp/shpool.sock".into()),
        };
        let mut cmd = process::Command::new("shpool");
        flags.apply(&mut cmd);
        let args = args_of(&cmd);
        // Exact order + presence check. Duplicated -v for verbose=2.
        assert_eq!(
            args,
            vec![
                "--config-file",
                "/etc/shpool/custom.toml",
                "--log-file",
                "/var/log/shpool.log",
                "-v",
                "-v",
                "--socket",
                "/tmp/shpool.sock",
            ],
        );
    }

    #[test]
    fn flags_precede_subcommand() {
        // clap requires global flags to appear before the subcommand.
        // Apply flags first, then the subcommand: the subcommand must
        // end up after every forwarded flag.
        let flags = Flags {
            config_file: Some("/c".into()),
            log_file: Some("/l".into()),
            verbose: 1,
            socket: Some("/s".into()),
        };
        let mut cmd = process::Command::new("shpool");
        flags.apply(&mut cmd);
        cmd.args(["list", "--json"]);
        let args = args_of(&cmd);
        let list_pos = args.iter().position(|a| a == "list").expect("list present");
        for flag in ["--config-file", "--log-file", "-v", "--socket"] {
            let pos = args.iter().position(|a| a == flag).expect(flag);
            assert!(
                pos < list_pos,
                "{flag} must come before `list`; got {args:?}"
            );
        }
    }
}
