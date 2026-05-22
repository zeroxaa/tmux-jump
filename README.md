# tmux-jump

A small Rust TUI for jumping across tmux sessions or windows.

## Usage

```sh
cargo run --release -- --all-windows
```

Controls:

- `j` / `k` or arrow keys: move
- `Enter`: switch to the selected tmux target
- `r`: refresh now
- `q` / `Esc`: quit

By default, `tmux-jump` shows one row per session and uses that session's active window.
Use `--all-windows` when you want one row per tmux window. The picker refreshes
preview output every 5 seconds while it is open.

Useful options:

```sh
tmux-jump --preview-lines 12
tmux-jump --all-windows --inline-lines 3
tmux-jump --window-lines 10
tmux-jump --refresh-seconds 0
tmux-jump --list --all-windows
```

`--window-lines 10` is a shortcut for showing every tmux window with the last
ten non-empty pane output lines embedded directly in the target list.
Use `--refresh-seconds 0` to disable automatic refresh.

## Install

```sh
cargo install --path .
```

## tmux binding

Put one of these in `~/.tmux.conf`.

Prefix key session picker:

```tmux
bind-key j display-popup -E -w 90% -h 85% "tmux-jump"
```

Prefix key window picker:

```tmux
bind-key j display-popup -E -w 90% -h 85% "tmux-jump --window-lines 10"
```

No-prefix window picker on `Alt-j`:

```tmux
bind-key -n M-j display-popup -E -w 90% -h 85% "tmux-jump --window-lines 10"
```
