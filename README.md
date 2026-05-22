# tmux-jump

A small Rust TUI for jumping across tmux sessions or windows.

## Usage

```sh
cargo run --release -- --all-windows
```

Controls:

- `h` / `l` (or `←` / `→`, `Tab` / `Shift-Tab`): switch sessions across the top
- `j` / `k` (or `↑` / `↓`): move through windows within the selected session
- `Enter`: switch to the selected tmux window
- `r`: refresh now
- `q` / `Esc`: quit

The picker groups every tmux window under its session. The header shows the
session strip with a cursor; pressing `l` jumps to the next session's active
window. The picker refreshes preview output every 5 seconds while it is open.
`--all-windows` only affects the `--list` printout (it makes `--list` enumerate
every window instead of just each session's active window); the interactive
picker always shows the grouped view.

Useful options:

```sh
tmux-jump --preview-lines 12
tmux-jump --window-lines 10
tmux-jump --refresh-seconds 0
tmux-jump --list --all-windows
```

`--window-lines 10` is a shortcut for showing every tmux window with the last
ten captured pane output lines in the right preview pane.
Use `--refresh-seconds 0` to disable automatic refresh.

## Install

```sh
cargo install --path .
```

## tmux binding

Put one of these in `~/.tmux.conf`.

Recommended shortcut:

- `prefix + j` opens the all-window picker.
- With tmux's default prefix, press `Control-B`, then `j`.

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
