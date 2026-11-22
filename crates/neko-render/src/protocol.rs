//! Owners for protocol objects SCTK does not wrap, destroying them on drop.

use wayland_client::protocol::{wl_buffer, wl_subsurface::WlSubsurface};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::WpFractionalScaleV1;
use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1;
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;

pub(crate) struct OwnedViewporter(pub(crate) WpViewporter);

impl Drop for OwnedViewporter {
    fn drop(&mut self) {
        self.0.destroy();
    }
}

pub(crate) struct OwnedFractionalScaleManager(pub(crate) WpFractionalScaleManagerV1);

impl Drop for OwnedFractionalScaleManager {
    fn drop(&mut self) {
        self.0.destroy();
    }
}

pub(crate) struct OwnedSinglePixelBufferManager(pub(crate) WpSinglePixelBufferManagerV1);

impl Drop for OwnedSinglePixelBufferManager {
    fn drop(&mut self) {
        self.0.destroy();
    }
}

pub(crate) struct SinglePixelBuffer;

pub(crate) struct OwnedWlBuffer(pub(crate) wl_buffer::WlBuffer);

impl Drop for OwnedWlBuffer {
    fn drop(&mut self) {
        self.0.destroy();
    }
}

pub(crate) struct OwnedViewport(pub(crate) WpViewport);

impl Drop for OwnedViewport {
    fn drop(&mut self) {
        self.0.destroy();
    }
}

pub(crate) struct OwnedFractionalScale(pub(crate) WpFractionalScaleV1);

impl Drop for OwnedFractionalScale {
    fn drop(&mut self) {
        self.0.destroy();
    }
}

pub(crate) struct OwnedSubsurface(pub(crate) WlSubsurface);

impl Drop for OwnedSubsurface {
    fn drop(&mut self) {
        self.0.destroy();
    }
}
