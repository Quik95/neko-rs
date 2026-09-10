//! Ties the pieces together: the D-Bus cursor feed, the state machine, and the
//! overlay it is drawn on.

mod cli;
mod cursor;

use std::io;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use clap::{CommandFactory as _, Parser as _};
use neko_core::{Config, Neko, SPRITE_SIZE, TICK};
use neko_render::{Overlay, OverlayConfig};
use neko_sprites::Palette;

use crate::cli::Cli;
use crate::cursor::{CursorSource as _, DbusCursor, FullscreenSource as _};

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Cli::parse();

    if let Some(shell) = args.completions {
        let mut command = Cli::command();
        let name = command.get_name().to_string();
        clap_complete::generate(shell, &mut command, name, &mut io::stdout());
        return Ok(());
    }
    if args.man {
        clap_mangen::Man::new(Cli::command()).render(&mut io::stdout())?;
        return Ok(());
    }

    run(&args)
}

fn run(args: &Cli) -> Result<()> {
    let animal = args.animal.resolve();
    let scale = args.scale;

    // The cursor feed comes up first: if the bus name is taken by another
    // instance there is no point building a surface.
    let cursor = DbusCursor::new()?;

    let mut overlay = Overlay::new(OverlayConfig {
        layer: args.layer.into(),
        output: args.output.clone(),
        scale,
        palette: Palette {
            background: args.background.0,
            outline: args.outline.0,
        },
        idle_after: (args.idle_notify > 0.0).then(|| Duration::from_secs_f64(args.idle_notify)),
    })?;

    let config = Config {
        speed: args.speed,
        sleepiness: args.sleepiness,
        scratch_walls: !args.no_wall_scratch,
        ..Config::default()
    };
    let mut neko = Neko::new(config, brain_bounds(overlay.size(), scale));
    let idle_sleep = (args.idle_sleep > 0.0).then(|| Duration::from_secs_f64(args.idle_sleep));

    log::info!(
        "{animal:?} on a {}x{} overlay across {} screen(s), scale {scale}",
        overlay.size().0,
        overlay.size().1,
        overlay.screen_count(),
    );

    let mut warned_about_silence = false;
    // Starts false so the first tick applies the night value if it is night.
    let mut night_now = false;
    loop {
        let frame_started = Instant::now();
        overlay.dispatch()?;

        if overlay.closed() {
            if !args.survive_close {
                log::info!("the compositor closed the surface, exiting");
                return Ok(());
            }
            bail!("the compositor closed the surface and it cannot be recreated yet");
        }
        neko.set_bounds(brain_bounds(overlay.size(), scale));

        // Tracked as "is it night" rather than as the value, so switching back
        // and forth is an unambiguous bool flip rather than a float compare.
        if let Some(night_sleepiness) = args.sleepiness_night {
            let night = is_night();
            if night != night_now {
                let wanted = if night {
                    night_sleepiness
                } else {
                    args.sleepiness
                };
                log::info!("sleepiness is now {wanted}");
                neko.set_sleepiness(wanted);
                night_now = night;
            }
        }

        // --static keeps the animal to itself, so the cursor is never consulted.
        match cursor.last_position().filter(|_| !args.is_static) {
            None => {
                if !warned_about_silence && !args.is_static {
                    log::warn!(
                        "no cursor position yet - is the nekors KWin script loaded? \
                         try `reload-kwin-script`"
                    );
                    warned_about_silence = true;
                }
                // Nothing to chase; let the idle chain run its course.
                let position = neko.position();
                neko.tick((position.0 + SPRITE_SIZE / 2, position.1 + SPRITE_SIZE));
            }
            Some((position, age)) => {
                // The KWin script reports in the compositor's global
                // coordinates, which start wherever the leftmost monitor does;
                // the state machine counts from the top-left of the layout.
                let origin = overlay.origin();
                neko.tick((position.0 - origin.0, position.1 - origin.1));
                // A cursor that has not moved in a long time means the user has
                // gone away - drop straight to sleep rather than standing there
                // washing indefinitely.
                if idle_sleep.is_some_and(|limit| age >= limit) {
                    neko.sleep_now();
                }
            }
        }

        // The compositor's word beats any guess made from cursor traffic: it
        // sees the keyboard too, and it knows about the lock screen.
        if overlay.session_idle() {
            neko.sleep_now();
        }

        // Fullscreen windows are the compositor's business too; a Wayland
        // client cannot see them. The animal keeps walking its usual route
        // across the layout - it is simply not drawn where a film is.
        if !args.over_fullscreen {
            overlay.hide_outputs(&cursor.fullscreen_outputs());
        }

        let frame = neko.frame();
        let sprite = animal
            .sprite(frame)
            .with_context(|| format!("no sprite named {frame}"))?;
        let (x, y) = neko.position();
        overlay.draw(sprite, x, y);

        // oneko thinks eight times a second; keep that rate regardless of how
        // long the frame took.
        if let Some(remaining) = TICK.checked_sub(frame_started.elapsed()) {
            std::thread::sleep(remaining);
        }
    }
}

/// Whether it is night where the machine is, for `--sleepiness-night`.
///
/// 22:00 to 06:00, matching what people usually mean by it. The timezone comes
/// from the system, so this follows the user across a flight without asking.
fn is_night() -> bool {
    let hour = jiff::Zoned::now().hour();
    !(6..22).contains(&hour)
}

/// The bounds to hand the state machine.
///
/// It reasons in unscaled 32x32 sprites, but what is drawn is `scale` times
/// that, so the screen is shrunk by the difference to keep the drawn animal on
/// it.
fn brain_bounds(size: (i32, i32), scale: u32) -> (i32, i32) {
    let overhang = SPRITE_SIZE * (i32::try_from(scale).unwrap_or(1) - 1);
    (
        (size.0 - overhang).max(SPRITE_SIZE),
        (size.1 - overhang).max(SPRITE_SIZE),
    )
}
