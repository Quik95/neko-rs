# neko-rs

An [oneko](https://github.com/tie/oneko) clone in Rust that actually works on
Wayland/KWin: the cat chases your cursor across the whole screen instead of
being stuck in a strip on the bottom edge.

## Why it is not just another Wayland oneko

A Wayland client only receives `wl_pointer.motion` while the cursor is over its
own surface; there is no "give me the global cursor position" protocol. Existing
ports work around that by anchoring a full-width strip to the bottom of the
screen, so the cat only ever knows its X coordinate — and that strip eats clicks.

neko-rs splits the two concerns:

* **Drawing** — a fullscreen `zwlr_layer_shell_v1` overlay surface with an
  *empty input region*, so clicks always pass through to whatever is beneath.
* **Cursor position** — pushed in from outside over D-Bus by a small KWin
  script reading `workspace.cursorPos`.

Because the position no longer comes from `wl_pointer`, the input region can
stay empty — and that is what makes a fullscreen overlay harmless.

```text
KWin  --(KWin script, ~30 Hz, callDBus)-->  org.nekors.Cursor.SetPos(x, y)
                                                     |
                                       nekors: FSM -> layer-shell overlay
```

## Layout

| Crate | Role |
| --- | --- |
| `neko-core` | The oneko state machine. No I/O, no Wayland, unit-testable. |
| `neko-sprites` | xbm parsing (in `build.rs`) and the oneko bitmaps. |
| `neko-render` | layer-shell + `wl_shm` + 1-bit blitting. |
| `nekors` | Binary: CLI, D-Bus server, event loop. |
| `kwin-script/` | The KWin script that feeds cursor positions in. |

## Usage

```sh
devenv shell
cargo run -p nekors
```

Install and load the KWin script (needed for the cat to see your cursor):

```sh
install-kwin-script     # copies to ~/.local/share/kwin/scripts/nekors
reload-kwin-script      # (re)loads it in the running KWin without a restart
```

## Licensing

neko-rs is licensed under the EUPL-1.2. The bitmaps and the state machine
it reimplements come from oneko, which is public domain. See
[LICENSE](LICENSE).
