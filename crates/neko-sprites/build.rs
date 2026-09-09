//! Turns the oneko `.xbm` bitmaps into `const` tables at build time, so the
//! generated code never has to live in the repository.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

/// One parsed xbm file: dimensions plus the raw 1-bit rows.
struct Xbm {
    width: u32,
    height: u32,
    bits: Vec<u8>,
}

/// Parses the subset of xbm that oneko's bitmaps use: two `#define`s for the
/// dimensions followed by a brace-delimited list of byte literals.
fn parse_xbm(src: &str, path: &Path) -> Xbm {
    let mut dims = Vec::new();
    for line in src.lines() {
        let Some(rest) = line.strip_prefix("#define ") else {
            continue;
        };
        if let Some(value) = rest.rsplit_once(' ')
            && (rest.contains("_width") || rest.contains("_height"))
        {
            dims.push(
                value
                    .1
                    .trim()
                    .parse::<u32>()
                    .unwrap_or_else(|e| panic!("{}: bad dimension: {e}", path.display())),
            );
        }
    }
    assert_eq!(
        dims.len(),
        2,
        "{}: expected width and height",
        path.display()
    );
    let (width, height) = (dims[0], dims[1]);

    let body = src
        .split_once('{')
        .and_then(|(_, rest)| rest.split_once('}'))
        .unwrap_or_else(|| panic!("{}: no brace-delimited byte list", path.display()))
        .0;

    let bits: Vec<u8> = body
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(|token| {
            let digits = token
                .strip_prefix("0x")
                .or_else(|| token.strip_prefix("0X"))
                .unwrap_or_else(|| panic!("{}: not a hex byte: {token}", path.display()));
            u8::from_str_radix(digits, 16)
                .unwrap_or_else(|e| panic!("{}: bad byte {token}: {e}", path.display()))
        })
        .collect();

    let stride = (width as usize).div_ceil(8);
    assert_eq!(
        bits.len(),
        stride * height as usize,
        "{}: {} bytes for {width}x{height}",
        path.display(),
        bits.len(),
    );

    Xbm {
        width,
        height,
        bits,
    }
}

/// `up1_dog_mask.xbm` -> (`up1`, mask), `up1.xbm` -> (`up1`, image).
fn split_stem(stem: &str, animal: &str) -> (String, bool) {
    let (base, is_mask) = match stem.strip_suffix("_mask") {
        Some(base) => (base, true),
        None => (stem, false),
    };
    let base = base
        .strip_suffix(&format!("_{animal}"))
        .unwrap_or(base)
        .to_owned();
    (base, is_mask)
}

fn emit_byte_slice(out: &mut String, bytes: &[u8]) {
    out.push('[');
    for byte in bytes {
        let _ = write!(out, "{byte:#04x},");
    }
    out.push(']');
}

fn emit_animal(out: &mut String, dir: &Path, animal: &str) {
    println!("cargo::rerun-if-changed={}", dir.display());

    // (image, mask) pairs keyed by sprite name; BTreeMap keeps the output stable.
    let mut sprites: BTreeMap<String, (Option<Xbm>, Option<Xbm>)> = BTreeMap::new();
    for entry in std::fs::read_dir(dir).expect("bitmap directory") {
        let path = entry.expect("directory entry").path();
        if path.extension().is_none_or(|ext| ext != "xbm") {
            continue;
        }
        println!("cargo::rerun-if-changed={}", path.display());

        let stem = path.file_stem().expect("file stem").to_string_lossy();
        let (name, is_mask) = split_stem(&stem, animal);
        let src = std::fs::read_to_string(&path).expect("readable xbm");
        let xbm = parse_xbm(&src, &path);

        let slot = sprites.entry(name).or_insert((None, None));
        if is_mask {
            slot.1 = Some(xbm);
        } else {
            slot.0 = Some(xbm);
        }
    }

    let _ = writeln!(out, "pub static {}: &[Sprite] = &[", animal.to_uppercase());
    for (name, (image, mask)) in &sprites {
        let image = image
            .as_ref()
            .unwrap_or_else(|| panic!("{animal}/{name}: mask without a bitmap"));
        let mask = mask
            .as_ref()
            .unwrap_or_else(|| panic!("{animal}/{name}: bitmap without a mask"));
        assert_eq!(
            (image.width, image.height),
            (mask.width, mask.height),
            "{animal}/{name}: bitmap and mask disagree on size",
        );

        let _ = write!(
            out,
            "Sprite{{name:{name:?},width:{},height:{},bits:&",
            image.width, image.height,
        );
        emit_byte_slice(out, &image.bits);
        out.push_str(",mask:&");
        emit_byte_slice(out, &mask.bits);
        out.push_str("},");
    }
    out.push_str("];\n");
}

fn main() {
    let bitmaps = Path::new(env!("CARGO_MANIFEST_DIR")).join("bitmaps");
    let mut out = String::new();
    for animal in ["neko", "dog"] {
        emit_animal(&mut out, &bitmaps.join(animal), animal);
    }

    let dest = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("sprites.rs");
    std::fs::write(dest, out).expect("write generated sprites");
}
