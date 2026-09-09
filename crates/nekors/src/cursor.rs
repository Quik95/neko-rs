//! Where the cursor is.
//!
//! Nothing here talks to Wayland: the position is pushed in from the outside.
//! A trait rather than a concrete D-Bus type, so an evdev or Hyprland source
//! can be dropped in later without the rest of the program noticing.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

/// The D-Bus name the `KWin` script pushes to.
const SERVICE: &str = "org.nekors.Cursor";
const PATH: &str = "/Cursor";

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

#[zbus::interface(name = "org.nekors.Cursor")]
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
pub struct DbusCursor {
    latest: Arc<Mutex<Option<Latest>>>,
    /// Held so the bus name stays claimed for as long as we run.
    _connection: zbus::blocking::Connection,
}

impl DbusCursor {
    pub fn new() -> Result<Self> {
        let latest = Arc::new(Mutex::new(None));
        let connection = zbus::blocking::connection::Builder::session()
            .context("connect to the session bus")?
            .serve_at(
                PATH,
                CursorService {
                    latest: Arc::clone(&latest),
                },
            )
            .context("serve the cursor object")?
            // Take the name outright rather than queueing behind a stale
            // instance: a queued nekors would sit there receiving nothing.
            .name(SERVICE)
            .with_context(|| format!("claim {SERVICE}"))?
            .build()
            .context("start the D-Bus server")?;

        log::info!("listening on {SERVICE} {PATH}");
        Ok(Self {
            latest,
            _connection: connection,
        })
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
