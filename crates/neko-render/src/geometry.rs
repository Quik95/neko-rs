//! Pixel and scale arithmetic: rectangles, the layout bounding box, and the
//! conversions between logical and device pixels. No Wayland objects here.

/// The unit `wp_fractional_scale_v1` reports in: 120ths of a scale factor.
pub(crate) const SCALE_DENOMINATOR: u32 = 120;

/// Rounds a scaled pixel count back to a whole pixel.
///
/// Screen coordinates are a few thousand at the outside and scale factors are
/// small, so nothing here comes close to overflowing.
#[allow(
    clippy::cast_possible_truncation,
    reason = "screen coordinates are far inside i32"
)]
pub(crate) fn round(value: f64) -> i32 {
    value.round() as i32
}

/// A rectangle in surface-local pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rect {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

impl Rect {
    /// The part of this rectangle inside `other`, if any.
    pub(crate) fn intersect(self, other: Self) -> Option<Self> {
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

/// Device-pixel dimensions and blit parameters for a sprite buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SpriteGeometry {
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) dst_x: i32,
    pub(crate) dst_y: i32,
    pub(crate) stretch: (u32, u32),
}

/// Converts a logical pixel length or offset to device pixels at `scale_120`.
fn device_pixels(logical: i32, scale_120: u32) -> i32 {
    let scale = f64::from(scale_120) / f64::from(SCALE_DENOMINATOR);
    round(f64::from(logical) * scale)
}

/// Computes the sprite buffer size, stretch target, and blit offset in device
/// pixels directly from rectangle dimensions rather than rounded edges, so the
/// buffer size stays constant as the sprite moves under a fractional scale.
pub(crate) fn sprite_geometry(whole: Rect, visible: Rect, scale_120: u32) -> SpriteGeometry {
    let device = |logical: i32| device_pixels(logical, scale_120);
    let width = device(visible.width).max(1);
    let height = device(visible.height).max(1);
    let stretch = (
        u32::try_from(device(whole.width).max(1)).expect("positive width"),
        u32::try_from(device(whole.height).max(1)).expect("positive height"),
    );
    let dst_x = device(whole.x - visible.x);
    let dst_y = device(whole.y - visible.y);
    SpriteGeometry {
        width,
        height,
        dst_x,
        dst_y,
        stretch,
    }
}

/// The integer `buffer_scale` for a compositor without a viewport, where
/// `scale_120` only ever holds whole multiples of 120.
pub(crate) fn integer_buffer_scale(scale_120: u32) -> i32 {
    i32::try_from(scale_120 / SCALE_DENOMINATOR)
        .unwrap_or(1)
        .max(1)
}

/// Scale to render at, falling back to 1.0x when neither `wp_viewporter` nor
/// `wl_surface.set_buffer_scale` (v3+) is available.
pub(crate) const fn effective_scale_120(
    scale_120: u32,
    has_viewport: bool,
    surface_version: u32,
) -> u32 {
    if !has_viewport && surface_version < 3 {
        SCALE_DENOMINATOR
    } else {
        scale_120
    }
}

/// Initial scale in 120ths derived from `wl_output`'s integer scale factor,
/// before `wp_fractional_scale_v1` reports a preferred scale.
pub(crate) fn initial_scale_120(scale_factor: Option<i32>) -> u32 {
    scale_factor
        .and_then(|factor| u32::try_from(factor).ok())
        .unwrap_or(1)
        .max(1)
        .saturating_mul(SCALE_DENOMINATOR)
}

pub(crate) fn layout_bounds(screens: impl IntoIterator<Item = Rect>) -> (i32, i32, i32, i32) {
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

pub(crate) fn point_on_hidden_screen(
    screens: impl IntoIterator<Item = (bool, Rect)>,
    x: i32,
    y: i32,
) -> bool {
    screens.into_iter().any(|(hidden, rect)| {
        hidden && x >= rect.x && y >= rect.y && x < rect.x + rect.width && y < rect.y + rect.height
    })
}

#[cfg(test)]
mod tests {
    use super::{
        Rect, SCALE_DENOMINATOR, effective_scale_120, initial_scale_120, layout_bounds,
        point_on_hidden_screen, sprite_geometry,
    };

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

    #[test]
    fn fractional_scale_buffer_size_is_invariant_under_translation() {
        // 133.3% (160/120), 125% (150/120), 140% (168/120), 160% (192/120).
        for scale_120 in [160, 150, 168, 192] {
            let origin = Rect {
                x: 0,
                y: 0,
                width: 32,
                height: 32,
            };
            let baseline = sprite_geometry(origin, origin, scale_120);
            for offset in 1..=64 {
                let moved = Rect {
                    x: offset,
                    y: offset * 3,
                    width: 32,
                    height: 32,
                };
                let geom = sprite_geometry(moved, moved, scale_120);
                assert_eq!(geom, baseline, "scale_120={scale_120}, offset={offset}");
                assert_eq!((geom.dst_x, geom.dst_y), (0, 0));
            }

            // Partially clipped on the left/top edge shifts the blit offset
            // negatively while keeping stretch constant.
            let whole = Rect {
                x: -10,
                y: -6,
                width: 32,
                height: 32,
            };
            let visible = Rect {
                x: 0,
                y: 0,
                width: 22,
                height: 26,
            };
            let clipped = sprite_geometry(whole, visible, scale_120);
            assert_eq!(clipped.stretch, baseline.stretch);
            assert!(clipped.dst_x < 0 && clipped.dst_y < 0);
            assert!(clipped.width > 0 && clipped.height > 0);
        }
    }

    #[test]
    fn integer_scale_buffer_dimensions_are_multiples_of_buffer_scale() {
        for buffer_scale in 1..=4 {
            let scale_120 = u32::try_from(buffer_scale).unwrap() * SCALE_DENOMINATOR;
            for &(w, h) in &[(1, 1), (13, 27), (32, 32), (64, 64), (19, 3)] {
                let whole = Rect {
                    x: -5,
                    y: 7,
                    width: 64,
                    height: 64,
                };
                let visible = Rect {
                    x: 0,
                    y: 7,
                    width: w,
                    height: h,
                };
                let geom = sprite_geometry(whole, visible, scale_120);
                assert_eq!(geom.width, w * buffer_scale);
                assert_eq!(geom.height, h * buffer_scale);
                assert_eq!(geom.width % buffer_scale, 0);
                assert_eq!(geom.height % buffer_scale, 0);
                assert_eq!(geom.dst_x, -5 * buffer_scale);
                assert_eq!(geom.dst_y, 0);
            }
        }
    }

    #[test]
    fn hidden_screen_hit_testing_preserves_geometry() {
        let left_hidden = (
            true,
            Rect {
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
            },
        );
        let right_visible = (
            false,
            Rect {
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
            },
        );
        let screens = [left_hidden, right_visible];

        // Inclusive top-left, exclusive bottom-right on the hidden screen.
        assert!(point_on_hidden_screen(screens, -1920, 0));
        assert!(point_on_hidden_screen(screens, -1, 1079));
        assert!(!point_on_hidden_screen(screens, -1921, 0));
        assert!(!point_on_hidden_screen(screens, -1, 1080));

        // Points on the visible neighbour are not hidden.
        assert!(!point_on_hidden_screen(screens, 0, 0));
        assert!(!point_on_hidden_screen(screens, 1280, 720));
    }

    #[test]
    fn effective_scale_falls_back_to_unity_without_viewport_or_buffer_scale() {
        assert_eq!(effective_scale_120(240, false, 1), SCALE_DENOMINATOR);
        assert_eq!(effective_scale_120(240, false, 2), SCALE_DENOMINATOR);
        assert_eq!(effective_scale_120(240, false, 3), 240);
        assert_eq!(effective_scale_120(240, false, 6), 240);
        // With a viewport, scaling is handled via set_destination regardless of surface version.
        assert_eq!(effective_scale_120(180, true, 1), 180);
        assert_eq!(effective_scale_120(240, true, 2), 240);
    }

    #[test]
    fn initial_scale_uses_output_factor_with_safe_floor() {
        assert_eq!(initial_scale_120(Some(2)), 2 * SCALE_DENOMINATOR);
        assert_eq!(initial_scale_120(Some(1)), SCALE_DENOMINATOR);
        assert_eq!(initial_scale_120(Some(0)), SCALE_DENOMINATOR);
        assert_eq!(initial_scale_120(Some(-3)), SCALE_DENOMINATOR);
        assert_eq!(initial_scale_120(None), SCALE_DENOMINATOR);
    }
}
