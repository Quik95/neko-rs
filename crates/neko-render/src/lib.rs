//! The window: one `zwlr_layer_shell_v1` overlay per output, none of which ever
//! takes input.
//!
//! The interesting part is [`Overlay::new`] setting an *empty* input region.
//! A fullscreen surface that swallowed clicks would make the desktop unusable,
//! which is exactly why other Wayland ports are stuck with a thin strip along
//! one edge: they need `wl_pointer` events to know where the cursor is. Here
//! the cursor position arrives from outside over D-Bus, so the surface can give
//! up input entirely and cover the whole screen.
//!
//! # Multiple monitors
//!
//! A layer surface covers exactly one output and is told nothing about where
//! that output sits, but the cursor positions arriving over D-Bus are in the
//! compositor's *global* coordinate space, which spans every monitor. So the
//! overlay keeps one surface per output, learns each output's place in that
//! space from `xdg_output`, and works in one coordinate system: the bounding
//! box of the whole layout, with the origin at its top-left
//! ([`Overlay::origin`], [`Overlay::size`]). A sprite lying across a monitor
//! edge is drawn on both surfaces, each clipping its own half, so the animal
//! walks from screen to screen instead of stopping at the seam.

mod geometry;
mod handlers;
mod idle;
mod protocol;
mod screen;
mod state;

use std::io;

use anyhow::{Context as _, Result, bail};
use neko_sprites::{Palette, Sprite};
use smithay_client_toolkit::compositor::CompositorState;
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::registry::RegistryState;
use smithay_client_toolkit::shell::wlr_layer::LayerShell;
use smithay_client_toolkit::shm::Shm;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::subcompositor::SubcompositorState;
use wayland_client::backend::WaylandError;
use wayland_client::globals::registry_queue_init;
use wayland_client::{Connection, EventQueue};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1;

use crate::geometry::point_on_hidden_screen;
use crate::idle::bind_idle_notification;
use crate::protocol::{
    OwnedFractionalScaleManager, OwnedSinglePixelBufferManager, OwnedViewporter,
};
use crate::state::State;

/// Initial size of the SHM pool in bytes, large enough for typical sprite
/// buffers without immediate `ftruncate` / `wl_shm_pool.resize` churn.
const INITIAL_SHM_POOL_SIZE: usize = 64 * 1024;

/// Maximum number of Wayland roundtrips to wait for all initial layer surfaces
/// to configure before starting with whichever ones have already responded.
const INITIAL_CONFIGURE_ROUNDTRIPS: usize = 3;

pub use smithay_client_toolkit::shell::wlr_layer::Layer;

/// How the overlay should be set up.
#[derive(Debug, Clone)]
pub struct OverlayConfig {
    /// Which layer-shell layer to sit on. `Overlay` puts the animal above
    /// everything including fullscreen windows.
    pub layer: Layer,
    /// The `wl_output` to confine the animal to, by name (e.g. `"DP-1"`).
    /// `None` spans every output, so it can cross from monitor to monitor.
    pub output: Option<String>,
    /// Integer upscaling of the 32x32 sprite, in logical pixels: the output's
    /// own scale comes on top.
    pub scale: u32,
    pub palette: Palette,
    /// How long the seat must be idle before [`Overlay::session_idle`] turns
    /// true. `None` skips `ext_idle_notify_v1` entirely.
    pub idle_after: Option<std::time::Duration>,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            layer: Layer::Overlay,
            output: None,
            scale: 1,
            palette: Palette::default(),
            idle_after: None,
        }
    }
}

/// Click-through surfaces covering every monitor, with one sprite drawn across
/// them.
pub struct Overlay {
    event_queue: EventQueue<State>,
    state: State,
}

impl Overlay {
    pub fn new(config: OverlayConfig) -> Result<Self> {
        let connection =
            Connection::connect_to_env().context("no Wayland compositor to connect to")?;
        let (globals, event_queue) =
            registry_queue_init(&connection).context("Wayland registry")?;
        let qh = event_queue.handle();

        let compositor = CompositorState::bind(&globals, &qh)
            .context("compositor does not offer wl_compositor")?;
        let subcompositor =
            SubcompositorState::bind(compositor.wl_compositor().clone(), &globals, &qh)
                .context("compositor does not offer wl_subcompositor")?;
        let layer_shell = LayerShell::bind(&globals, &qh)
            .context("compositor does not offer zwlr_layer_shell_v1")?;
        let shm = Shm::bind(&globals, &qh).context("compositor does not offer wl_shm")?;
        let outputs = OutputState::new(&globals, &qh);

        let idle_monitor = config
            .idle_after
            .and_then(|after| bind_idle_notification(&globals, &qh, after));

        // Fractional scaling only pays off if the buffer can be handed over at
        // device resolution, which needs a viewport; without one, fall back to
        // the integer scale the compositor reports through wl_surface.
        let viewporter: Option<OwnedViewporter> =
            globals.bind(&qh, 1..=1, ()).ok().map(OwnedViewporter);
        let fractional_scales = viewporter.as_ref().and_then(|_| {
            globals
                .bind::<WpFractionalScaleManagerV1, _, _>(&qh, 1..=1, ())
                .ok()
                .map(OwnedFractionalScaleManager)
        });
        let single_pixel_buffer_manager = viewporter.as_ref().and_then(|_| {
            globals
                .bind::<WpSinglePixelBufferManagerV1, _, _>(&qh, 1..=1, ())
                .ok()
                .map(OwnedSinglePixelBufferManager)
        });
        if viewporter.is_none() {
            log::info!("no wp_viewporter; drawing at the output's integer scale");
        } else if fractional_scales.is_none() {
            log::info!("no wp_fractional_scale_v1; drawing at the output's integer scale");
        }

        let pool = SlotPool::new(INITIAL_SHM_POOL_SIZE, &shm).context("create the shm pool")?;
        let state = State {
            _idle_monitor: idle_monitor,
            session_idle: false,
            screens: Vec::new(),
            viewporter,
            fractional_scales,
            single_pixel_buffer_manager,
            registry: RegistryState::new(&globals),
            qh: qh.clone(),
            compositor,
            subcompositor,
            layer_shell,
            outputs,
            shm,
            pool,
            config,
            next_id: 0,
            closed: false,
        };

        let mut overlay = Self { event_queue, state };

        // Registry initialisation announces the outputs but not their names or
        // their places in the layout: those arrive as wl_output and xdg_output
        // events on the next round. Building surfaces before they land would
        // stack every monitor at the origin and lose the layout entirely.
        overlay
            .event_queue
            .roundtrip(&mut overlay.state)
            .context("waiting for the output layout")?;

        // The outputs the compositor has now; anything plugged in later arrives
        // through OutputHandler::new_output.
        let known: Vec<_> = overlay.state.outputs.outputs().collect();
        for output in &known {
            overlay.state.add_screen(output);
        }
        if overlay.state.screens.is_empty() {
            match &overlay.state.config.output {
                Some(wanted) => bail!("no output named {wanted}"),
                None => bail!("the compositor reports no outputs"),
            }
        }

        // Wait for every initial surface to configure so `origin()` and
        // `size()` cover the full monitor layout from the very first frame.
        // Bounded to a few roundtrips so a compositor that never answers one
        // surface cannot hang startup; late configures still join in `dispatch`.
        for _ in 0..INITIAL_CONFIGURE_ROUNDTRIPS {
            if overlay.state.all_configured() || overlay.state.closed {
                break;
            }
            overlay
                .event_queue
                .roundtrip(&mut overlay.state)
                .context("waiting for layer surfaces to configure")?;
        }
        if !overlay.state.all_configured() && !overlay.state.closed {
            let configured = overlay
                .state
                .screens
                .iter()
                .filter(|screen| screen.surface.as_ref().is_some_and(|s| s.configured))
                .count();
            log::warn!(
                "only {configured}/{} layer surface(s) configured after \
                 {INITIAL_CONFIGURE_ROUNDTRIPS} roundtrips; continuing anyway",
                overlay.state.screens.len(),
            );
        }
        Ok(overlay)
    }

    /// The top-left of the monitor layout in the compositor's global logical
    /// coordinates - the space cursor positions arrive in.
    ///
    /// Subtract it from a global cursor position to get a coordinate in the
    /// space [`Overlay::size`] and [`Overlay::draw`] work in.
    #[must_use]
    pub fn origin(&self) -> (i32, i32) {
        let (x, y, _, _) = self.state.bounds();
        (x, y)
    }

    /// The size of the whole monitor layout in logical pixels, which is the
    /// coordinate space the state machine works in.
    #[must_use]
    pub fn size(&self) -> (i32, i32) {
        let (_, _, width, height) = self.state.bounds();
        (width, height)
    }

    /// Whether a point in global logical coordinates falls on a screen that is
    /// currently hidden.
    ///
    /// A cursor sitting on a fullscreen window is a cursor the animal cannot
    /// reach: chasing it would leave the cat pressed against the edge of the
    /// neighbouring monitor for the length of the film.
    #[must_use]
    pub fn hidden_at(&self, x: i32, y: i32) -> bool {
        point_on_hidden_screen(
            self.state
                .screens
                .iter()
                .map(|screen| (screen.hidden, screen.rect())),
            x,
            y,
        )
    }

    /// True once the compositor has taken every surface away.
    #[must_use]
    pub fn closed(&self) -> bool {
        self.state.closed
    }

    /// True while the seat has been idle for longer than `idle_after`. Always
    /// false when that was `None`, or when the compositor has no
    /// `ext_idle_notify_v1`.
    #[must_use]
    pub fn session_idle(&self) -> bool {
        self.state.session_idle
    }

    /// How many outputs the animal is currently spread across.
    #[must_use]
    pub fn screen_count(&self) -> usize {
        self.state.screens.len()
    }

    /// Handles pending Wayland events without blocking - configures, output
    /// changes, closes.
    pub fn dispatch(&mut self) -> Result<()> {
        self.event_queue
            .dispatch_pending(&mut self.state)
            .context("Wayland dispatch")?;
        if let Some(guard) = self.event_queue.prepare_read() {
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error).context("Wayland read"),
            }
        }
        self.event_queue
            .dispatch_pending(&mut self.state)
            .context("Wayland dispatch")?;
        Ok(())
    }

    /// Sends any queued Wayland requests to the compositor without waiting for a reply.
    pub fn flush(&mut self) -> Result<()> {
        match self.event_queue.flush() {
            Ok(()) => Ok(()),
            Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {
                log::debug!("Wayland socket buffer full, deferring flush");
                Ok(())
            }
            Err(error) => Err(error).context("Wayland flush"),
        }
    }

    /// Draws `sprite` with its top-left at `(x, y)`, in logical pixels relative
    /// to [`Overlay::origin`].
    pub fn draw(&mut self, sprite: &Sprite, x: i32, y: i32) {
        self.state.draw(sprite, x, y);
    }

    /// Takes the animal off the outputs named in `names` and puts it back on
    /// every other one.
    ///
    /// Meant for outputs showing a fullscreen window. Nothing here can tell
    /// that by itself - a Wayland client is told nothing about anyone else's
    /// windows - so the caller supplies the names; unknown ones are ignored,
    /// which is what makes a stale report from a monitor that has since been
    /// unplugged harmless.
    pub fn hide_outputs<S: AsRef<str>>(&mut self, names: &[S]) {
        // Collected first: showing a screen again rebuilds its surface, which
        // needs the globals on State while the screens are borrowed.
        let mut show = Vec::new();
        for screen in &mut self.state.screens {
            let hidden = names.iter().any(|name| name.as_ref() == screen.name);
            if hidden == screen.hidden {
                continue;
            }
            log::debug!(
                "screen {} ({}) is now {}",
                screen.id.0,
                screen.name,
                if hidden { "hidden" } else { "visible" }
            );
            screen.hidden = hidden;
            if hidden {
                // Dropping the surface destroys the subsurface, extensions,
                // layer surface and SHM buffers immediately, unmapping the
                // overlay so the compositor can scan out the fullscreen window
                // directly.
                screen.surface = None;
            } else {
                show.push(screen.id);
            }
        }
        for id in show {
            self.state.rebuild_surface(id);
        }
    }
}
