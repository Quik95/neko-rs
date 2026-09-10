//! Command line surface. Flag names follow wayneko where the flag still means
//! something here, so muscle memory carries over.

use std::str::FromStr;

use anyhow::{Result, bail};
use clap::{Parser, ValueEnum};
use neko_render::Layer;
use neko_sprites::Animal;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum AnimalArg {
    Neko,
    Inu,
    Random,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum LayerArg {
    Background,
    Bottom,
    Top,
    Overlay,
}

impl From<LayerArg> for Layer {
    fn from(value: LayerArg) -> Self {
        match value {
            LayerArg::Background => Self::Background,
            LayerArg::Bottom => Self::Bottom,
            LayerArg::Top => Self::Top,
            LayerArg::Overlay => Self::Overlay,
        }
    }
}

/// An ARGB8888 colour parsed from the command line.
#[derive(Debug, Clone, Copy)]
pub struct Colour(pub u32);

impl FromStr for Colour {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let named = match s {
            "black" => Some(0xff00_0000),
            "white" => Some(0xffff_ffff),
            "transparent" | "none" => Some(0x0000_0000),
            _ => None,
        };
        if let Some(colour) = named {
            return Ok(Self(colour));
        }

        let digits = s.strip_prefix('#').unwrap_or(s);
        let value = u32::from_str_radix(digits, 16)
            .map_err(|e| anyhow::anyhow!("{s:?} is not a colour: {e}"))?;
        match digits.len() {
            // Bare rgb is taken as fully opaque; there is no point in an
            // invisible cat by accident.
            6 => Ok(Self(0xff00_0000 | value)),
            8 => Ok(Self(value)),
            _ => bail!("{s:?} is not a colour: want #rrggbb or #aarrggbb"),
        }
    }
}

#[derive(Debug, Parser)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "these are command line flags, not a state machine"
)]
#[command(
    name = "nekors",
    version,
    about = "An oneko clone that chases the cursor across a Wayland desktop",
    long_about = "Draws a cat on a fullscreen click-through layer-shell overlay and \
runs it after your cursor. The cursor position comes from the companion KWin \
script over D-Bus - without it loaded, the cat has nothing to chase."
)]
pub struct Cli {
    /// Which animal to draw.
    #[arg(long = "type", value_enum, default_value = "neko")]
    pub animal: AnimalArg,

    /// Colour filling the animal's body.
    #[arg(
        long = "background-colour",
        alias = "background-color",
        default_value = "white"
    )]
    pub background: Colour,

    /// Colour of the animal's outline.
    #[arg(
        long = "outline-colour",
        alias = "outline-color",
        default_value = "black"
    )]
    pub outline: Colour,

    /// Which layer-shell layer to draw on.
    #[arg(long, value_enum, default_value = "overlay")]
    pub layer: LayerArg,

    /// Keep to one output, by name (e.g. DP-1). By default the animal roams
    /// every monitor and can walk from one to the next.
    #[arg(long)]
    pub output: Option<String>,

    /// Stay on screens showing a fullscreen window, instead of stepping off
    /// them. Off by default: a cat walking across a film is the one place
    /// nobody wants it. Needs the `KWin` script, which is what reports it.
    #[arg(long)]
    pub over_fullscreen: bool,

    /// Integer upscaling. 32x32 is a speck on a 4K screen.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=16))]
    pub scale: u32,

    /// Pixels travelled per 125 ms tick.
    #[arg(long, default_value_t = 13.0)]
    pub speed: f64,

    /// Multiplies the idle timers; above 1 the animal nods off sooner.
    #[arg(long, default_value_t = 1.0)]
    pub sleepiness: f64,

    /// Sleepiness to use at night instead of --sleepiness. Night is 22:00 to
    /// 06:00 in the local timezone; unset means the same all day.
    #[arg(long)]
    pub sleepiness_night: Option<f64>,

    /// Seconds without the cursor moving before the animal is put to sleep.
    /// 0 disables it and leaves the usual idle chain to get there.
    #[arg(long, default_value_t = 0.0, value_parser = idle_seconds)]
    pub idle_sleep: f64,

    /// Seconds of seat idleness, as reported by the compositor's idle-notify
    /// protocol, before the animal sleeps. Unlike --idle-sleep this counts the
    /// keyboard too, and asks the compositor rather than guessing. 0 disables it.
    #[arg(long, default_value_t = 0.0, value_parser = idle_seconds)]
    pub idle_notify: f64,

    /// Don't claw at screen edges when the cursor is somewhere unreachable.
    #[arg(long)]
    pub no_wall_scratch: bool,

    #[arg(
        long,
        help = "Exit with an error when the compositor closes the surfaces, allowing a supervisor to restart; does not recreate surfaces. Without this flag, exit successfully."
    )]
    pub survive_close: bool,

    /// Ignore the cursor and let the animal idle where it stands, the way
    /// wayneko behaves without --follow-pointer.
    #[arg(long = "static")]
    pub is_static: bool,

    /// Print a shell completion script and exit.
    #[arg(long, value_enum, value_name = "SHELL")]
    pub completions: Option<clap_complete::Shell>,

    /// Print a roff man page and exit.
    #[arg(long)]
    pub man: bool,
}

fn idle_seconds(value: &str) -> Result<f64> {
    let seconds: f64 = value.parse()?;
    std::time::Duration::try_from_secs_f64(seconds)
        .map_err(|error| anyhow::anyhow!("invalid idle duration: {error}"))?;
    Ok(seconds)
}

impl AnimalArg {
    pub fn resolve(self) -> Animal {
        match self {
            Self::Neko => Animal::Neko,
            Self::Inu => Animal::Dog,
            // Not worth a dependency on a random number generator.
            Self::Random => {
                if std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.subsec_nanos())
                    .is_multiple_of(2)
                {
                    Animal::Neko
                } else {
                    Animal::Dog
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_durations_are_checked_during_parsing() {
        for flag in ["--idle-sleep", "--idle-notify"] {
            for value in ["inf", "NaN", "-inf", "-1", "1e300"] {
                assert!(Cli::try_parse_from(["nekors", &format!("{flag}={value}")]).is_err());
            }
            for value in ["0", "0.5", "3600"] {
                assert!(Cli::try_parse_from(["nekors", &format!("{flag}={value}")]).is_ok());
            }
        }
    }

    #[test]
    fn colour_parser_preserves_straight_alpha() {
        assert_eq!("#80804020".parse::<Colour>().unwrap().0, 0x8080_4020);
    }
}
