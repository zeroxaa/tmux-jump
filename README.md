# tmux-jump

A small Rust TUI for jumping across tmux sessions or windows.

## Usage

```sh
cargo run --release -- --all-windows
```

Controls:

- `j` / `k` or arrow keys: move
- `Enter`: switch to the selected tmux target
- `r`: refresh
- `q` / `Esc`: quit

By default, `tmux-jump` shows one row per session and uses that session's active window.
Use `--all-windows` when you want one row per tmux window.

Useful options:

```sh
tmux-jump --preview-lines 12
tmux-jump --all-windows --inline-lines 3
tmux-jump --list --all-windows
```

## Install

```sh
cargo install --path .
```

## tmux binding

Put one of these in `~/.tmux.conf`.

Prefix key session picker:

```tmux
bind-key J display-popup -E -w 90% -h 85% "tmux-jump"
```

Prefix key window picker:

```tmux
bind-key J display-popup -E -w 90% -h 85% "tmux-jump --all-windows"
```

No-prefix window picker on `Alt-j`:

```tmux
bind-key -n M-j display-popup -E -w 90% -h 85% "tmux-jump --all-windows"
```
