// Feeds the global cursor position to nekors.
//
// A Wayland client only sees pointer motion over its own surface, so nekors
// cannot find the cursor by itself. KWin can: workspace.cursorPos is the real
// global position. KWin scripts cannot export a D-Bus service of their own, but
// they can call one, so the data is pushed outwards rather than polled.
//
// cursorPosChanged is not reliably emitted on Wayland - it is wired up on the
// X11 path - so this polls instead. Polling is cheap as long as we do not talk
// to D-Bus when nothing has moved, which is most of the time.

const SERVICE = "org.nekors.Cursor";
const PATH = "/Cursor";
const INTERFACE = "org.nekors.Cursor";

// ~30 Hz. nekors only thinks eight times a second, but sampling faster keeps
// the position it thinks with fresh.
const INTERVAL_MS = 33;

let lastX = null;
let lastY = null;

const timer = new QTimer();
timer.interval = INTERVAL_MS;
timer.timeout.connect(function () {
    const pos = workspace.cursorPos;
    const x = Math.round(pos.x);
    const y = Math.round(pos.y);

    // The cursor is parked most of the time; sending unchanged positions would
    // be thousands of pointless D-Bus round trips an hour.
    if (x === lastX && y === lastY) {
        return;
    }
    lastX = x;
    lastY = y;

    callDBus(SERVICE, PATH, INTERFACE, "SetPos", x, y);
});
timer.start();

print("nekors: cursor feed started at " + INTERVAL_MS + " ms");
