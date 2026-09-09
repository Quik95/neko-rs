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

## Installing with Nix

The flake ships the package and a Home Manager module:

```nix
{
  inputs.neko-rs.url = "github:quik95/neko-rs";

  # in your Home Manager configuration
  imports = [inputs.neko-rs.homeModules.nekors];

  services.nekors = {
    enable = true;
    extraArgs = ["--scale" "2"];
  };
}
```

The module installs the package, links the KWin script into
`~/.local/share/kwin/scripts/nekors`, writes the kwinrc entry that enables it,
and runs nekors as a user service bound to `graphical-session.target`. KWin
reads kwinrc at startup, so the first activation needs a relog.

## Flags worth knowing

Full list in `nekors --help` and `nekors(1)`; the less obvious ones:

| Flag | Effect |
| --- | --- |
| `--scale N` | Multiplies the 32×32 sprites; 2 is right for a HiDPI screen. |
| `--speed N` | Pixels per tick (the original moves 16). |
| `--static` | Ignores the cursor and lets the animal idle in place. |
| `--sleepiness N` | How quickly boredom turns into sleep. |
| `--sleepiness-night N` | Overrides `--sleepiness` between 22:00 and 06:00. |
| `--idle-notify SECS` | Sleeps once the seat is idle for this long (`ext_idle_notify_v1`). |
| `--type NAME` | oneko's alternative animals. |

Under a fractional scale the overlay draws at device resolution through
`wp_viewporter`, so the sprite stays sharp rather than being scaled up by the
compositor.

## Licensing

neko-rs is licensed under the EUPL-1.2. The bitmaps and the state machine
it reimplements come from oneko, which is public domain. See
[LICENSE](LICENSE).
