//! The oneko sprite sheet: 1-bit xbm bitmaps parsed at build time, plus the
//! blit that turns a bitmap/mask pair into ARGB8888 pixels.
//!
//! Each sprite is a pair of 1-bit images, exactly as oneko drew them: the mask
//! is the silhouette (filled with the background colour) and the bitmap is the
//! line art on top (the outline colour). Bits are xbm order - LSB first within
//! each byte, rows padded up to a whole number of bytes.

include!(concat!(env!("OUT_DIR"), "/sprites.rs"));

/// A single animation frame.
pub struct Sprite {
    /// oneko's name for the frame, e.g. `"upleft1"` or `"sleep2"`.
    pub name: &'static str,
    pub width: u32,
    pub height: u32,
    /// The line art, 1 bit per pixel.
    pub bits: &'static [u8],
    /// The silhouette, 1 bit per pixel, same dimensions.
    pub mask: &'static [u8],
}

/// Which sprite sheet to draw from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Animal {
    #[default]
    Neko,
    Dog,
}

impl Animal {
    #[must_use]
    pub fn sprites(self) -> &'static [Sprite] {
        match self {
            Self::Neko => NEKO,
            Self::Dog => DOG,
        }
    }

    /// Looks a frame up by its oneko name.
    #[must_use]
    pub fn sprite(self, name: &str) -> Option<&'static Sprite> {
        self.sprites().iter().find(|sprite| sprite.name == name)
    }
}

impl std::str::FromStr for Animal {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "neko" | "cat" => Ok(Self::Neko),
            "dog" | "inu" => Ok(Self::Dog),
            other => Err(format!("unknown animal: {other}")),
        }
    }
}

/// A borrowed ARGB8888 pixel buffer laid out as `stride`-pixel rows.
pub struct Canvas<'a> {
    pub pixels: &'a mut [u32],
    pub stride: usize,
}

impl Canvas<'_> {
    #[must_use]
    pub fn rows(&self) -> usize {
        self.pixels.len() / self.stride
    }

    /// Writes one pixel, ignoring coordinates outside the buffer.
    fn put(&mut self, x: i64, y: i64, colour: u32) {
        let (Ok(x), Ok(y)) = (usize::try_from(x), usize::try_from(y)) else {
            return;
        };
        if x >= self.stride || y >= self.rows() {
            return;
        }
        self.pixels[y * self.stride + x] = colour;
    }
}

/// The two colours a 1-bit sprite is drawn with, as ARGB8888.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Fills the silhouette.
    pub background: u32,
    /// Draws the line art on top of it.
    pub outline: u32,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            background: 0xffff_ffff,
            outline: 0xff00_0000,
        }
    }
}

impl Sprite {
    fn get(bits: &[u8], stride: usize, x: u32, y: u32) -> bool {
        let index = y as usize * stride + (x as usize) / 8;
        bits[index] >> (x % 8) & 1 == 1
    }

    /// Blits onto `canvas` with its top-left at `(dst_x, dst_y)`, scaled up by
    /// an integer factor with nearest-neighbour sampling so the 1-bit art stays
    /// crisp. Pixels outside the canvas are clipped, and anything outside the
    /// mask is left untouched - the surface stays transparent there.
    pub fn blit_argb(
        &self,
        canvas: &mut Canvas,
        dst_x: i32,
        dst_y: i32,
        scale: u32,
        palette: Palette,
    ) {
        assert!(scale > 0, "scale must be positive");
        let stride = (self.width as usize).div_ceil(8);
        let (dst_x, dst_y, scale) = (i64::from(dst_x), i64::from(dst_y), i64::from(scale));

        for y in 0..self.height {
            for x in 0..self.width {
                if !Self::get(self.mask, stride, x, y) {
                    continue;
                }
                let colour = if Self::get(self.bits, stride, x, y) {
                    palette.outline
                } else {
                    palette.background
                };

                // One source pixel becomes a scale x scale block.
                let base_x = dst_x + i64::from(x) * scale;
                let base_y = dst_y + i64::from(y) * scale;
                for sub_y in 0..scale {
                    for sub_x in 0..scale {
                        canvas.put(base_x + sub_x, base_y + sub_y, colour);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The full oneko set: two frames each of eight directions, four wall
    /// scratches and the idle animations.
    const EXPECTED: &[&str] = &[
        "awake", "down1", "down2", "dtogi1", "dtogi2", "dwleft1", "dwleft2", "dwright1",
        "dwright2", "jare2", "kaki1", "kaki2", "left1", "left2", "ltogi1", "ltogi2", "mati2",
        "mati3", "right1", "right2", "rtogi1", "rtogi2", "sleep1", "sleep2", "up1", "up2",
        "upleft1", "upleft2", "upright1", "upright2", "utogi1", "utogi2",
    ];

    #[test]
    fn both_animals_have_the_full_frame_set() {
        for animal in [Animal::Neko, Animal::Dog] {
            for name in EXPECTED {
                assert!(
                    animal.sprite(name).is_some(),
                    "{animal:?} is missing {name}",
                );
            }
            assert_eq!(animal.sprites().len(), EXPECTED.len());
        }
    }

    #[test]
    fn sprites_are_32x32_with_matching_bit_counts() {
        for sprite in Animal::Neko.sprites() {
            assert_eq!((sprite.width, sprite.height), (32, 32), "{}", sprite.name);
            assert_eq!(sprite.bits.len(), 32 * 4, "{}", sprite.name);
            assert_eq!(sprite.mask.len(), 32 * 4, "{}", sprite.name);
        }
    }

    #[test]
    fn blit_fills_the_mask_and_nothing_else() {
        let sprite = Animal::Neko.sprite("mati2").expect("mati2");
        let mut pixels = vec![0u32; 32 * 32];
        let mut canvas = Canvas {
            pixels: &mut pixels,
            stride: 32,
        };
        sprite.blit_argb(&mut canvas, 0, 0, 1, Palette::default());

        let painted = pixels.iter().filter(|&&px| px != 0).count();
        let masked = (0..32)
            .flat_map(|y| (0..32).map(move |x| (x, y)))
            .filter(|&(x, y)| Sprite::get(sprite.mask, 4, x, y))
            .count();
        assert_eq!(painted, masked);
        assert!(masked > 0);
    }

    #[test]
    fn blit_clips_instead_of_panicking() {
        let sprite = Animal::Neko.sprite("mati2").expect("mati2");
        let mut pixels = vec![0u32; 16 * 16];
        let mut canvas = Canvas {
            pixels: &mut pixels,
            stride: 16,
        };
        sprite.blit_argb(&mut canvas, -8, -8, 2, Palette::default());
    }
}
