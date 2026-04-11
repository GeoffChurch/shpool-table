# shpool-manager design plan

## What this is

`shpool-manager` is a standalone TUI that wraps [shpool] as a session manager.
It is **not** a shpool session itself — it runs as a normal process owning
your terminal (e.g., as your SSH entry-point), draws a list of sessions, and
lets you select/create/kill them. Selecting a session spawns
`shpool attach <name>` as a child process, which takes over the terminal until
you detach with shpool's existing `Ctrl-Space Ctrl-q` binding, at which point
the child exits and you land back in the manager's menu.

[shpool]: https://github.com/shell-pool/shpool

## Why this design

Two alternatives were considered and rejected:

1. **Daemon-side PTY session for the manager.** Would require a new control
   chunk in shpool's wire protocol, a mechanism to hot-swap which session the
   daemon streams to a live client, and touches the `SessionInner` lock that's
   held exactly while a client is attached. Invasive.
2. **Client-side TUI triggered by an in-session keybinding.** Smaller than
   option 1, but still requires: a new `Action` variant in
   `libshpool/src/daemon/keybindings.rs`, some way for the daemon to signal
   the client that "the user asked for the manager" mid-stream, and upstream
   changes to shpool.

The wrapper-process approach requires **zero changes to shpool** for v0.1.
The daemon never knows the manager exists. When the manager shells out to
`shpool attach <name>`, shpool sees a perfectly normal client connection.
When the user detaches, shpool sees a perfectly normal client disconnection.
The manager just happens to be the parent process waiting on the child.

## Why shell out to the CLI instead of using `shpool-protocol` directly

Per shpool's `HACKING.md` version policy:

- The `shpool` **CLI** and config file format are explicitly *public* interface
  — semver-breaking changes require a major version bump.
- The **wire protocol** between attach and daemon (the `shpool-protocol` crate)
  is explicitly *not* public and can change in any release.

Shelling out pins us to the stable contract and survives shpool upgrades. It
also means we inherit `shpool`'s autodaemonization, socket-path discovery, and
version handshake for free, without duplicating that logic.

**Latency**: `shpool list --json` measures in single-digit milliseconds
including the daemon roundtrip. `fork`+`exec` on Linux is sub-millisecond. In
a TUI, `list` is called on menu open and on an explicit refresh key — not per
keystroke. Human perception is ~100ms, so we have two orders of magnitude of
headroom.

## v0.1 scope

- **List sessions.** Call `shpool list --json`, parse, display.
- **Select → attach.** Spawn `shpool attach <name>` as a child, wait, redraw
  the menu on return.
- **Create.** Prompt for a name, spawn `shpool attach <new-name>` (shpool
  creates a session on first attach).
- **Kill.** Shell out to `shpool kill <name>`, then refresh.
- **Resize.** Redraw on `SIGWINCH`.

## Non-goals (explicitly)

- **Rename.** shpool has no rename primitive today. Out of scope.
- **Previews.** Requires a screen-snapshot endpoint in shpool that doesn't
  exist. Possible future work — see "Future directions" below.
- **Multiplexing.** Like shpool itself, the manager is a session selector, not
  a terminal multiplexer. If you want tiling, use a tiling window manager.
- **Replacing shpool's keybinding engine.** The in-session detach binding
  remains shpool's job.

## Iteration plan

Each milestone is a reviewable commit.

1. **Plumbing.** `shpool list --json` → parse → print formatted list. No TUI
   yet. *(this scaffold)*
2. **Raw mode + selection.** Put stdin in raw mode, arrow keys move the
   selection, Enter spawns `shpool attach <name>`, on return refresh the list.
3. **Create.** `n` key prompts for a name, spawns attach.
4. **Kill.** `k` key kills (with confirmation) the highlighted session.
5. **Resize.** `SIGWINCH` handling, redraw on resize.
6. **Polish.** Error display, color, empty-state messaging.

## Dependencies

Minimal by default:

- `anyhow` — error handling
- `serde` + `serde_json` — parsing `shpool list --json` output

Added as later milestones require:

- **Raw mode + terminal size**: `nix` (already a shpool transitive dep, well
  maintained) or `termios` (smaller, more focused). Decision deferred to when
  milestone 2 lands.
- **SIGWINCH**: whichever of the above we picked; `signal-hook` as a fallback.

We're explicitly *not* reaching for `ratatui` / `crossterm` unless milestone 6
reveals a strong reason. The TUI is intended to be small enough that
hand-rolled ANSI is simpler than a framework.

## Session-status handling

`shpool list --json` serializes session status as `"Attached"` or
`"Disconnected"` (the `SessionStatus` enum variant names in
`shpool-protocol`). We use `#[serde(other)]` on a catch-all `Unknown` variant
so future additions in shpool don't break parsing.

## Future directions (post-v0.1)

If previews turn out to be worth having, the cleanest path is to propose a
new CLI subcommand upstream to shpool:

```
shpool peek <name>    # prints the current in-memory screen snapshot as ANSI
```

Per the shpool version policy this is a public-interface addition (not a
breaking change), and `shpool-manager` can keep its CLI-only, shell-out
architecture while gaining a preview pane. Worth filing as an upstream issue
first to check the maintainers' appetite — and per shpool's `HACKING.md` AI
policy, *write that issue yourself*, don't have an AI draft it.
