# pixdrawd

A multiplayer pixel-art editor you draw in **together over SSH**.

```sh
ssh -p 24123 bigbox@bore.pub
```

> Hosted on [Nest](https://hackclub.app). Nest gives each container a public
> **IPv6** address but no public IPv4, so the app is reachable directly at
> `ssh -p 2222 bigbox@2a01:4f9:3080:3192::1987` (IPv6 networks only) and over
> IPv4 through a small `bore` relay at the address above.

No install, no account, no shell — you land straight in the editor. Everyone
connected shares **one canvas**, so you see other people's strokes appear live,
along with their cursors and a count of who's online in the header.

```
+--------------------------------------------------+
| alice  ·  3 online · pixdraw                      |  <- who you are, who's here
| pixdrawd  brush[1]  ■ White           ?          |
+--------------------------------------------------+
|                                                  |
|                  your shared canvas              |
|                                                  |
+--------------------------------------------------+
| Palette: ...                                      |
+--------------------------------------------------+
```

## Controls

- **Mouse** — draw (left drag), erase with a right drag, right-click for the
  context menu, scroll to change colour.
- **Tools** — `i` eyedropper, `e` eraser, `f` flood fill, `m` mirror,
  `p` spray, `b` rainbow, `g` grid, `t` text.
- **Shapes** — `r`/`R` rect, `o`/`O` circle, `l` line, `G` gradient.
- **Selection** — `s` select, `Ctrl+C`/`Ctrl+X`/`Ctrl+V`, arrows to nudge.
- **Colours** — `1`–`9` palette, `Space` next, `u` generate, `Ctrl+N` picker.
- **History** — `Ctrl+Z` undo, `Ctrl+Y` redo.
- **Effects** — `L` game of life (shared — everyone watches it run), `P` posterize.
- **`?`** full searchable help. **`q`** disconnect.

## How it works

- **`russh`** runs the SSH server. Every connection gets its own ratatui
  terminal rendered into the SSH channel; there is no shell to escape to.
- One **authoritative canvas** lives in the server. Each client keeps a local
  replica for fast rendering and, after handling input, diffs it against the
  authoritative copy and broadcasts the changed cells. Per-cell
  last-writer-wins — right for pixel art, and it keeps the hot path lock-light.
- SSH input is turned back into `crossterm` events with **`terminput`** (the
  parser extracted from crossterm), so the editor's input handling is unchanged.
- The **host key** is generated once and persisted; restarting the server does
  not change it, so clients never see the scary
  "REMOTE HOST IDENTIFICATION HAS CHANGED" warning.

## Running it yourself

```sh
cargo build --release
./target/release/pixdrawd --bind 0.0.0.0 --port 2222 --host-key ./host_key
```

Then connect with `ssh -p 2222 <your-host>`. Use a port other than 22 so it does
not collide with the machine's real sshd.

Options:

| flag | default | meaning |
| --- | --- | --- |
| `--bind` | `::` | address to listen on (dual-stack IPv6+IPv4) |
| `--port` | `2222` | port to listen on |
| `--host-key` | `host_key` | persistent host key path (created on first run) |

Theme and palette come from `~/.config/pixdraw/config.toml` (a commented
default is written on first run).

## Development

```sh
cargo test        # input-framing and event-to-draw tests
cargo clippy --all-targets -- -D warnings
```

Tests cover the trickiest part: splitting a raw SSH byte stream into exactly one
terminal event per call (escape sequences, UTF-8, mouse, bracketed paste).
