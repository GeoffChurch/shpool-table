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
