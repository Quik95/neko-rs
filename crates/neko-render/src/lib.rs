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
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_shm, wl_surface};
use wayland_client::{Connection, EventQueue, QueueHandle};

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
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            layer: Layer::Overlay,
            output: None,
            scale: 1,
            palette: Palette::default(),
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

        let pool = SlotPool::new(1, &shm).context("create the shm pool")?;
        let state = State {
            registry: RegistryState::new(&globals),
            outputs: output_state,
            shm,
            pool,
            config,
            layer,
            size: (0, 0),
            buffer: None,
            last_drawn: None,
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
    /// Reused across frames so undamaged pixels survive; reallocated on resize.
    buffer: Option<Buffer>,
    /// Where the sprite went last frame, so it can be erased.
    last_drawn: Option<Rect>,
    configured: bool,
    closed: bool,
}

impl State {
    /// (Re)allocates the shm buffer after a resize.
    fn ensure_buffer(&mut self) -> Result<()> {
        if self.buffer.is_some() {
            return Ok(());
        }
        let (width, height) = self.size;
        let stride = width * 4;
        let (buffer, canvas) = self
            .pool
            .create_buffer(width, height, stride, wl_shm::Format::Argb8888)
            .context("allocate the shm buffer")?;
        // A fresh slot may hold whatever the last one did; start transparent.
        canvas.fill(0);
        self.buffer = Some(buffer);
        self.last_drawn = None;
        Ok(())
    }

    fn draw(&mut self, sprite: &Sprite, x: i32, y: i32) -> Result<()> {
        if !self.configured || self.size.0 <= 0 || self.size.1 <= 0 {
            return Ok(());
        }
        self.ensure_buffer()?;
        let stride = usize::try_from(self.size.0).expect("non-negative width");
        let scale = i32::try_from(self.config.scale).unwrap_or(1);

        let target = Rect {
            x,
            y,
            width: i32::try_from(sprite.width).expect("sprite fits") * scale,
            height: i32::try_from(sprite.height).expect("sprite fits") * scale,
        };

        // Taken out so the buffer and the pool can be borrowed at once; put
        // back before returning.
        let buffer = self.buffer.take().expect("just ensured");
        let Some(canvas) = buffer.canvas(&mut self.pool) else {
            // Every buffer is still held by the compositor; skip this frame
            // rather than tearing what is on screen.
            log::debug!("shm buffer still in use, skipping a frame");
            self.buffer = Some(buffer);
            return Ok(());
        };
        // The pool hands out bytes; the format is Argb8888, so they are pixels.
        let pixels: &mut [u32] = bytemuck::cast_slice_mut(canvas);

        // Only two rectangles ever change: where the sprite was and where it is
        // going. Repainting a 4K screen eight times a second to move 32 pixels
        // would be silly.
        if let Some(previous) = self.last_drawn {
            clear(pixels, stride, previous);
        }
        let mut canvas = Canvas { pixels, stride };
        sprite.blit_argb(&mut canvas, x, y, self.config.scale, self.config.palette);

        let surface = self.layer.wl_surface();
        for rect in self.last_drawn.into_iter().chain([target]) {
            surface.damage_buffer(rect.x, rect.y, rect.width, rect.height);
        }
        self.last_drawn = Some(target);

        buffer
            .attach_to(surface)
            .context("attach the buffer to the surface")?;
        self.layer.commit();
        self.buffer = Some(buffer);
        Ok(())
    }
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
            // The old buffer is the wrong size now.
            self.buffer = None;
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

delegate_dispatch2!(State);
delegate_registry!(State);
