# rmux-jump

A small Rust TUI for jumping across RMUX sessions or windows.

It talks to the RMUX daemon through the Rust SDK and typed protocol requests,
instead of scraping `rmux` CLI output.

## Usage

```sh
cargo run --release --bin rmux-jump -- --all-windows
```

Controls:

- `h` / `l` (or `←` / `→`, `Tab` / `Shift-Tab`): switch sessions across the top
- `j` / `k` (or `↑` / `↓`): move through windows within the selected session (wraps around)
- `Enter`: switch to the selected RMUX window
- `$`: rename the selected session (Enter saves, Esc cancels)
- `,`: rename the selected window (Enter saves, Esc cancels)
- `x`: kill the selected window (`y` confirms, `n` / `Esc` cancels)
- `X`: kill the selected session (`y` confirms, `n` / `Esc` cancels)
- `r`: refresh now
- `q` / `Esc`: quit

The picker groups every RMUX window under its session. The header shows the
session strip with a cursor; pressing `l` jumps to the next session's active
window. The picker refreshes preview output every 5 seconds while it is open.
`--all-windows` only affects the `--list` printout (it makes `--list` enumerate
every window instead of just each session's active window); the interactive
picker always shows the grouped view.

Useful options:

```sh
rmux-jump --preview-lines 12
rmux-jump --window-lines 10
rmux-jump --refresh-seconds 0
rmux-jump --list --all-windows
```

`--window-lines 10` is a shortcut for showing every RMUX window with the last
ten visible pane lines from the SDK snapshot in the right preview pane.
Use `--refresh-seconds 0` to disable automatic refresh.

## Install

```sh
cargo install --path . --bin rmux-jump --force
```

## RMUX binding

Put one of these in `~/.rmux.conf` or `~/.config/rmux/rmux.conf`, then run
`rmux source-file ~/.rmux.conf`.

Recommended shortcut:

- `prefix + j` opens the all-window picker.
- With RMUX's default prefix, press `Control-B`, then `j`.

Prefix key session picker:

```rmux
bind-key j display-popup -E -w 90% -h 85% "rmux-jump"
```

Prefix key window picker:

```rmux
bind-key j display-popup -E -w 90% -h 85% "rmux-jump --window-lines 10"
```

No-prefix window picker on `Ctrl-Space`:

```rmux
bind-key -n C-Space display-popup -E -w 90% -h 85% "rmux-jump --window-lines 10"
bind-key -n C-@ display-popup -E -w 90% -h 85% "rmux-jump --window-lines 10"
bind-key -n M-Space display-popup -E -w 90% -h 85% "rmux-jump --window-lines 10"
```

To apply the window picker immediately:

```sh
rmux bind-key j display-popup -E -w 90% -h 85% "rmux-jump --window-lines 10"
rmux bind-key -n C-Space display-popup -E -w 90% -h 85% "rmux-jump --window-lines 10"
rmux bind-key -n C-@ display-popup -E -w 90% -h 85% "rmux-jump --window-lines 10"
rmux bind-key -n M-Space display-popup -E -w 90% -h 85% "rmux-jump --window-lines 10"
```
