//! Where the cursor is.
//!
//! Nothing here talks to Wayland: the position is pushed in from the outside.
//! A trait rather than a concrete D-Bus type, so an evdev or Hyprland source
//! can be dropped in later without the rest of the program noticing.

use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

const SERVICE: &str = "io.github.quik95.Nekors";
const PATH: &str = "/io/github/quik95/Nekors/Cursor";
/// Second object on the same name, for what the compositor knows about
/// windows. Kept apart from the cursor so a source that can only supply one of
/// the two does not have to pretend to serve the other.
const WINDOWS_PATH: &str = "/io/github/quik95/Nekors/Windows";

/// Anything that can say which outputs are covered by a fullscreen window.
pub trait FullscreenSource {
    /// The outputs to keep the animal off, by compositor name. An empty slice
    /// means every output is free.
    fn fullscreen_outputs(&self) -> Vec<String>;
}

/// Anything that can say where the cursor is.
pub trait CursorSource {
    /// The last known position, and how long ago it was reported. A source that
    /// has never heard anything returns `None`.
    fn last_position(&self) -> Option<(Position, Duration)>;
}

/// A cursor position in logical screen pixels.
pub type Position = (i32, i32);

#[derive(Debug)]
struct Latest {
    position: Position,
    at: Instant,
}

/// The D-Bus object the `KWin` script calls into.
struct CursorService {
    latest: Arc<Mutex<Option<Latest>>>,
}

/// The other object, for fullscreen reports.
struct WindowsService {
    fullscreen: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "io.github.quik95.Nekors.Windows")]
impl WindowsService {
    /// Reports which outputs currently show a fullscreen window.
    ///
    /// A comma-separated list rather than an array of strings: `callDBus` in a
    /// `KWin` script marshals JavaScript values by guessing, and an empty
    /// array is indistinguishable from no argument at all - which is exactly
    /// the case that has to arrive reliably, since it is the one that gives
    /// the animal its screens back.
    fn set_fullscreen(&self, outputs: &str) {
        let names: Vec<String> = outputs
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        let mut fullscreen = self.fullscreen.lock().expect("fullscreen mutex");
        if *fullscreen != names {
            log::debug!("fullscreen outputs: {names:?}");
            *fullscreen = names;
        }
    }
}

#[zbus::interface(name = "io.github.quik95.Nekors.Cursor")]
impl CursorService {
    /// Reports the global cursor position, in logical screen pixels.
    fn set_pos(&self, x: i32, y: i32) {
        let mut latest = self.latest.lock().expect("cursor mutex");
        *latest = Some(Latest {
            position: (x, y),
            at: Instant::now(),
        });
    }
}

/// A cursor source fed by the `KWin` script over the session bus.
///
/// The connection lives on a thread of its own: zbus only dispatches incoming
/// calls while a thread sits in `zbus::block_on`, and the main thread is busy
/// with the Wayland loop. That thread parks for the rest of the process, so
/// the bus name stays claimed until exit.
pub struct DbusCursor {
    latest: Arc<Mutex<Option<Latest>>>,
    fullscreen: Arc<Mutex<Vec<String>>>,
}

impl DbusCursor {
    pub fn new() -> Result<Self> {
        let latest = Arc::new(Mutex::new(None));
        let fullscreen = Arc::new(Mutex::new(Vec::new()));
        let cursor = CursorService {
            latest: Arc::clone(&latest),
        };
        let windows = WindowsService {
            fullscreen: Arc::clone(&fullscreen),
        };

        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("nekors-dbus".into())
            .spawn(move || {
                zbus::block_on(async move {
                    let built = zbus::connection::Builder::session()
                        .serve_at(PATH, cursor)
                        .serve_at(WINDOWS_PATH, windows)
                        // Take the name outright rather than queueing behind a
                        // stale instance: a queued nekors would sit there
                        // receiving nothing.
                        .name(SERVICE)
                        .build()
                        .await;
                    match built {
                        Ok(_connection) => {
                            // The receiver only goes away if `new` has already
                            // returned, and then nobody is left to tell.
                            let _ = ready_tx.send(Ok(()));
                            std::future::pending::<()>().await;
                        }
                        Err(error) => {
                            let _ = ready_tx.send(Err(error));
                        }
                    }
                });
            })
            .context("spawn the D-Bus thread")?;
        ready_rx
            .recv()
            .context("the D-Bus thread exited without reporting")?
            .with_context(|| format!("serve {SERVICE} on the session bus"))?;

        log::info!("listening on {SERVICE} {PATH} and {WINDOWS_PATH}");
        Ok(Self { latest, fullscreen })
    }
}

impl CursorSource for DbusCursor {
    fn last_position(&self) -> Option<(Position, Duration)> {
        let latest = self.latest.lock().expect("cursor mutex");
        latest
            .as_ref()
            .map(|latest| (latest.position, latest.at.elapsed()))
    }
}

impl FullscreenSource for DbusCursor {
    fn fullscreen_outputs(&self) -> Vec<String> {
        self.fullscreen.lock().expect("fullscreen mutex").clone()
    }
}
