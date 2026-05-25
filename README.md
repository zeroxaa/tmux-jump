# tmux-jump

A small Rust TUI for jumping across tmux sessions or windows.

## Usage

```sh
cargo run --release -- --all-windows
```

Controls:

- `j` / `k` (or `↑` / `↓`): move through every window of every session in one flat list (wraps around)
- `h` / `l` (or `←` / `→`, `Tab` / `Shift-Tab`): jump to the previous / next session
- `Enter`: switch to the selected tmux window
- `$`: rename the selected session (Enter saves, Esc cancels)
- `,`: rename the selected window (Enter saves, Esc cancels)
- `x`: kill the selected window (`y` confirms, `n` / `Esc` cancels)
- `X`: kill the selected session (`y` confirms, `n` / `Esc` cancels)
- `r`: refresh now
- `q` / `Esc`: quit

The picker lists every tmux window in one vertical list, grouped under a header
for each session. `j` / `k` walk through the whole list (crossing session
boundaries and wrapping at the ends), while `h` / `l` jump straight to the
previous / next session's active window. The picker refreshes preview output
every 5 seconds while it is open.
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
