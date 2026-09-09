//! Renders every frame of a sheet into one PPM contact sheet, for eyeballing
//! that the xbm parsing and the blit produce a recognisable cat.
//!
//! ```sh
//! cargo run -p neko-sprites --example contact_sheet -- neko /tmp/neko.ppm
//! ```

use std::str::FromStr as _;

use neko_sprites::{Animal, Canvas, Palette};

const COLUMNS: usize = 8;
const SCALE: u32 = 4;

fn main() {
    let mut args = std::env::args().skip(1);
    let animal = args.next().unwrap_or_else(|| "neko".into());
    let out = args.next().unwrap_or_else(|| "contact-sheet.ppm".into());
    let animal = Animal::from_str(&animal).expect("neko or dog");

    let sprites = animal.sprites();
    let cell = 32 * SCALE as usize;
    let rows = sprites.len().div_ceil(COLUMNS);
    let (width, height) = (COLUMNS * cell, rows * cell);

    // Mid grey background so both the white fill and the black outline show.
    let mut pixels = vec![0xff80_8080u32; width * height];
    let mut canvas = Canvas {
        pixels: &mut pixels,
        stride: width,
    };
    for (index, sprite) in sprites.iter().enumerate() {
        let x = (index % COLUMNS) * cell;
        let y = (index / COLUMNS) * cell;
        sprite.blit_argb(
            &mut canvas,
            i32::try_from(x).expect("in range"),
            i32::try_from(y).expect("in range"),
            SCALE,
            Palette::default(),
        );
        println!("{index:2}: {}", sprite.name);
    }

    let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
    for pixel in pixels {
        let [_, r, g, b] = pixel.to_be_bytes();
        ppm.extend_from_slice(&[r, g, b]);
    }
    std::fs::write(&out, ppm).expect("write contact sheet");
    println!("wrote {out}");
}
