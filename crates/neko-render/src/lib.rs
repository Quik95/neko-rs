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
use smithay_client_toolkit::subcompositor::SubcompositorState;
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};
use wayland_client::globals::{GlobalList, registry_queue_init};
use wayland_client::protocol::{
    wl_output, wl_seat, wl_shm, wl_subsurface::WlSubsurface, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy as _, QueueHandle};
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
    /// The part of this rectangle inside `other`, if any.
    fn intersect(self, other: Self) -> Option<Self> {
        let (left, top) = (self.x.max(other.x), self.y.max(other.y));
        let right = (self.x + self.width).min(other.x + other.width);
        let bottom = (self.y + self.height).min(other.y + other.height);
        (right > left && bottom > top).then_some(Self {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        })
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

        let idle_notification = config
            .idle_after
            .and_then(|after| bind_idle_notification(&globals, &qh, after));

        // Fractional scaling only pays off if the buffer can be handed over at
        // device resolution, which needs a viewport; without one, fall back to
        // the integer scale the compositor reports through wl_surface.
        let viewporter: Option<WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();
        let fractional_scales = viewporter.as_ref().and(
            globals
                .bind::<WpFractionalScaleManagerV1, _, _>(&qh, 1..=1, ())
                .ok(),
        );
        if viewporter.is_none() {
            log::info!("no wp_viewporter; drawing at the output's integer scale");
        } else if fractional_scales.is_none() {
            log::info!("no wp_fractional_scale_v1; drawing at the output's integer scale");
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
            subcompositor,
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

    /// Whether a point in global logical coordinates falls on a screen that is
    /// currently hidden.
    ///
    /// A cursor sitting on a fullscreen window is a cursor the animal cannot
    /// reach: chasing it would leave the cat pressed against the edge of the
    /// neighbouring monitor for the length of the film.
    #[must_use]
    pub fn hidden_at(&self, x: i32, y: i32) -> bool {
        self.state.screens.iter().any(|screen| {
            screen.hidden
                && x >= screen.position.0
                && y >= screen.position.1
                && x < screen.position.0 + screen.size.0
                && y < screen.position.1 + screen.size.1
        })
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

/// One output: its surface, the sprite on it, and where it sits in the layout.
///
/// The layer surface itself only ever holds a transparent backdrop: it is
/// there to cover the output and to be the parent of `sprite`, a subsurface
/// just big enough for the animal that is moved around on top of it. That
/// keeps every buffer that changes a few kilobytes, however big the screen.
struct Screen {
    id: ScreenId,
    output: wl_output::WlOutput,
    /// The compositor's name for this output, e.g. `"HDMI-A-1"`. What the
    /// caller uses to say which screens are covered by a fullscreen window.
    name: String,
    layer: LayerSurface,
    /// Stretches the 1x1 backdrop over the logical size the surface was given.
    /// `None` on a compositor without `wp_viewporter`, where the backdrop has
    /// to be as big as the output and `wl_surface.set_buffer_scale` handles
    /// integer scales only.
    viewport: Option<WpViewport>,
    /// Held so the preferred scale keeps arriving. While there is one, the
    /// coarser integer scale from `wl_surface` is ignored.
    fractional_scale: Option<WpFractionalScaleV1>,
    /// The scale to draw at, in 120ths: 120 means 1.0. From
    /// `wp_fractional_scale_v1` when there is one, otherwise a whole multiple
    /// of 120 from the integer scale.
    scale_120: u32,
    /// Where this output's top-left sits in the global logical layout.
    position: (i32, i32),
    /// Surface size in logical pixels.
    size: (i32, i32),
    /// The transparent buffer attached to the layer surface. `None` until it
    /// is (re)attached, which the next draw does.
    backdrop: Option<Buffer>,
    sprite: SpriteSurface,
    configured: bool,
    /// Set while something is fullscreen here. The surface is unmapped rather
    /// than drawn transparent: a mapped overlay, however empty, keeps the
    /// compositor from handing the fullscreen window straight to the display
    /// controller, which is the whole cost we are trying to avoid.
    hidden: bool,
}

/// The subsurface the animal is drawn on, sized to the part of it that lies on
/// this output.
struct SpriteSurface {
    surface: wl_surface::WlSurface,
    subsurface: WlSubsurface,
    /// Scales the device-pixel buffer to the logical size of the sprite; see
    /// [`Screen::viewport`].
    viewport: Option<WpViewport>,
    /// Buffers the compositor may still hold, reused once released. It keeps
    /// the last two it was given, so drawing never waits on a release: another
    /// small buffer is allocated instead.
    buffers: Vec<Buffer>,
    /// Whether a buffer is attached, i.e. the animal is on this output.
    shown: bool,
}

impl SpriteSurface {
    /// Destroys the protocol objects. The subsurface goes first: destroying
    /// the surface under a live role object is a protocol error.
    fn destroy(&self) {
        if let Some(viewport) = &self.viewport {
            viewport.destroy();
        }
        self.subsurface.destroy();
        self.surface.destroy();
    }

    /// A released buffer of exactly `width` x `height`, or a new one. Released
    /// buffers of another size are dropped on the way. `None` only if the
    /// compositor is sitting on an absurd number of them.
    fn take_buffer(
        &mut self,
        pool: &mut SlotPool,
        width: i32,
        height: i32,
    ) -> Result<Option<Buffer>> {
        let stride = width * 4;
        self.buffers.retain(|buffer| {
            buffer.canvas(pool).is_none()
                || (buffer.height() == height && buffer.stride() == stride)
        });
        if let Some(index) = self
            .buffers
            .iter()
            .position(|buffer| buffer.canvas(pool).is_some())
        {
            return Ok(Some(self.buffers.swap_remove(index)));
        }
        if self.buffers.len() >= MAX_SPRITE_BUFFERS {
            return Ok(None);
        }
        let (buffer, _) = pool
            .create_buffer(width, height, stride, wl_shm::Format::Argb8888)
            .context("allocate the sprite buffer")?;
        Ok(Some(buffer))
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.destroy_extensions();
    }
}

impl Screen {
    /// Destroys everything hung off the layer surface, which has to happen
    /// before the layer surface itself goes.
    fn destroy_extensions(&mut self) {
        self.sprite.destroy();
        if let Some(viewport) = self.viewport.take() {
            viewport.destroy();
        }
        if let Some(scale) = self.fractional_scale.take() {
            scale.destroy();
        }
    }

    /// Drops the buffers sized for the old scale or size. The next draw
    /// attaches fresh ones.
    fn invalidate_buffers(&mut self) {
        self.backdrop = None;
        self.sprite.buffers.clear();
    }

    /// The scale to draw at as a plain number.
    fn scale_factor(&self) -> f64 {
        f64::from(self.scale_120) / f64::from(SCALE_DENOMINATOR)
    }

    /// The integer `buffer_scale` for a compositor without a viewport, where
    /// `scale_120` only ever holds whole multiples of 120.
    fn buffer_scale(&self) -> i32 {
        i32::try_from(self.scale_120 / SCALE_DENOMINATOR)
            .unwrap_or(1)
            .max(1)
    }

    /// A logical coordinate in device pixels.
    fn device(&self, logical: i32) -> i32 {
        round(f64::from(logical) * self.scale_factor())
    }

    /// Adopts a new scale, in 120ths, from whichever protocol reported it.
    fn set_scale(&mut self, scale_120: u32) {
        if scale_120 == self.scale_120 {
            return;
        }
        log::info!(
            "preferred scale on screen {} is now {:.3}",
            self.id.0,
            f64::from(scale_120) / f64::from(SCALE_DENOMINATOR)
        );
        self.scale_120 = scale_120;
        // The buffers are sized in device pixels, so they are the wrong size
        // now.
        self.invalidate_buffers();
        self.update_scaling();
    }

    /// Tells the compositor how to fit the backdrop into the logical size the
    /// layer surface was configured at: through the viewport when there is
    /// one, otherwise as an integer buffer scale.
    ///
    /// Both are pending state, applied with the next backdrop - which is
    /// always a fresh one, since every caller has just invalidated the old one
    /// or is about to receive a configure that does.
    fn update_scaling(&self) {
        // The preferred scale can arrive before the first configure, and a
        // destination of 0x0 is a protocol error rather than a no-op.
        if self.size.0 <= 0 || self.size.1 <= 0 {
            return;
        }
        if let Some(viewport) = &self.viewport {
            viewport.set_destination(self.size.0, self.size.1);
            return;
        }
        let surface = self.layer.wl_surface();
        // set_buffer_scale arrived in wl_surface version 3.
        if surface.version() >= 3 {
            surface.set_buffer_scale(self.buffer_scale());
        }
    }

    /// Attaches a transparent buffer to the layer surface, which maps it.
    /// Never replaced until the size or scale changes, so it never has to
    /// come back.
    fn attach_backdrop(&mut self, pool: &mut SlotPool) -> Result<()> {
        let (width, height) = if self.viewport.is_some() {
            (1, 1)
        } else {
            // Rounding up: a buffer a hair too small would leave a gap along
            // the right or bottom edge. Integer scales come out exact.
            let scale = self.scale_factor();
            let device = |logical: i32| round((f64::from(logical) * scale).ceil());
            (device(self.size.0), device(self.size.1))
        };
        let (buffer, canvas) = pool
            .create_buffer(width, height, width * 4, wl_shm::Format::Argb8888)
            .context("allocate the backdrop")?;
        canvas.fill(0);
        let surface = self.layer.wl_surface();
        buffer
            .attach_to(surface)
            .context("attach the backdrop to the surface")?;
        surface.damage_buffer(0, 0, width, height);
        self.backdrop = Some(buffer);
        Ok(())
    }

    /// Takes the surface off the screen entirely.
    ///
    /// A null buffer unmaps a layer surface, which is what actually lets the
    /// compositor scan the fullscreen window out directly; a transparent
    /// buffer would look the same and cost the same as any other overlay.
    /// The sprite goes with it, being a child of that surface.
    fn unmap(&mut self) {
        let surface = self.layer.wl_surface();
        surface.attach(None, 0, 0);
        surface.commit();
        self.backdrop = None;
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
    /// is not shown here - and one straddling the edge shows its own half here.
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
        let logical = |pixels: u32| i32::try_from(pixels * config.scale).expect("sprite fits");
        let whole = Rect {
            x,
            y,
            width: logical(sprite.width),
            height: logical(sprite.height),
        };
        let visible = whole.intersect(Rect {
            x: 0,
            y: 0,
            width: self.size.0,
            height: self.size.1,
        });

        let mut changed = false;
        if self.backdrop.is_none() {
            self.attach_backdrop(pool)?;
            changed = true;
        }
        match visible {
            Some(visible) => changed |= self.draw_sprite(pool, config, sprite, whole, visible)?,
            // With several monitors most of them have nothing to show on most
            // frames; committing anyway would wake the compositor for them
            // eight times a second to change nothing.
            None if self.sprite.shown => {
                self.sprite.surface.attach(None, 0, 0);
                self.sprite.surface.commit();
                self.sprite.shown = false;
                changed = true;
            }
            None => {}
        }
        // The sprite is a synchronised subsurface: its buffer and position
        // only take effect with this commit, so they always move together.
        if changed {
            self.layer.commit();
        }
        Ok(())
    }

    /// Puts the `visible` part of the sprite, which spans `whole`, on the
    /// subsurface. Both are in this output's logical pixels. Returns whether
    /// anything was committed.
    fn draw_sprite(
        &mut self,
        pool: &mut SlotPool,
        config: &OverlayConfig,
        sprite: &Sprite,
        whole: Rect,
        visible: Rect,
    ) -> Result<bool> {
        // Everything below works in device pixels, taken from where the
        // compositor will put the edges, so the buffer lands on whole pixels
        // unscaled and the 1-bit art stays sharp. The price under a fractional
        // scale is that some of its pixels are one device pixel wider than
        // their neighbours.
        let (left, top) = (self.device(visible.x), self.device(visible.y));
        let width = (self.device(visible.x + visible.width) - left).max(1);
        let height = (self.device(visible.y + visible.height) - top).max(1);
        let Some(buffer) = self.sprite.take_buffer(pool, width, height)? else {
            log::debug!("every sprite buffer still in use, skipping a frame");
            return Ok(false);
        };
        let canvas = buffer.canvas(pool).expect("a released or fresh buffer");
        canvas.fill(0);
        // The pool hands out bytes; the format is Argb8888, so they are pixels.
        let pixels: &mut [u32] = bytemuck::cast_slice_mut(canvas);
        let stride = usize::try_from(width).expect("positive width");
        let stretch = (
            u32::try_from((self.device(whole.x + whole.width) - self.device(whole.x)).max(1))
                .expect("positive width"),
            u32::try_from((self.device(whole.y + whole.height) - self.device(whole.y)).max(1))
                .expect("positive height"),
        );
        sprite.blit_argb(
            &mut Canvas { pixels, stride },
            self.device(whole.x) - left,
            self.device(whole.y) - top,
            stretch,
            config.palette,
        );

        let surface = &self.sprite.surface;
        buffer
            .attach_to(surface)
            .context("attach the sprite buffer")?;
        surface.damage_buffer(0, 0, width, height);
        if let Some(viewport) = &self.sprite.viewport {
            viewport.set_destination(visible.width, visible.height);
        } else if surface.version() >= 3 {
            surface.set_buffer_scale(self.buffer_scale());
        }
        self.sprite.subsurface.set_position(visible.x, visible.y);
        surface.commit();
        self.sprite.buffers.push(buffer);
        self.sprite.shown = true;
        Ok(true)
    }
}

struct State {
    registry: RegistryState,
    qh: QueueHandle<State>,
    compositor: CompositorState,
    subcompositor: SubcompositorState,
    layer_shell: LayerShell,
    outputs: OutputState,
    shm: Shm,
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
        let sprite = self.make_sprite(layer.wl_surface(), id);

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
            backdrop: None,
            sprite,
            configured: false,
            hidden: false,
        });
        self.closed = false;
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

    /// Builds the subsurface the animal is drawn on, above `parent`.
    fn make_sprite(&self, parent: &wl_surface::WlSurface, id: ScreenId) -> SpriteSurface {
        let (subsurface, surface) = self
            .subcompositor
            .create_subsurface(parent.clone(), &self.qh);
        // A subsurface starts out taking input everywhere it covers, so the
        // animal would swallow clicks without this.
        match Region::new(&self.compositor) {
            Ok(region) => surface.set_input_region(Some(region.wl_region())),
            Err(error) => log::error!("no input region ({error}); the animal will eat clicks"),
        }
        let viewport = self
            .viewporter
            .as_ref()
            .map(|viewporter| viewporter.get_viewport(&surface, &self.qh, id));
        SpriteSurface {
            surface,
            subsurface,
            viewport,
            buffers: Vec::new(),
            shown: false,
        }
    }

    /// Replaces a hidden screen's surface with a new one, putting it back on
    /// screen.
    ///
    /// Dropping the old `LayerSurface` destroys the role object and the
    /// `wl_surface` under it, so nothing accumulates there. The sprite, the
    /// viewport and the fractional-scale object are plain protocol proxies
    /// with no such courtesy: they have to be destroyed by hand, or every trip
    /// in and out of fullscreen would leave them behind in the compositor.
    fn rebuild_surface(&mut self, id: ScreenId) {
        let Some(screen) = self.screens.iter().find(|screen| screen.id == id) else {
            return;
        };
        let output = screen.output.clone();
        let (layer, viewport, fractional_scale) = self.make_layer(&output, id);
        let sprite = self.make_sprite(layer.wl_surface(), id);
        let Some(screen) = self.screen_mut(id) else {
            return;
        };
        // Before the surface they hang off goes away with the old layer.
        screen.destroy_extensions();
        screen.layer = layer;
        screen.viewport = viewport;
        screen.fractional_scale = fractional_scale;
        screen.sprite = sprite;
        screen.scale_120 = SCALE_DENOMINATOR;
        screen.update_scaling();
        // Everything below is what the coming configure will fill in again.
        screen.backdrop = None;
        screen.configured = false;
    }

    fn screen_mut(&mut self, id: ScreenId) -> Option<&mut Screen> {
        self.screens.iter_mut().find(|screen| screen.id == id)
    }

    fn any_configured(&self) -> bool {
        self.screens.iter().any(|screen| screen.configured)
    }

    /// A layout with a hole in it - two monitors of different heights, say -
    /// leaves the animal able to walk into space no output covers, where it is
    /// simply not drawn. That beats the alternative of it being unable to
    /// cross at all.
    fn bounds(&self) -> (i32, i32, i32, i32) {
        layout_bounds(self.screens.iter().map(|screen| Rect {
            x: screen.position.0,
            y: screen.position.1,
            width: screen.size.0,
            height: screen.size.1,
        }))
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
        proxy: &WpFractionalScaleV1,
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
        if screen.fractional_scale.as_ref() == Some(proxy) {
            screen.set_scale(scale);
        }
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

fn layout_bounds(screens: impl IntoIterator<Item = Rect>) -> (i32, i32, i32, i32) {
    let mut bounds: Option<(i32, i32, i32, i32)> = None;
    for screen in screens
        .into_iter()
        .filter(|screen| screen.width > 0 && screen.height > 0)
    {
        let (left, top) = (screen.x, screen.y);
        let (right, bottom) = (left + screen.width, top + screen.height);
        bounds = Some(match bounds {
            None => (left, top, right, bottom),
            Some((x, y, r, b)) => (x.min(left), y.min(top), r.max(right), b.max(bottom)),
        });
    }
    let (x, y, right, bottom) = bounds.unwrap_or((0, 0, 0, 0));
    (x, y, right - x, bottom - y)
}

/// Sprite buffers per screen before a frame is skipped instead. The compositor
/// holds two; anything past that means it has stopped releasing them.
const MAX_SPRITE_BUFFERS: usize = 4;

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
            screen.invalidate_buffers();
            screen.update_scaling();
        }
        screen.configured = true;
    }
}

impl CompositorHandler for State {
    /// The integer scale, from `wl_surface.preferred_buffer_scale` or the
    /// outputs the surface is on. Only used where `wp_fractional_scale_v1` is
    /// missing: it is the same scale rounded up, and following it too would
    /// make the two fight.
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        let Some(screen) = self
            .screens
            .iter_mut()
            .find(|screen| screen.layer.wl_surface() == surface)
        else {
            return;
        };
        if screen.fractional_scale.is_some() {
            return;
        }
        let factor = u32::try_from(new_factor).unwrap_or(1).max(1);
        screen.set_scale(factor * SCALE_DENOMINATOR);
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

delegate_dispatch2!(State);
delegate_registry!(State);

#[cfg(test)]
mod tests {
    use super::{Rect, layout_bounds};

    #[test]
    fn layout_retains_geometry_without_surface_configuration() {
        let left = Rect {
            x: -1920,
            y: -200,
            width: 1920,
            height: 1080,
        };
        let right = Rect {
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
        };
        let unknown = Rect {
            x: -9000,
            y: -9000,
            width: 0,
            height: 0,
        };
        assert_eq!(
            layout_bounds([left, right, unknown]),
            (-1920, -200, 4480, 1640)
        );
        assert_eq!(layout_bounds([right]), (0, 0, 2560, 1440));
        assert_eq!(layout_bounds([]), (0, 0, 0, 0));
    }

    #[test]
    fn intersection_keeps_only_the_overlap() {
        let screen = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let straddling = Rect {
            x: 1900,
            y: -10,
            width: 32,
            height: 32,
        };
        assert_eq!(
            straddling.intersect(screen),
            Some(Rect {
                x: 1900,
                y: 0,
                width: 20,
                height: 22,
            })
        );
        let touching = Rect {
            x: 1920,
            ..straddling
        };
        assert_eq!(touching.intersect(screen), None);
    }
}
