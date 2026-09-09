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

## Multiple monitors

A layer surface covers one output and is never told where that output sits,
while the cursor positions coming over D-Bus are in KWin's global coordinates
spanning every screen. So nekors opens one surface per output, reads the layout
from `xdg_output`, and runs the state machine in the bounding box of the whole
arrangement — the animal walks off one monitor and onto the next, drawn on both
surfaces while it straddles the seam. Monitors plugged in, unplugged, or
rearranged mid-session are picked up as they happen.

Pass `--output eDP-1` to keep it to a single screen.

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
`~/.local/share/kwin/scripts/nekors` and runs nekors as a user service bound to
`graphical-session.target`.

Switching the script on in kwinrc is left to you, because kwinrc is a
whole-file target in Home Manager and the module would fight whatever else
manages it:

```nix
# with plasma-manager
programs.plasma.configFile.kwinrc.Plugins.nekorsEnabled = true;

# or, once:
kwriteconfig6 --file kwinrc --group Plugins --key nekorsEnabled true
```

KWin reads kwinrc at startup, so the first activation needs a relog.

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
| `--output NAME` | Confines it to one monitor instead of all of them. |

Under a fractional scale the overlay draws at device resolution through
`wp_viewporter`, so the sprite stays sharp rather than being scaled up by the
compositor.

## Licensing

neko-rs is licensed under the EUPL-1.2. The bitmaps and the state machine
it reimplements come from oneko, which is public domain. See
[LICENSE](LICENSE).
