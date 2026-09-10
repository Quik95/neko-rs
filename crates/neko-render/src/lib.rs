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

use anyhow::{Context as _, Result, bail};
use neko_sprites::{Canvas, Palette, Sprite};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::WaylandSurface as _;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    registry_handlers,
};
use wayland_client::globals::{GlobalList, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_seat, wl_shm, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notification_v1::{
    self, ExtIdleNotificationV1,
};
use wayland_protocols::ext::idle_notify::v1::client::ext_idle_notifier_v1::{
    self, ExtIdleNotifierV1,
};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::{
    self, WpFractionalScaleManagerV1,
};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{
    self, WpFractionalScaleV1,
};
use wayland_protocols::wp::viewporter::client::wp_viewport::{self, WpViewport};
use wayland_protocols::wp::viewporter::client::wp_viewporter::{self, WpViewporter};

/// The unit `wp_fractional_scale_v1` reports in: 120ths of a scale factor.
const SCALE_DENOMINATOR: u32 = 120;

/// Rounds a scaled pixel count back to a whole pixel.
///
/// Screen coordinates are a few thousand at the outside and scale factors are
/// small, so nothing here comes close to overflowing.
#[allow(
    clippy::cast_possible_truncation,
    reason = "screen coordinates are far inside i32"
)]
fn round(value: f64) -> i32 {
    value.round() as i32
}

pub use smithay_client_toolkit::shell::wlr_layer::Layer;

/// A rectangle in surface-local pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl Rect {
    /// Whether any pixel of this rectangle lands on a surface `width` x
    /// `height` - i.e. whether that surface has anything to redraw.
    fn hits(self, width: i32, height: i32) -> bool {
        self.x < width && self.y < height && self.x + self.width > 0 && self.y + self.height > 0
    }
}

/// Identifies a surface across the protocol objects hung off it.
///
/// Fractional-scale and viewport objects arrive with no hint as to which of
/// several outputs they belong to, so they carry this as their user data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScreenId(u32);

/// How the overlay should be set up.
#[derive(Debug, Clone)]
pub struct OverlayConfig {
    /// Which layer-shell layer to sit on. `Overlay` puts the animal above
    /// everything including fullscreen windows.
    pub layer: Layer,
    /// The `wl_output` to confine the animal to, by name (e.g. `"DP-1"`).
    /// `None` spans every output, so it can cross from monitor to monitor.
    pub output: Option<String>,
    /// Integer upscaling of the 32x32 sprite.
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
        let layer_shell = LayerShell::bind(&globals, &qh)
            .context("compositor does not offer zwlr_layer_shell_v1")?;
        let shm = Shm::bind(&globals, &qh).context("compositor does not offer wl_shm")?;
        let outputs = OutputState::new(&globals, &qh);

        let idle_notification = config
            .idle_after
            .and_then(|after| bind_idle_notification(&globals, &qh, after));

        // Fractional scaling only pays off if the buffer can be handed over at
        // device resolution, which needs a viewport; without one, ask for
        // nothing and let the compositor scale a logical-sized buffer.
        let viewporter: Option<WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();
        let fractional_scales = viewporter.as_ref().and(
            globals
                .bind::<WpFractionalScaleManagerV1, _, _>(&qh, 1..=1, ())
                .ok(),
        );
        if viewporter.is_none() {
            log::info!("no wp_viewporter; the compositor will scale the overlay itself");
        }

        let pool = SlotPool::new(1, &shm).context("create the shm pool")?;
        let state = State {
            _idle_notification: idle_notification,
            session_idle: false,
            viewporter,
            fractional_scales,
            registry: RegistryState::new(&globals),
            qh: qh.clone(),
            compositor,
            layer_shell,
            outputs,
            shm,
            pool,
            config,
            screens: Vec::new(),
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

        // A surface is not usable until the compositor has told us how big it
        // is, which it does in response to the commits above. One is enough to
        // start drawing; the others join as they are configured.
        while !overlay.state.any_configured() && !overlay.state.closed {
            overlay
                .event_queue
                .blocking_dispatch(&mut overlay.state)
                .context("waiting for the first layer surface configure")?;
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

    /// Handles pending Wayland events - configures, output changes, closes.
    pub fn dispatch(&mut self) -> Result<()> {
        self.event_queue
            .roundtrip(&mut self.state)
            .context("Wayland roundtrip")?;
        Ok(())
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
                screen.unmap();
            } else {
                show.push(screen.id);
            }
        }
        for id in show {
            self.state.rebuild_surface(id);
        }
    }
}

/// One output: its surface, its buffers, and where it sits in the layout.
struct Screen {
    id: ScreenId,
    output: wl_output::WlOutput,
    /// The compositor's name for this output, e.g. `"HDMI-A-1"`. What the
    /// caller uses to say which screens are covered by a fullscreen window.
    name: String,
    layer: LayerSurface,
    /// Scales the buffer down to the logical size the surface was given, so we
    /// can hand over real device pixels. `None` on a compositor without
    /// `wp_viewporter`, where the buffer has to be logical-sized.
    viewport: Option<WpViewport>,
    /// Held so the preferred scale keeps arriving; never read.
    fractional_scale: Option<WpFractionalScaleV1>,
    /// The compositor's preferred scale, in 120ths. 120 means 1.0.
    scale_120: u32,
    /// Where this output's top-left sits in the global logical layout.
    position: (i32, i32),
    /// Surface size in logical pixels.
    size: (i32, i32),
    /// Two buffers, alternated. A single one would never come back: the
    /// compositor holds an attached buffer until another is attached, so we
    /// would be waiting for a release that only our own next frame can cause.
    slots: [Slot; 2],
    /// Which slot to prefer next frame - the one not currently on screen.
    next_slot: usize,
    configured: bool,
    /// Set while something is fullscreen here. The surface is unmapped rather
    /// than drawn transparent: a mapped overlay, however empty, keeps the
    /// compositor from handing the fullscreen window straight to the display
    /// controller, which is the whole cost we are trying to avoid.
    hidden: bool,
}

impl Screen {
    /// The compositor's preferred scale as a plain number.
    fn scale_factor(&self) -> f64 {
        f64::from(self.scale_120) / f64::from(SCALE_DENOMINATOR)
    }

    /// The buffer size in real device pixels, which is what we paint into when
    /// there is a viewport to scale it back down.
    fn device_size(&self) -> (i32, i32) {
        if self.viewport.is_none() {
            return self.size;
        }
        let scale = self.scale_factor();
        let device = |logical: i32| {
            // Rounding up: a buffer a hair too small would leave a gap along
            // the right or bottom edge.
            round((f64::from(logical) * scale).ceil())
        };
        (device(self.size.0), device(self.size.1))
    }

    /// Tells the viewport to squeeze the device-pixel buffer back into the
    /// logical size the layer surface was configured at.
    fn update_viewport(&self) {
        // The preferred scale can arrive before the first configure, and a
        // destination of 0x0 is a protocol error rather than a no-op.
        if self.size.0 <= 0 || self.size.1 <= 0 {
            return;
        }
        if let Some(viewport) = &self.viewport {
            viewport.set_destination(self.size.0, self.size.1);
        }
    }

    /// (Re)allocates the shm buffers after a resize.
    fn ensure_buffers(&mut self, pool: &mut SlotPool) -> Result<()> {
        let (width, height) = self.device_size();
        let stride = width * 4;
        for slot in &mut self.slots {
            if slot.buffer.is_some() {
                continue;
            }
            let (buffer, canvas) = pool
                .create_buffer(width, height, stride, wl_shm::Format::Argb8888)
                .context("allocate the shm buffer")?;
            // A fresh slot may hold whatever the last one did; start transparent.
            canvas.fill(0);
            slot.buffer = Some(buffer);
            slot.last_drawn = None;
        }
        Ok(())
    }

    /// Takes the surface off the screen entirely.
    ///
    /// A null buffer unmaps a layer surface, which is what actually lets the
    /// compositor scan the fullscreen window out directly; a transparent
    /// buffer would look the same and cost the same as any other overlay. The
    /// buffers go with it, both to hand the memory back and so that the frame
    /// after a remap starts from a cleared canvas instead of damaging against
    /// pixels that are no longer on screen.
    fn unmap(&mut self) {
        let surface = self.layer.wl_surface();
        surface.attach(None, 0, 0);
        surface.commit();
        self.slots = [Slot::EMPTY, Slot::EMPTY];
        // An unmapped layer surface is back where it started: attaching a
        // buffer before the compositor has configured it again is a protocol
        // error that kills the connection, so it counts as unconfigured until
        // that configure arrives.
        self.configured = false;
    }

    /// Draws the sprite at `(x, y)` in this output's own logical pixels.
    ///
    /// The caller has already translated out of the layout-wide space, so an
    /// animal standing on another monitor simply lands outside this surface and
    /// is clipped away - and one straddling the edge is drawn half here.
    fn draw(
        &mut self,
        pool: &mut SlotPool,
        config: &OverlayConfig,
        sprite: &Sprite,
        x: i32,
        y: i32,
    ) -> Result<()> {
        // Unmapping and mapping again both happen once, in `hide_outputs`.
        if self.hidden || !self.configured || self.size.0 <= 0 || self.size.1 <= 0 {
            return Ok(());
        }
        self.ensure_buffers(pool)?;
        let (device_width, device_height) = self.device_size();
        let stride = usize::try_from(device_width).expect("non-negative width");

        // Everything below works in device pixels. The sprite is upscaled by a
        // whole number even so - a 1-bit bitmap resampled by 1.25 turns to mush,
        // so the animal ends up a few percent off its requested size instead.
        let factor = self.scale_factor();
        let pixel_scale = u32::try_from(round(f64::from(config.scale) * factor))
            .unwrap_or(1)
            .max(1);
        let scale = i32::try_from(pixel_scale).unwrap_or(1);
        let (x, y) = if self.viewport.is_some() {
            (round(f64::from(x) * factor), round(f64::from(y) * factor))
        } else {
            (x, y)
        };

        let target = Rect {
            x,
            y,
            width: i32::try_from(sprite.width).expect("sprite fits") * scale,
            height: i32::try_from(sprite.height).expect("sprite fits") * scale,
        };

        // With several monitors most of them have nothing to do on most frames:
        // the animal is elsewhere and was elsewhere last frame too. Committing
        // anyway would wake the compositor for every screen eight times a
        // second to change nothing.
        let touched = target.hits(device_width, device_height)
            || self
                .slots
                .iter()
                .filter_map(|slot| slot.last_drawn)
                .any(|rect| rect.hits(device_width, device_height));
        if !touched {
            return Ok(());
        }

        // Taken out so the buffer and the pool can be borrowed at once; put
        // back before returning. The preferred slot is the one not on screen,
        // but either will do if that one has not been released yet.
        let mut free = None;
        for index in [self.next_slot, 1 - self.next_slot] {
            let Some(buffer) = self.slots[index].buffer.take() else {
                continue;
            };
            if buffer.canvas(pool).is_some() {
                free = Some((index, buffer));
                break;
            }
            self.slots[index].buffer = Some(buffer);
        }
        let Some((index, buffer)) = free else {
            // Both buffers are still held by the compositor; skip this frame
            // rather than tearing what is on screen.
            log::debug!("both shm buffers still in use, skipping a frame");
            return Ok(());
        };
        self.next_slot = 1 - index;
        let canvas = buffer.canvas(pool).expect("just checked");
        // The pool hands out bytes; the format is Argb8888, so they are pixels.
        let pixels: &mut [u32] = bytemuck::cast_slice_mut(canvas);

        // Only two rectangles ever change: where the sprite was and where it is
        // going. Repainting a 4K screen eight times a second to move 32 pixels
        // would be silly.
        // This buffer still holds what it showed two frames ago, so both stale
        // rectangles have to go: its own, and the one the visible buffer shows.
        let stale_rects = [
            self.slots[index].last_drawn,
            self.slots[1 - index].last_drawn,
        ];
        for previous in stale_rects.into_iter().flatten() {
            clear(pixels, stride, previous);
        }
        let mut canvas = Canvas { pixels, stride };
        sprite.blit_argb(&mut canvas, x, y, pixel_scale, config.palette);

        let surface = self.layer.wl_surface();
        for rect in stale_rects.into_iter().flatten().chain([target]) {
            surface.damage_buffer(rect.x, rect.y, rect.width, rect.height);
        }
        self.slots[index].last_drawn = Some(target);

        buffer
            .attach_to(surface)
            .context("attach the buffer to the surface")?;
        self.layer.commit();
        self.slots[index].buffer = Some(buffer);
        Ok(())
    }
}

struct State {
    registry: RegistryState,
    qh: QueueHandle<State>,
    compositor: CompositorState,
    layer_shell: LayerShell,
    outputs: OutputState,
    shm: Shm,
    /// One pool behind every surface: the buffers are small and the pool grows
    /// to fit, so there is nothing to gain from one each.
    pool: SlotPool,
    config: OverlayConfig,
    viewporter: Option<WpViewporter>,
    fractional_scales: Option<WpFractionalScaleManagerV1>,
    screens: Vec<Screen>,
    next_id: u32,
    /// Held so the compositor keeps sending idle events; never read.
    _idle_notification: Option<ExtIdleNotificationV1>,
    session_idle: bool,
    closed: bool,
}

impl State {
    /// Whether the animal belongs on this output.
    fn wanted(&self, output: &wl_output::WlOutput) -> bool {
        let Some(wanted) = &self.config.output else {
            return true;
        };
        self.outputs
            .info(output)
            .and_then(|info| info.name)
            .is_some_and(|name| &name == wanted)
    }

    /// Where the compositor puts this output in the global layout, as
    /// `xdg_output` reports it. An output without one is placed at the origin,
    /// which is right for a single monitor and the best guess otherwise.
    fn position_of(&self, output: &wl_output::WlOutput) -> (i32, i32) {
        self.outputs
            .info(output)
            .and_then(|info| info.logical_position)
            .unwrap_or((0, 0))
    }

    /// Builds a surface for an output, unless `--output` rules it out or it
    /// already has one.
    fn add_screen(&mut self, output: &wl_output::WlOutput) {
        if !self.wanted(output) || self.screens.iter().any(|screen| &screen.output == output) {
            return;
        }

        let id = ScreenId(self.next_id);
        self.next_id += 1;

        let (layer, viewport, fractional_scale) = self.make_layer(output, id);

        let name = self
            .outputs
            .info(output)
            .and_then(|info| info.name)
            .unwrap_or_else(|| "?".to_owned());
        let position = self.position_of(output);
        log::info!(
            "covering output {name} at {},{} as screen {}",
            position.0,
            position.1,
            id.0
        );

        self.screens.push(Screen {
            id,
            output: output.clone(),
            name: name.clone(),
            layer,
            viewport,
            fractional_scale,
            scale_120: SCALE_DENOMINATOR,
            position,
            size: (0, 0),
            slots: [Slot::EMPTY, Slot::EMPTY],
            next_slot: 0,
            configured: false,
            hidden: false,
        });
    }

    /// Builds a fresh overlay surface for one output.
    ///
    /// Used both when an output appears and when a screen is shown again after
    /// being hidden: `KWin` does not answer the bare commit that is supposed to
    /// start a new configure round on an unmapped layer surface, so the way
    /// back on screen is a new surface rather than a revived one.
    fn make_layer(
        &mut self,
        output: &wl_output::WlOutput,
        id: ScreenId,
    ) -> (
        LayerSurface,
        Option<WpViewport>,
        Option<WpFractionalScaleV1>,
    ) {
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            self.config.layer,
            Some("nekors"),
            Some(output),
        );

        // Anchoring to all four edges asks for the whole output. The negative
        // exclusive zone means "do not reserve space, and ignore everyone
        // else's reserved space" - panels stay usable and we still cover them.
        layer.set_anchor(Anchor::all());
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);

        // An empty region, not a missing one: no input region at all would mean
        // "the whole surface", which is the opposite of what we want.
        match Region::new(&self.compositor) {
            Ok(region) => layer.set_input_region(Some(region.wl_region())),
            Err(error) => log::error!("no input region ({error}); the overlay will eat clicks"),
        }
        layer.commit();

        let viewport = self
            .viewporter
            .as_ref()
            .map(|viewporter| viewporter.get_viewport(layer.wl_surface(), &self.qh, id));
        let fractional_scale = viewport.as_ref().and(
            self.fractional_scales
                .as_ref()
                .map(|manager| manager.get_fractional_scale(layer.wl_surface(), &self.qh, id)),
        );

        (layer, viewport, fractional_scale)
    }

    /// Replaces a hidden screen's surface with a new one, putting it back on
    /// screen. The old surface goes away with the value it is swapped out of.
    fn rebuild_surface(&mut self, id: ScreenId) {
        let Some(screen) = self.screens.iter().find(|screen| screen.id == id) else {
            return;
        };
        let output = screen.output.clone();
        let (layer, viewport, fractional_scale) = self.make_layer(&output, id);
        let Some(screen) = self.screen_mut(id) else {
            return;
        };
        screen.layer = layer;
        screen.viewport = viewport;
        screen.fractional_scale = fractional_scale;
        // Everything below is what the coming configure will fill in again.
        screen.size = (0, 0);
        screen.slots = [Slot::EMPTY, Slot::EMPTY];
        screen.next_slot = 0;
        screen.configured = false;
    }

    fn screen_mut(&mut self, id: ScreenId) -> Option<&mut Screen> {
        self.screens.iter_mut().find(|screen| screen.id == id)
    }

    fn any_configured(&self) -> bool {
        self.screens.iter().any(|screen| screen.configured)
    }

    /// The bounding box of every configured output, in global logical pixels:
    /// `(x, y, width, height)`.
    ///
    /// A layout with a hole in it - two monitors of different heights, say -
    /// leaves the animal able to walk into space no output covers, where it is
    /// simply not drawn. That beats the alternative of it being unable to
    /// cross at all.
    fn bounds(&self) -> (i32, i32, i32, i32) {
        let mut bounds: Option<(i32, i32, i32, i32)> = None;
        for screen in self.screens.iter().filter(|screen| screen.configured) {
            let (left, top) = screen.position;
            let (right, bottom) = (left + screen.size.0, top + screen.size.1);
            bounds = Some(match bounds {
                None => (left, top, right, bottom),
                Some((x, y, r, b)) => (x.min(left), y.min(top), r.max(right), b.max(bottom)),
            });
        }
        let (x, y, right, bottom) = bounds.unwrap_or((0, 0, 0, 0));
        (x, y, right - x, bottom - y)
    }

    /// Never fails as a whole: a screen that cannot be drawn on says so in the
    /// log and the others carry on.
    fn draw(&mut self, sprite: &Sprite, x: i32, y: i32) {
        let (origin_x, origin_y, _, _) = self.bounds();
        // Out of the layout-wide space the state machine thinks in and into the
        // compositor's, which is where the outputs are placed.
        let (global_x, global_y) = (origin_x + x, origin_y + y);

        // By index, so the pool can be borrowed alongside one screen at a time.
        for index in 0..self.screens.len() {
            let (position, id) = {
                let screen = &self.screens[index];
                (screen.position, screen.id)
            };
            let (local_x, local_y) = (global_x - position.0, global_y - position.1);
            let Some(screen) = self.screens.get_mut(index) else {
                continue;
            };
            // One screen failing - a buffer that could not be allocated, say -
            // is no reason to take the animal off the others.
            if let Err(error) = screen.draw(&mut self.pool, &self.config, sprite, local_x, local_y)
            {
                log::warn!("drawing on screen {}: {error:#}", id.0);
            }
        }
    }
}

/// Asks the compositor to say when the seat goes idle.
///
/// Optional on purpose: `ext_idle_notify_v1` is a staging protocol, and an
/// animal that will not start because the compositor lacks it would be a poor
/// trade for a nicety.
fn bind_idle_notification(
    globals: &GlobalList,
    qh: &QueueHandle<State>,
    after: std::time::Duration,
) -> Option<ExtIdleNotificationV1> {
    let notifier: ExtIdleNotifierV1 = match globals.bind(qh, 1..=2, ()) {
        Ok(notifier) => notifier,
        Err(error) => {
            log::info!("no ext_idle_notify_v1 ({error}); --idle-notify will do nothing");
            return None;
        }
    };
    // The protocol wants a seat, and idleness is per seat. The first one is the
    // one a single-user desktop has.
    let seat: wl_seat::WlSeat = match globals.bind(qh, 1..=9, ()) {
        Ok(seat) => seat,
        Err(error) => {
            log::info!("no wl_seat to watch for idleness ({error})");
            return None;
        }
    };
    let millis = u32::try_from(after.as_millis()).unwrap_or(u32::MAX);
    log::info!("sleeping after {millis} ms of seat idleness");
    Some(notifier.get_idle_notification(millis, &seat, qh, ()))
}

impl Dispatch<WpFractionalScaleV1, ScreenId> for State {
    fn event(
        state: &mut Self,
        _proxy: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        id: &ScreenId,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let wp_fractional_scale_v1::Event::PreferredScale { scale } = event else {
            return;
        };
        let Some(screen) = state.screen_mut(*id) else {
            return;
        };
        if scale == screen.scale_120 {
            return;
        }
        log::info!(
            "preferred scale on screen {} is now {:.3}",
            id.0,
            f64::from(scale) / f64::from(SCALE_DENOMINATOR)
        );
        screen.scale_120 = scale;
        // The buffers are sized in device pixels, so they are the wrong size
        // now.
        screen.slots = [Slot::EMPTY, Slot::EMPTY];
        screen.update_viewport();
    }
}

impl Dispatch<WpViewporter, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewporter,
        _event: wp_viewporter::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // No events.
    }
}

impl Dispatch<WpViewport, ScreenId> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewport,
        _event: wp_viewport::Event,
        _id: &ScreenId,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // No events.
    }
}

impl Dispatch<WpFractionalScaleManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WpFractionalScaleManagerV1,
        _event: wp_fractional_scale_manager_v1::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // No events.
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ExtIdleNotifierV1,
        _event: ext_idle_notifier_v1::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // The notifier itself has no events.
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_seat::WlSeat,
        _event: wl_seat::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Capabilities and the seat name are of no interest: the surface takes
        // no input, the seat is here only to hang the idle notification on.
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_idle_notification_v1::Event::Idled => {
                log::debug!("the seat went idle");
                state.session_idle = true;
            }
            ext_idle_notification_v1::Event::Resumed => {
                log::debug!("the seat came back");
                state.session_idle = false;
            }
            _ => {}
        }
    }
}

/// One of the two buffers, and where the sprite went on it.
struct Slot {
    buffer: Option<Buffer>,
    last_drawn: Option<Rect>,
}

impl Slot {
    const EMPTY: Self = Self {
        buffer: None,
        last_drawn: None,
    };
}

fn clear(pixels: &mut [u32], stride: usize, rect: Rect) {
    let rows = pixels.len() / stride;
    for y in rect.y.max(0)..(rect.y + rect.height).max(0) {
        let Ok(y) = usize::try_from(y) else { continue };
        if y >= rows {
            break;
        }
        let start = rect.x.max(0);
        let end = (rect.x + rect.width).max(0);
        for x in start..end {
            let Ok(x) = usize::try_from(x) else { continue };
            if x >= stride {
                break;
            }
            pixels[y * stride + x] = 0;
        }
    }
}

impl LayerShellHandler for State {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        // One monitor going away is not the end: the animal keeps to the
        // others. Only losing the last surface leaves nothing to do.
        self.screens
            .retain(|screen| screen.layer.wl_surface() != layer.wl_surface());
        if self.screens.is_empty() {
            self.closed = true;
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let size = (
            i32::try_from(configure.new_size.0).unwrap_or(i32::MAX),
            i32::try_from(configure.new_size.1).unwrap_or(i32::MAX),
        );
        let Some(screen) = self
            .screens
            .iter_mut()
            .find(|screen| screen.layer.wl_surface() == layer.wl_surface())
        else {
            return;
        };
        if size != screen.size {
            log::info!("screen {} configured at {}x{}", screen.id.0, size.0, size.1);
            screen.size = size;
            // The old buffers are the wrong size now.
            screen.slots = [Slot::EMPTY, Slot::EMPTY];
            screen.update_viewport();
        }
        screen.configured = true;
    }
}

impl CompositorHandler for State {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.add_screen(&output);
    }

    /// Monitors get rearranged - a laptop docked to the left of an external
    /// screen one day and to the right the next - so the layout is re-read
    /// rather than trusted from startup.
    fn update_output(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        // An output whose name only turned up now may be the one --output asks
        // for, and one that was disabled at startup comes back this way too.
        self.add_screen(&output);

        let position = self.position_of(&output);
        if let Some(screen) = self
            .screens
            .iter_mut()
            .find(|screen| screen.output == output)
            && screen.position != position
        {
            log::info!(
                "screen {} moved to {},{}",
                screen.id.0,
                position.0,
                position.1
            );
            screen.position = position;
        }
    }

    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.screens.retain(|screen| screen.output != output);
        if self.screens.is_empty() {
            self.closed = true;
        }
    }
}

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }

    registry_handlers![OutputState];
}

delegate_compositor!(State);
delegate_output!(State);
delegate_shm!(State);
delegate_layer!(State);
delegate_registry!(State);
