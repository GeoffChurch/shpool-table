# shpool-manager

A lightweight TUI for managing [shpool] sessions.

`shpool-manager` is **not** a shpool session itself — it's a standalone
process that owns your terminal, shows your sessions in a navigable list,
and spawns `shpool attach` when you pick one. When you detach from a
session (via shpool's detach keybinding), you land back in the manager's
menu. It requires **zero changes to shpool** — the daemon doesn't know
the manager exists.

[shpool]: https://github.com/shell-pool/shpool

## Installation

Requires [shpool] and a Rust toolchain (1.85+).

```
cargo install --path .
```

Or to build from source without installing:

```
cargo build --release
# binary at target/release/shpool-manager
```

## Usage

```
shpool-manager
```

| Key          | Action                                    |
|--------------|-------------------------------------------|
| Up / Down    | Move selection                            |
| Enter        | Attach to the selected session            |
| n            | Create a new session (prompts for name)   |
| k            | Kill the selected session (confirms first)|
| q / Ctrl-C   | Quit                                      |

The TUI footer is the canonical source for key bindings — the table
above is a convenience snapshot.

When you attach, `shpool attach <name>` takes over the terminal. Detach
with shpool's keybinding (default `Ctrl-Space Ctrl-q`, configurable in
`~/.config/shpool/config.toml`) and you're back in the manager.

### As an SSH entry-point

Set your SSH config to land directly in the manager:

```
Host myserver
    Hostname remote.example.com
    RemoteCommand shpool-manager
    RequestTTY yes
```

### Alongside tmux or other multiplexers

`shpool-manager` handles session navigation, not terminal multiplexing.
If you want split panes or tiled windows, run your multiplexer as the
outer layer and `shpool-manager` inside each pane:

- **tmux / zellij**: each pane runs `shpool-manager`, you pick a shpool
  session per pane. Your multiplexer layout persists independently from
  your shpool sessions — detaching from shpool drops you back to the
  manager in that pane, not out of the multiplexer.
- **dtach / abduco**: same idea — these handle persistence at the
  multiplexer level while shpool handles it at the shell level.
- **Tiling window managers** (sway, i3, etc.): each terminal window runs
  `shpool-manager` directly.

## Multiplexing and previews

`shpool-manager` doesn't currently multiplex (show multiple sessions
side-by-side). This is possible in principle — shpool continuously
maintains an in-memory virtual terminal render of every session (via
`shpool_vt100`), so a future version could display live previews or a
split view by requesting snapshots from the daemon. This would require a
small upstream addition to shpool (e.g., a `shpool peek <name>`
subcommand). See [Future directions](#future-directions) below.

## Architecture

The manager shells out to the `shpool` CLI rather than using
`shpool-protocol` directly. Per shpool's version policy, the CLI is a
public, semver-stable interface while the wire protocol is explicitly
not — shelling out means `shpool-manager` survives shpool upgrades
without breakage, and inherits autodaemonization, socket discovery, and
version negotiation for free.

### Code layout

| File             | Role                                               |
|------------------|----------------------------------------------------|
| `src/main.rs`    | Entry point, `fetch_sessions`, TUI orchestration   |
| `src/session.rs` | Serde types for `shpool list --json` output        |
| `src/tui.rs`     | Model, input parser, render — pure logic, tested   |
| `src/tty.rs`     | Unsafe libc wrappers: raw mode, tty size, alt screen |

All unsafe code is isolated in `tty.rs` behind RAII (`RawMode` guard).
The pure state-transition and input-parsing logic in `tui.rs` has unit
tests; no trait-based mocking layer — direct style, small enough to
refactor if needed.

### Testing

```bash
cargo test
```

The test suite covers:

- **JSON schema compatibility.** Deserializes representative `shpool
  list --json` output (including unknown status variants and extra
  fields) to catch serde drift without needing a running daemon.
- **Selection state.** Wrap-around for up/down, empty-list edge case.
- **Input parsing.** Escape-sequence state machine for arrow keys,
  Enter, quit keys, and unknown sequences.
- **Input→action dispatch.** `process_input` is extracted from the
  event loop as a pure function: given a byte buffer, a `Model`, and an
  `InputParser`, it returns an optional `LoopAction`. Tests verify
  multi-key sequences like "Down, Down, Enter → Attach third session"
  without any terminal I/O.

End-to-end tests against a real shpool daemon are feasible (create
sessions with `shpool attach -b`, assert on output, clean up with
`shpool kill`) but not yet wired up. The main consideration is test
isolation — each test needs its own daemon socket and config to avoid
cross-test pollution and sensitivity to the host's shpool config.

### Dependencies

Minimal by design:

- `anyhow` — error handling
- `libc` — termios raw mode, `ioctl(TIOCGWINSZ)`, `isatty`
- `serde` + `serde_json` — parsing `shpool list --json`

No TUI framework (`ratatui`, `crossterm`, etc.) — the interface is small
enough that hand-rolled ANSI escape sequences are simpler than a
dependency.

## Future directions

- **Session previews.** The cleanest path is a `shpool peek <name>`
  upstream subcommand that prints the daemon's in-memory screen snapshot
  as ANSI. `shpool-manager` could then show a preview pane without
  leaving the CLI-only, shell-out architecture.
- **Resize handling.** Redraw the manager's own menu on `SIGWINCH`
  (currently redraws on next keypress).
