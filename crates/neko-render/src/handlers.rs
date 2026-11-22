//! Everything the compositor calls back into: protocol `Dispatch` impls and
//! the SCTK handler traits.

use smithay_client_toolkit::compositor::CompositorHandler;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::WaylandSurface as _;
use smithay_client_toolkit::shell::wlr_layer::{
    LayerShellHandler, LayerSurface, LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};
use wayland_client::protocol::{wl_buffer, wl_output, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::{
    self, WpFractionalScaleManagerV1,
};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{
    self, WpFractionalScaleV1,
};
use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::{
    self, WpSinglePixelBufferManagerV1,
};
use wayland_protocols::wp::viewporter::client::wp_viewport::{self, WpViewport};
use wayland_protocols::wp::viewporter::client::wp_viewporter::{self, WpViewporter};

use crate::geometry::SCALE_DENOMINATOR;
use crate::protocol::SinglePixelBuffer;
use crate::screen::ScreenId;
use crate::state::State;

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
        if scale == 0 {
            return;
        }
        let Some(screen) = state.screen_mut(*id) else {
            return;
        };
        if screen
            .surface
            .as_ref()
            .and_then(|surface| surface.fractional_scale.as_ref())
            .is_some_and(|scale_obj| &scale_obj.0 == proxy)
        {
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

impl Dispatch<WpSinglePixelBufferManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WpSinglePixelBufferManagerV1,
        _event: wp_single_pixel_buffer_manager_v1::Event,
        (): &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // No events.
    }
}

impl Dispatch<wl_buffer::WlBuffer, SinglePixelBuffer> for State {
    fn event(
        _state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        _event: wl_buffer::Event,
        _data: &SinglePixelBuffer,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // Single-pixel buffers are immutable and never overwritten, so
        // wl_buffer.release does not need handling.
    }
}

impl LayerShellHandler for State {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, layer: &LayerSurface) {
        // One monitor going away is not the end: the animal keeps to the
        // others. Only losing the last surface leaves nothing to do.
        self.screens.retain(|screen| {
            screen
                .surface
                .as_ref()
                .is_none_or(|surface| surface.layer.wl_surface() != layer.wl_surface())
        });
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
        let Some(screen) = self.screens.iter_mut().find(|screen| {
            screen
                .surface
                .as_ref()
                .is_some_and(|surface| surface.layer.wl_surface() == layer.wl_surface())
        }) else {
            return;
        };
        let was_configured = screen.surface.as_ref().is_some_and(|s| s.configured);
        let size_changed = size != screen.size;
        if size_changed {
            log::info!("screen {} configured at {}x{}", screen.id.0, size.0, size.1);
            screen.size = size;
            if let Some(surface) = &mut screen.surface
                && surface.viewport.is_none()
            {
                surface.backdrop = None;
            }
        }
        if !was_configured || size_changed {
            screen.update_scaling();
        }
        if let Some(surface) = &mut screen.surface {
            surface.configured = true;
            surface.needs_commit = true;
        }
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
        let Some(screen) = self.screens.iter_mut().find(|screen| {
            screen
                .surface
                .as_ref()
                .is_some_and(|s| s.layer.wl_surface() == surface)
        }) else {
            return;
        };
        if screen
            .surface
            .as_ref()
            .is_some_and(|s| s.fractional_scale.is_some())
        {
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
        let name = self.outputs.info(&output).and_then(|info| info.name);
        if let Some(screen) = self
            .screens
            .iter_mut()
            .find(|screen| screen.output == output)
        {
            if let Some(name) = name
                && screen.name != name
            {
                log::info!("screen {} is now named {name}", screen.id.0);
                screen.name = name;
            }
            if screen.position != position {
                log::info!(
                    "screen {} moved to {},{}",
                    screen.id.0,
                    position.0,
                    position.1
                );
                screen.position = position;
            }
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
