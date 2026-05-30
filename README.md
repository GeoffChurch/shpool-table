# shpool-table

A lightweight TUI for managing [shpool] sessions. shpool-table is written in Rust. [shperl] is its counterpart in Perl.

```
                          shpool (3 sessions)
  name       created  active
 >acme -nw   2h       now
  stuxnet    2h       1m
* djt-miner  1d       3h
  j down   k up   spc attach   n new   d kill   D daemon   q quit
```

`shpool-table` is **not** a shpool session itself — it's a standalone
process that owns your terminal, shows your sessions in a navigable list,
and spawns `shpool attach` when you pick one. When you detach from a
session (via shpool's detach keybinding), you land back in the manager's
menu. It requires **zero changes to shpool** — the daemon doesn't know
the manager exists.

[shpool]: https://github.com/shell-pool/shpool
[shperl]: https://github.com/GeoffChurch/shperl

## Installation

Requires [shpool].

```
cargo install --git https://github.com/GeoffChurch/shpool-table
```

To hack on it locally instead:

```
git clone https://github.com/GeoffChurch/shpool-table
cd shpool-table
cargo build --release
# binary at target/release/shpool-table
```

## Usage

```
shpool-table
```

When you attach, `shpool attach <name>` takes over the terminal. Detach
with shpool's keybinding (default `Ctrl-Space Ctrl-q`, configurable in
`~/.config/shpool/config.toml`) and you're back in the manager.

## How it works

shpool-table shells out to the `shpool` CLI for everything — it speaks no
private protocol and needs zero changes to shpool:

- **`shpool events`** — subscribes to shpool's event stream and refreshes
  the table whenever another client creates, kills, attaches, or detaches
  a session, so the list stays live without a keystroke. If the stream is
  unavailable — no daemon yet, an older daemon without the events socket,
  or a dropped subscription — shpool-table says so in the footer and falls
  back to refreshing on keystrokes and terminal focus, then reconnects on
  your next keypress once the stream is available again.
- **`shpool list --json`** — the table contents. The `D` binding runs it
  with `--daemonize`, which forks a daemon first if one isn't running.
- **`shpool attach <name>`** — attach, and create (attaching to a fresh
  name makes it). shpool-table is stricter than shpool here: it refuses to
  create a name that already exists, and gates an attach to a session
  that's live in another terminal behind a force-confirm prompt.
- **`shpool kill <name>`** — kill.

shpool-table's own top-level flags (`--config-file`, `--log-file`,
`--socket`, `-v`) are forwarded to every one of these calls, so e.g.
`shpool-table --socket /tmp/s2` manages the daemon on that socket.
