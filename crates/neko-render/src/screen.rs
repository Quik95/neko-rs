//! One output's surfaces: the transparent layer surface covering it and the
//! sprite subsurface moved around on top, plus drawing onto them.

use anyhow::{Context as _, Result};
use neko_sprites::{Canvas, Sprite};
use smithay_client_toolkit::compositor::Surface;
use smithay_client_toolkit::shell::WaylandSurface as _;
use smithay_client_toolkit::shell::wlr_layer::LayerSurface;
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use wayland_client::protocol::{wl_output, wl_shm, wl_surface};
use wayland_client::{Proxy as _, QueueHandle};

use crate::OverlayConfig;
use crate::geometry::{
    Rect, SCALE_DENOMINATOR, effective_scale_120, integer_buffer_scale, round, sprite_geometry,
};
use crate::protocol::{
    OwnedFractionalScale, OwnedSinglePixelBufferManager, OwnedSubsurface, OwnedViewport,
    OwnedWlBuffer, SinglePixelBuffer,
};
use crate::state::State;

/// Identifies a surface across the protocol objects hung off it.
///
/// Fractional-scale and viewport objects arrive with no hint as to which of
/// several outputs they belong to, so they carry this as their user data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScreenId(pub(crate) u32);

#[expect(
    dead_code,
    reason = "variants are held for Drop to keep the attached buffer alive"
)]
pub(crate) enum Backdrop {
    SinglePixel(OwnedWlBuffer),
    Shm(Buffer),
}

/// The live Wayland surfaces and buffers for one visible output.
///
/// Field declaration order is load-bearing for `Drop` (Rust drops fields top to
/// bottom): `sprite` (its viewport, subsurface and surface) and the layer's
/// `viewport` / `fractional_scale` extensions must all be destroyed before
/// `layer` tears down `zwlr_layer_surface_v1` and its underlying `wl_surface`.
pub(crate) struct ScreenSurface {
    pub(crate) sprite: SpriteSurface,
    /// Stretches the 1x1 backdrop over the logical size the surface was given.
    /// `None` on a compositor without `wp_viewporter`, where the backdrop has
    /// to be as big as the output and `wl_surface.set_buffer_scale` handles
    /// integer scales only.
    pub(crate) viewport: Option<OwnedViewport>,
    /// Held so the preferred scale keeps arriving. While there is one, the
    /// coarser integer scale from `wl_surface` is ignored.
    pub(crate) fractional_scale: Option<OwnedFractionalScale>,
    pub(crate) layer: LayerSurface,
    /// The transparent buffer attached to the layer surface. `None` until it
    /// is (re)attached, which the next draw does.
    pub(crate) backdrop: Option<Backdrop>,
    pub(crate) configured: bool,
    /// Set on every `configure` so the next draw commits `ack_configure` even
    /// when the size, backdrop and sprite are unchanged.
    pub(crate) needs_commit: bool,
}

/// One output: its live surface (when not hidden) and where it sits in the
/// layout.
///
/// The layer surface itself only ever holds a transparent backdrop: it is
/// there to cover the output and to be the parent of `sprite`, a subsurface
/// just big enough for the animal that is moved around on top of it. That
/// keeps every buffer that changes a few kilobytes, however big the screen.
pub(crate) struct Screen {
    pub(crate) id: ScreenId,
    pub(crate) output: wl_output::WlOutput,
    /// The compositor's name for this output, e.g. `"HDMI-A-1"`. What the
    /// caller uses to say which screens are covered by a fullscreen window.
    pub(crate) name: String,
    /// `None` while the screen is hidden under a fullscreen window so all
    /// surfaces, extensions and SHM buffers are freed immediately.
    pub(crate) surface: Option<ScreenSurface>,
    /// The scale to draw at, in 120ths: 120 means 1.0. From
    /// `wp_fractional_scale_v1` when there is one, otherwise a whole multiple
    /// of 120 from the integer scale.
    pub(crate) scale_120: u32,
    /// Where this output's top-left sits in the global logical layout.
    pub(crate) position: (i32, i32),
    /// Surface size in logical pixels.
    pub(crate) size: (i32, i32),
    /// Set while something is fullscreen here. The surface is destroyed rather
    /// than drawn transparent: a mapped overlay, however empty, keeps the
    /// compositor from handing the fullscreen window straight to the display
    /// controller, which is the whole cost we are trying to avoid.
    pub(crate) hidden: bool,
}

/// The last frame committed to a screen's sprite subsurface, used to skip
/// redrawing and committing when nothing has changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DrawnFrame {
    sprite: &'static str,
    whole: Rect,
    visible: Rect,
}

/// Tracks what is currently committed on a screen's sprite subsurface so
/// unchanged frames can skip redrawing and committing.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameCache {
    /// Whether a buffer is attached, i.e. the animal is on this output.
    shown: bool,
    last: Option<DrawnFrame>,
}

impl FrameCache {
    fn is_current(&self, frame: DrawnFrame) -> bool {
        self.shown && self.last == Some(frame)
    }

    fn record(&mut self, frame: DrawnFrame) {
        self.shown = true;
        self.last = Some(frame);
    }

    fn clear_shown(&mut self) {
        self.shown = false;
        self.last = None;
    }

    fn invalidate(&mut self) {
        self.last = None;
    }
}

/// The subsurface the animal is drawn on, sized to the part of it that lies on
/// this output.
///
/// Field declaration order is load-bearing for `Drop` (Rust drops fields top to
/// bottom): `viewport` (`wp_viewport`) and `subsurface` (`wl_subsurface`) must
/// be destroyed before `surface` (`wl_surface`) to avoid `no_surface` and
/// `defunct_role_object` protocol errors.
pub(crate) struct SpriteSurface {
    /// Scales the device-pixel buffer to the logical size of the sprite; see
    /// [`ScreenSurface::viewport`].
    pub(crate) viewport: Option<OwnedViewport>,
    pub(crate) subsurface: OwnedSubsurface,
    pub(crate) surface: Surface,
    /// Buffers the compositor may still hold, reused once released. It keeps
    /// the last two it was given, so drawing never waits on a release: another
    /// small buffer is allocated instead.
    pub(crate) buffers: Vec<Buffer>,
    pub(crate) cache: FrameCache,
}

impl SpriteSurface {
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

/// Sprite buffers per screen before a frame is skipped instead. The compositor
/// holds two; anything past that means it has stopped releasing them.
const MAX_SPRITE_BUFFERS: usize = 4;

/// Marks a surface as damaged using `damage_buffer` (`wl_surface` v4+) when
/// supported, falling back to logical `damage` on older compositors.
fn damage_surface(
    surface: &wl_surface::WlSurface,
    buf_w: i32,
    buf_h: i32,
    logical_w: i32,
    logical_h: i32,
) {
    if surface.version() >= 4 {
        surface.damage_buffer(0, 0, buf_w, buf_h);
    } else {
        surface.damage(0, 0, logical_w, logical_h);
    }
}

pub(crate) struct DrawContext<'a> {
    pub(crate) pool: &'a mut SlotPool,
    pub(crate) single_pixel_buffer_manager: Option<&'a OwnedSinglePixelBufferManager>,
    pub(crate) qh: &'a QueueHandle<State>,
    pub(crate) config: &'a OverlayConfig,
}

impl ScreenSurface {
    /// Attaches a transparent buffer to the layer surface, which maps it.
    /// Never replaced until the size or scale changes (and with a viewport,
    /// kept across both).
    fn attach_backdrop(
        &mut self,
        ctx: &mut DrawContext<'_>,
        size: (i32, i32),
        scale_120: u32,
    ) -> Result<()> {
        let surface = self.layer.wl_surface();
        if self.viewport.is_some()
            && let Some(manager) = ctx.single_pixel_buffer_manager
        {
            let buffer = manager
                .0
                .create_u32_rgba_buffer(0, 0, 0, 0, ctx.qh, SinglePixelBuffer);
            surface.attach(Some(&buffer), 0, 0);
            damage_surface(surface, 1, 1, size.0, size.1);
            self.backdrop = Some(Backdrop::SinglePixel(OwnedWlBuffer(buffer)));
            return Ok(());
        }

        let (width, height) = if self.viewport.is_some() {
            (1, 1)
        } else {
            let effective_scale = effective_scale_120(scale_120, false, surface.version());
            let scale_factor = f64::from(effective_scale) / f64::from(SCALE_DENOMINATOR);
            // Rounding up: a buffer a hair too small would leave a gap along
            // the right or bottom edge. Integer scales come out exact.
            let device = |logical: i32| round((f64::from(logical) * scale_factor).ceil());
            (device(size.0), device(size.1))
        };
        let (buffer, canvas) = ctx
            .pool
            .create_buffer(width, height, width * 4, wl_shm::Format::Argb8888)
            .context("allocate the backdrop")?;
        canvas.fill(0);
        buffer
            .attach_to(surface)
            .context("attach the backdrop to the surface")?;
        damage_surface(surface, width, height, size.0, size.1);
        self.backdrop = Some(Backdrop::Shm(buffer));
        Ok(())
    }

    /// Puts the `visible` part of the sprite, which spans `whole`, on the
    /// subsurface. Both are in this output's logical pixels. Returns whether
    /// anything was committed.
    fn draw_sprite(
        &mut self,
        ctx: &mut DrawContext<'_>,
        sprite: &Sprite,
        whole: Rect,
        visible: Rect,
        scale_120: u32,
    ) -> Result<bool> {
        let frame = DrawnFrame {
            sprite: sprite.name,
            whole,
            visible,
        };
        if self.sprite.cache.is_current(frame) {
            return Ok(false);
        }

        let surface_version = self.sprite.surface.wl_surface().version();
        let effective_scale =
            effective_scale_120(scale_120, self.sprite.viewport.is_some(), surface_version);
        // Size the buffer directly from the rectangle dimensions rather than
        // rounded screen edges so moving under a fractional scale does not
        // jitter the buffer size by ±1 device pixel and thrash allocations.
        let geom = sprite_geometry(whole, visible, effective_scale);
        let Some(buffer) = self.sprite.take_buffer(ctx.pool, geom.width, geom.height)? else {
            log::debug!("every sprite buffer still in use, skipping a frame");
            return Ok(false);
        };
        let canvas = buffer.canvas(ctx.pool).expect("a released or fresh buffer");
        canvas.fill(0);
        // The pool hands out bytes; the format is Argb8888, so they are pixels.
        let pixels: &mut [u32] = bytemuck::cast_slice_mut(canvas);
        let stride = usize::try_from(geom.width).expect("positive width");
        sprite.blit_argb(
            &mut Canvas { pixels, stride },
            geom.dst_x,
            geom.dst_y,
            geom.stretch,
            ctx.config.palette,
        );

        let surface = self.sprite.surface.wl_surface();
        buffer
            .attach_to(surface)
            .context("attach the sprite buffer")?;
        damage_surface(
            surface,
            geom.width,
            geom.height,
            visible.width,
            visible.height,
        );
        if let Some(viewport) = &self.sprite.viewport {
            viewport.0.set_destination(visible.width, visible.height);
        } else if surface.version() >= 3 {
            surface.set_buffer_scale(integer_buffer_scale(effective_scale));
        }
        self.sprite.subsurface.0.set_position(visible.x, visible.y);
        surface.commit();
        self.sprite.buffers.push(buffer);
        self.sprite.cache.record(frame);
        Ok(true)
    }
}

impl Screen {
    /// Where this output sits in the global logical layout.
    pub(crate) fn rect(&self) -> Rect {
        Rect {
            x: self.position.0,
            y: self.position.1,
            width: self.size.0,
            height: self.size.1,
        }
    }

    /// Adopts a new scale, in 120ths, from whichever protocol reported it.
    pub(crate) fn set_scale(&mut self, scale_120: u32) {
        let scale_120 = self.surface.as_ref().map_or(scale_120, |surface| {
            effective_scale_120(
                scale_120,
                surface.viewport.is_some(),
                surface.layer.wl_surface().version(),
            )
        });
        if scale_120 == self.scale_120 {
            return;
        }
        log::info!(
            "preferred scale on screen {} is now {:.3}",
            self.id.0,
            f64::from(scale_120) / f64::from(SCALE_DENOMINATOR)
        );
        self.scale_120 = scale_120;
        if let Some(surface) = &mut self.surface {
            if surface.viewport.is_none() {
                surface.backdrop = None;
            }
            surface.sprite.buffers.clear();
            surface.sprite.cache.invalidate();
        }
        self.update_scaling();
    }

    /// Tells the compositor how to fit the backdrop into the logical size the
    /// layer surface was configured at: through the viewport when there is
    /// one, otherwise as an integer buffer scale.
    ///
    /// Both are pending state, applied on the next commit.
    pub(crate) fn update_scaling(&self) {
        let Some(screen_surface) = &self.surface else {
            return;
        };
        // The preferred scale can arrive before the first configure, and a
        // destination of 0x0 is a protocol error rather than a no-op.
        if self.size.0 <= 0 || self.size.1 <= 0 {
            return;
        }
        if let Some(viewport) = &screen_surface.viewport {
            viewport.0.set_destination(self.size.0, self.size.1);
            return;
        }
        let surface = screen_surface.layer.wl_surface();
        // set_buffer_scale arrived in wl_surface version 3.
        if surface.version() >= 3 {
            surface.set_buffer_scale(integer_buffer_scale(self.scale_120));
        }
    }

    /// Draws the sprite at `(x, y)` in this output's own logical pixels.
    ///
    /// The caller has already translated out of the layout-wide space, so an
    /// animal standing on another monitor simply lands outside this surface and
    /// is not shown here - and one straddling the edge shows its own half here.
    pub(crate) fn draw(
        &mut self,
        ctx: &mut DrawContext<'_>,
        sprite: &Sprite,
        x: i32,
        y: i32,
    ) -> Result<()> {
        let scale_120 = self.scale_120;
        let size = self.size;
        // Destroying and rebuilding the surface both happen once, in `hide_outputs`.
        let Some(surface) = &mut self.surface else {
            return Ok(());
        };
        if self.hidden || !surface.configured || size.0 <= 0 || size.1 <= 0 {
            return Ok(());
        }
        let logical = |pixels: u32| i32::try_from(pixels * ctx.config.scale).expect("sprite fits");
        let whole = Rect {
            x,
            y,
            width: logical(sprite.width),
            height: logical(sprite.height),
        };
        let visible = whole.intersect(Rect {
            x: 0,
            y: 0,
            width: size.0,
            height: size.1,
        });

        let mut changed = surface.needs_commit;
        if surface.backdrop.is_none() {
            surface.attach_backdrop(ctx, size, scale_120)?;
            changed = true;
        }
        match visible {
            Some(visible) => {
                changed |= surface.draw_sprite(ctx, sprite, whole, visible, scale_120)?;
            }
            // With several monitors most of them have nothing to show on most
            // frames; committing anyway would wake the compositor for them
            // eight times a second to change nothing.
            None if surface.sprite.cache.shown => {
                let wl_surface = surface.sprite.surface.wl_surface();
                wl_surface.attach(None, 0, 0);
                wl_surface.commit();
                surface.sprite.cache.clear_shown();
                changed = true;
            }
            None => {
                surface.sprite.cache.invalidate();
            }
        }
        // The sprite is a synchronised subsurface: its buffer and position
        // only take effect with this commit, so they always move together.
        if changed {
            surface.layer.commit();
            surface.needs_commit = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{DrawnFrame, FrameCache};
    use crate::geometry::Rect;

    #[test]
    fn frame_cache_skips_unchanged_frames_and_honours_invalidation() {
        let mut cache = FrameCache::default();
        let rect = Rect {
            x: 100,
            y: 200,
            width: 32,
            height: 32,
        };
        let sleep1 = DrawnFrame {
            sprite: "sleep1",
            whole: rect,
            visible: rect,
        };
        let sleep2 = DrawnFrame {
            sprite: "sleep2",
            ..sleep1
        };
        let moved = DrawnFrame {
            whole: Rect { x: 104, ..rect },
            visible: Rect { x: 104, ..rect },
            ..sleep1
        };

        // Nothing drawn yet.
        assert!(!cache.is_current(sleep1));

        // After committing, the same frame is skipped.
        cache.record(sleep1);
        assert!(cache.shown);
        assert!(cache.is_current(sleep1));

        // Different sprite or position requires a redraw.
        assert!(!cache.is_current(sleep2));
        assert!(!cache.is_current(moved));

        // If a frame is skipped due to no free buffer, `record` is not called,
        // so the new frame remains dirty on the next tick.
        assert!(!cache.is_current(sleep2));

        // Scale or size change invalidates the cached frame while keeping
        // `shown` true so leaving the screen still detaches the old buffer.
        cache.invalidate();
        assert!(cache.shown);
        assert!(!cache.is_current(sleep1));

        // Redrawing at the new scale/size caches the frame again.
        cache.record(sleep1);
        assert!(cache.is_current(sleep1));

        // Leaving the screen or unmapping clears both `shown` and the cache;
        // returning to the exact same spot and sprite forces a redraw.
        cache.clear_shown();
        assert!(!cache.shown);
        assert!(!cache.is_current(sleep1));
    }
}
