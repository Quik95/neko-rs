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

// The other half of the feed: which screens are showing a fullscreen window.
const WINDOWS_PATH = "/Windows";
const WINDOWS_INTERFACE = "org.nekors.Windows";

// ~30 Hz. nekors only thinks eight times a second, but sampling faster keeps
// the position it thinks with fresh.
const INTERVAL_MS = 33;

let lastX = null;
let lastY = null;

const timer = new QTimer();
timer.interval = INTERVAL_MS;
timer.timeout.connect(timer, function () {
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

// Fullscreen reporting.
//
// nekors cannot see anyone else's windows - no Wayland protocol offers that -
// so it is told. Unlike the cursor this is driven by signals: full screen is
// entered a handful of times a day, and polling the window list at 30 Hz to
// watch for it would cost far more than the cat itself.

// How often the current list is sent again even though nothing changed. This
// is what carries the state across a restart of nekors: it comes up knowing
// nothing, and a purely signal-driven feed would leave it that way until the
// next time someone entered or left fullscreen.
const RESYNC_MS = 2000;

let lastOutputs = null;

function report(force) {
    const windows = workspace.windowList();
    const outputs = [];
    for (let i = 0; i < windows.length; i++) {
        const w = windows[i];
        // Any fullscreen window counts, not just the active one: a film on the
        // second monitor stops being active the moment you click back to your
        // work, and that is exactly when the cat must not walk onto it.
        if (!w.fullScreen || w.minimized || !w.output) {
            continue;
        }
        if (w.desktops.length > 0 && w.desktops.indexOf(workspace.currentDesktop) === -1) {
            continue;
        }
        if (w.activities.length > 0 && w.activities.indexOf(workspace.currentActivity) === -1) {
            continue;
        }
        if (outputs.indexOf(w.output.name) === -1) {
            outputs.push(w.output.name);
        }
    }

    // The whole list is recomputed rather than tracked incrementally, so a
    // signal that never arrives - a window dragged between monitors, say - is
    // corrected by the next one instead of leaving a screen dark forever.
    const joined = outputs.join(",");
    if (joined === lastOutputs && !force) {
        return;
    }
    lastOutputs = joined;
    callDBus(SERVICE, WINDOWS_PATH, WINDOWS_INTERFACE, "SetFullscreen", joined);
}

function reportChanged() {
    report(false);
}

function watch(window) {
    window.fullScreenChanged.connect(timer, reportChanged);
    window.outputChanged.connect(timer, reportChanged);
    window.minimizedChanged.connect(timer, reportChanged);
    window.desktopsChanged.connect(timer, reportChanged);
    window.activitiesChanged.connect(timer, reportChanged);
}

const existing = workspace.windowList();
for (let i = 0; i < existing.length; i++) {
    watch(existing[i]);
}
workspace.windowAdded.connect(timer, function (window) {
    watch(window);
    report(false);
});
workspace.windowRemoved.connect(timer, reportChanged);
workspace.currentDesktopChanged.connect(timer, reportChanged);
workspace.currentActivityChanged.connect(timer, reportChanged);
report(true);

// One call every two seconds, whatever happens - far below the cursor feed's
// own traffic, and it makes the whole thing self-healing: whichever side
// restarts, the two agree again within a tick or two.
const resync = new QTimer();
resync.interval = RESYNC_MS;
resync.timeout.connect(timer, function () {
    report(true);
});
resync.start();

print("nekors: cursor feed started at " + INTERVAL_MS + " ms");
