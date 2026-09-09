//! The window: a fullscreen `zwlr_layer_shell_v1` overlay that never takes
//! input.
//!
//! The interesting part is [`Overlay::new`] setting an *empty* input region.
//! A fullscreen surface that swallowed clicks would make the desktop unusable,
//! which is exactly why other Wayland ports are stuck with a thin strip along
//! one edge: they need `wl_pointer` events to know where the cursor is. Here
//! the cursor position arrives from outside over D-Bus, so the surface can give
//! up input entirely and cover the whole screen.

use anyhow::{Context as _, Result};
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

pub use smithay_client_toolkit::shell::wlr_layer::Layer;

/// A rectangle in surface-local pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

/// How the overlay should be set up.
#[derive(Debug, Clone)]
pub struct OverlayConfig {
    /// Which layer-shell layer to sit on. `Overlay` puts the animal above
    /// everything including fullscreen windows.
    pub layer: Layer,
    /// The `wl_output` to cover, by name (e.g. `"DP-1"`). `None` lets the
    /// compositor pick.
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

/// A fullscreen click-through surface with one sprite drawn on it.
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
        let output_state = OutputState::new(&globals, &qh);

        let output = match &config.output {
            None => None,
            Some(wanted) => Some(
                output_state
                    .outputs()
                    .find(|output| {
                        output_state
                            .info(output)
                            .and_then(|info| info.name)
                            .is_some_and(|name| &name == wanted)
                    })
                    .with_context(|| format!("no output named {wanted}"))?,
            ),
        };

        let surface = compositor.create_surface(&qh);
        let layer = layer_shell.create_layer_surface(
            &qh,
            surface,
            config.layer,
            Some("nekors"),
            output.as_ref(),
        );

        // Anchoring to all four edges asks for the whole output. The negative
        // exclusive zone means "do not reserve space, and ignore everyone
        // else's reserved space" - panels stay usable and we still cover them.
        layer.set_anchor(Anchor::all());
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);

        // An empty region, not a missing one: no input region at all would mean
        // "the whole surface", which is the opposite of what we want.
        let region = Region::new(&compositor).context("create an empty input region")?;
        layer.set_input_region(Some(region.wl_region()));
        layer.commit();

        let idle_notification = config
            .idle_after
            .and_then(|after| bind_idle_notification(&globals, &qh, after));

        let pool = SlotPool::new(1, &shm).context("create the shm pool")?;
        let state = State {
            _idle_notification: idle_notification,
            session_idle: false,
            registry: RegistryState::new(&globals),
            outputs: output_state,
            shm,
            pool,
            config,
            layer,
            size: (0, 0),
            slots: [Slot::EMPTY, Slot::EMPTY],
            next_slot: 0,
            configured: false,
            closed: false,
        };

        let mut overlay = Self { event_queue, state };

        // The surface is not usable until the compositor has told us how big it
        // is, which it does in response to the commit above.
        while !overlay.state.configured && !overlay.state.closed {
            overlay
                .event_queue
                .blocking_dispatch(&mut overlay.state)
                .context("waiting for the first layer surface configure")?;
        }
        Ok(overlay)
    }

    /// The surface size in logical pixels, which is the coordinate space the
    /// cursor positions and the state machine both work in.
    #[must_use]
    pub fn size(&self) -> (i32, i32) {
        self.state.size
    }

    /// True once the compositor has taken the surface away.
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

    /// Handles pending Wayland events - configures, output changes, closes.
    pub fn dispatch(&mut self) -> Result<()> {
        self.event_queue
            .roundtrip(&mut self.state)
            .context("Wayland roundtrip")?;
        Ok(())
    }

    /// Draws `sprite` with its top-left at `(x, y)` in logical pixels.
    pub fn draw(&mut self, sprite: &Sprite, x: i32, y: i32) -> Result<()> {
        self.state.draw(sprite, x, y)
    }
}

struct State {
    registry: RegistryState,
    outputs: OutputState,
    shm: Shm,
    pool: SlotPool,
    config: OverlayConfig,
    layer: LayerSurface,
    /// Surface size in logical pixels.
    size: (i32, i32),
    /// Two buffers, alternated. A single one would never come back: the
    /// compositor holds an attached buffer until another is attached, so we
    /// would be waiting for a release that only our own next frame can cause.
    slots: [Slot; 2],
    /// Which slot to prefer next frame - the one not currently on screen.
    next_slot: usize,
    /// Held so the compositor keeps sending idle events; never read.
    _idle_notification: Option<ExtIdleNotificationV1>,
    session_idle: bool,
    configured: bool,
    closed: bool,
}

impl State {
    /// (Re)allocates the shm buffers after a resize.
    fn ensure_buffers(&mut self) -> Result<()> {
        let (width, height) = self.size;
        let stride = width * 4;
        for slot in &mut self.slots {
            if slot.buffer.is_some() {
                continue;
            }
            let (buffer, canvas) = self
                .pool
                .create_buffer(width, height, stride, wl_shm::Format::Argb8888)
                .context("allocate the shm buffer")?;
            // A fresh slot may hold whatever the last one did; start transparent.
            canvas.fill(0);
            slot.buffer = Some(buffer);
            slot.last_drawn = None;
        }
        Ok(())
    }

    fn draw(&mut self, sprite: &Sprite, x: i32, y: i32) -> Result<()> {
        if !self.configured || self.size.0 <= 0 || self.size.1 <= 0 {
            return Ok(());
        }
        self.ensure_buffers()?;
        let stride = usize::try_from(self.size.0).expect("non-negative width");
        let scale = i32::try_from(self.config.scale).unwrap_or(1);

        let target = Rect {
            x,
            y,
            width: i32::try_from(sprite.width).expect("sprite fits") * scale,
            height: i32::try_from(sprite.height).expect("sprite fits") * scale,
        };

        // Taken out so the buffer and the pool can be borrowed at once; put
        // back before returning. The preferred slot is the one not on screen,
        // but either will do if that one has not been released yet.
        let mut free = None;
        for index in [self.next_slot, 1 - self.next_slot] {
            let Some(buffer) = self.slots[index].buffer.take() else {
                continue;
            };
            if buffer.canvas(&mut self.pool).is_some() {
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
        let canvas = buffer.canvas(&mut self.pool).expect("just checked");
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
        sprite.blit_argb(&mut canvas, x, y, self.config.scale, self.config.palette);

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
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.closed = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let size = (
            i32::try_from(configure.new_size.0).unwrap_or(i32::MAX),
            i32::try_from(configure.new_size.1).unwrap_or(i32::MAX),
        );
        if size != self.size {
            log::info!("layer surface configured at {}x{}", size.0, size.1);
            self.size = size;
            // The old buffers are the wrong size now.
            self.slots = [Slot::EMPTY, Slot::EMPTY];
        }
        self.configured = true;
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

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
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
