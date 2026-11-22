//! The Wayland client state: globals, the screens, and building their surfaces.

use neko_sprites::Sprite;
use smithay_client_toolkit::compositor::{CompositorState, Region, Surface};
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::registry::RegistryState;
use smithay_client_toolkit::shell::WaylandSurface as _;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, LayerShell, LayerSurface,
};
use smithay_client_toolkit::shm::Shm;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::subcompositor::SubcompositorState;
use wayland_client::protocol::{wl_output, wl_surface};
use wayland_client::{Proxy as _, QueueHandle};

use crate::OverlayConfig;
use crate::geometry::{effective_scale_120, initial_scale_120, layout_bounds};
use crate::idle::IdleMonitor;
use crate::protocol::{
    OwnedFractionalScale, OwnedFractionalScaleManager, OwnedSinglePixelBufferManager,
    OwnedSubsurface, OwnedViewport, OwnedViewporter,
};
use crate::screen::{DrawContext, FrameCache, Screen, ScreenId, ScreenSurface, SpriteSurface};

pub(crate) struct State {
    pub(crate) registry: RegistryState,
    pub(crate) qh: QueueHandle<State>,
    pub(crate) compositor: CompositorState,
    pub(crate) subcompositor: SubcompositorState,
    pub(crate) layer_shell: LayerShell,
    pub(crate) outputs: OutputState,
    pub(crate) shm: Shm,
    pub(crate) pool: SlotPool,
    pub(crate) config: OverlayConfig,
    pub(crate) screens: Vec<Screen>,
    pub(crate) viewporter: Option<OwnedViewporter>,
    pub(crate) fractional_scales: Option<OwnedFractionalScaleManager>,
    pub(crate) single_pixel_buffer_manager: Option<OwnedSinglePixelBufferManager>,
    pub(crate) next_id: u32,
    /// Held so the compositor keeps sending idle events; never read.
    pub(crate) _idle_monitor: Option<IdleMonitor>,
    pub(crate) session_idle: bool,
    pub(crate) closed: bool,
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

    /// Where the compositor puts this output in the global layout, preferring
    /// `xdg_output`'s `logical_position` and falling back to `wl_output`'s
    /// `location`.
    pub(crate) fn position_of(&self, output: &wl_output::WlOutput) -> (i32, i32) {
        self.outputs.info(output).map_or((0, 0), |info| {
            info.logical_position.unwrap_or(info.location)
        })
    }

    /// Builds a surface for an output, unless `--output` rules it out or it
    /// already has one.
    pub(crate) fn add_screen(&mut self, output: &wl_output::WlOutput) {
        if !self.wanted(output) || self.screens.iter().any(|screen| &screen.output == output) {
            return;
        }

        let id = ScreenId(self.next_id);
        self.next_id += 1;

        let surface = self.make_screen_surface(output, id);

        let info = self.outputs.info(output);
        let name = info
            .as_ref()
            .and_then(|info| info.name.clone())
            .unwrap_or_else(|| "?".to_owned());
        let scale_120 = effective_scale_120(
            initial_scale_120(info.as_ref().map(|info| info.scale_factor)),
            surface.viewport.is_some(),
            surface.layer.wl_surface().version(),
        );
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
            name,
            surface: Some(surface),
            scale_120,
            position,
            size: (0, 0),
            hidden: false,
        });
        self.closed = false;
    }

    /// Builds a complete [`ScreenSurface`] (layer, extensions and sprite
    /// subsurface) for one output.
    fn make_screen_surface(&mut self, output: &wl_output::WlOutput, id: ScreenId) -> ScreenSurface {
        let (layer, viewport, fractional_scale) = self.make_layer(output, id);
        let sprite = self.make_sprite(layer.wl_surface(), id);
        ScreenSurface {
            sprite,
            viewport,
            fractional_scale,
            layer,
            backdrop: None,
            configured: false,
            needs_commit: false,
        }
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
        Option<OwnedViewport>,
        Option<OwnedFractionalScale>,
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

        // Create viewport and fractional-scale objects before the initial
        // commit so the compositor can report the preferred scale alongside
        // the first configure instead of forcing an immediate redraw.
        let viewport = self.viewporter.as_ref().map(|viewporter| {
            OwnedViewport(viewporter.0.get_viewport(layer.wl_surface(), &self.qh, id))
        });
        let fractional_scale = viewport.as_ref().and_then(|_| {
            self.fractional_scales.as_ref().map(|manager| {
                OwnedFractionalScale(manager.0.get_fractional_scale(
                    layer.wl_surface(),
                    &self.qh,
                    id,
                ))
            })
        });

        layer.commit();

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
            .map(|viewporter| OwnedViewport(viewporter.0.get_viewport(&surface, &self.qh, id)));
        SpriteSurface {
            viewport,
            subsurface: OwnedSubsurface(subsurface),
            surface: Surface::from(surface),
            buffers: Vec::new(),
            cache: FrameCache::default(),
        }
    }

    /// Gives a previously hidden screen a fresh surface, putting it back on
    /// screen.
    pub(crate) fn rebuild_surface(&mut self, id: ScreenId) {
        let Some(screen) = self.screens.iter().find(|screen| screen.id == id) else {
            return;
        };
        let output = screen.output.clone();
        let surface = self.make_screen_surface(&output, id);
        // Without `wp_fractional_scale_v1`, SCTK starts a new surface at
        // scale 1 and only fires `scale_factor_changed` on a change, so if the
        // output dropped from 2x to 1x while hidden we would stay stuck at 2x.
        // `OutputState` keeps tracking output scale while hidden, so refresh
        // from it here.
        let fallback_scale = surface.fractional_scale.is_none().then(|| {
            effective_scale_120(
                initial_scale_120(self.outputs.info(&output).map(|info| info.scale_factor)),
                surface.viewport.is_some(),
                surface.layer.wl_surface().version(),
            )
        });
        let Some(screen) = self.screen_mut(id) else {
            return;
        };
        screen.surface = Some(surface);
        if let Some(scale_120) = fallback_scale {
            screen.scale_120 = scale_120;
        }
    }

    pub(crate) fn screen_mut(&mut self, id: ScreenId) -> Option<&mut Screen> {
        self.screens.iter_mut().find(|screen| screen.id == id)
    }

    pub(crate) fn all_configured(&self) -> bool {
        !self.screens.is_empty()
            && self
                .screens
                .iter()
                .all(|screen| screen.surface.as_ref().is_some_and(|s| s.configured))
    }

    /// A layout with a hole in it - two monitors of different heights, say -
    /// leaves the animal able to walk into space no output covers, where it is
    /// simply not drawn. That beats the alternative of it being unable to
    /// cross at all.
    pub(crate) fn bounds(&self) -> (i32, i32, i32, i32) {
        layout_bounds(self.screens.iter().map(Screen::rect))
    }

    /// Never fails as a whole: a screen that cannot be drawn on says so in the
    /// log and the others carry on.
    pub(crate) fn draw(&mut self, sprite: &Sprite, x: i32, y: i32) {
        let (origin_x, origin_y, _, _) = self.bounds();
        // Out of the layout-wide space the state machine thinks in and into the
        // compositor's, which is where the outputs are placed.
        let (global_x, global_y) = (origin_x + x, origin_y + y);

        let mut ctx = DrawContext {
            pool: &mut self.pool,
            single_pixel_buffer_manager: self.single_pixel_buffer_manager.as_ref(),
            qh: &self.qh,
            config: &self.config,
        };
        for screen in &mut self.screens {
            let (local_x, local_y) = (global_x - screen.position.0, global_y - screen.position.1);
            // One screen failing - a buffer that could not be allocated, say -
            // is no reason to take the animal off the others.
            if let Err(error) = screen.draw(&mut ctx, sprite, local_x, local_y) {
                log::warn!("drawing on screen {}: {error:#}", screen.id.0);
            }
        }
    }
}
