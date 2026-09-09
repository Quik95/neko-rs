//! The oneko brain: where the cat is, what it is doing, and which frame that
//! means. A reimplementation of `oneko.c`'s state machine (public domain).
//!
//! Deliberately free of I/O - no Wayland, no D-Bus, no clock. The caller ticks
//! it at a fixed rate (125 ms in the original) and hands it the cursor
//! position; everything else is arithmetic, which is what makes the whole
//! thing testable without a compositor.

use std::time::Duration;

/// oneko's tick interval: the animal thinks eight times a second.
pub const TICK: Duration = Duration::from_millis(125);

/// Sprite size in unscaled pixels. Every oneko frame is 32x32.
pub const SPRITE_SIZE: i32 = 32;

/// sin(pi/8) and sin(3pi/8) split the circle into the eight directions.
const SIN_PI_PER_8: f64 = 0.382_683_432_365_089_8;
const SIN_PI_PER_8_TIMES_3: f64 = 0.923_879_532_511_286_7;

const STOP_TIME: u32 = 4;
const JARE_TIME: u32 = 10;
const KAKI_TIME: u32 = 4;
const AKUBI_TIME: u32 = 6;
const AWAKE_TIME: u32 = 3;
const TOGI_TIME: u32 = 10;

/// Truncates towards zero, the way oneko's `(int)` casts do, and saturates
/// instead of wrapping on the absurd values a bogus screen size could produce.
#[allow(clippy::cast_possible_truncation, reason = "saturated by the clamp")]
fn truncate(value: f64) -> i32 {
    value.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

/// One of the eight directions the animal can run in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
    UpLeft,
    UpRight,
    DownLeft,
    DownRight,
}

/// A screen edge the animal can scratch at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wall {
    Up,
    Down,
    Left,
    Right,
}

/// What the animal is doing. The idle chain runs
/// `Stop -> Jare -> Kaki -> Akubi -> Sleep`, and any cursor movement kicks it
/// back to `Awake`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    /// Standing still, just arrived.
    #[default]
    Stop,
    /// Washing its face.
    Jare,
    /// Scratching itself.
    Kaki,
    /// Yawning.
    Akubi,
    Sleep,
    /// The startle before it starts running.
    Awake,
    Move(Direction),
    /// Clawing at a screen edge it cannot get past.
    Scratch(Wall),
}

impl State {
    /// The two oneko frame names this state alternates between.
    #[must_use]
    pub fn frames(self) -> [&'static str; 2] {
        match self {
            Self::Stop => ["mati2", "mati2"],
            Self::Jare => ["jare2", "mati2"],
            Self::Kaki => ["kaki1", "kaki2"],
            Self::Akubi => ["mati3", "mati3"],
            Self::Sleep => ["sleep1", "sleep2"],
            Self::Awake => ["awake", "awake"],
            Self::Move(Direction::Up) => ["up1", "up2"],
            Self::Move(Direction::Down) => ["down1", "down2"],
            Self::Move(Direction::Left) => ["left1", "left2"],
            Self::Move(Direction::Right) => ["right1", "right2"],
            Self::Move(Direction::UpLeft) => ["upleft1", "upleft2"],
            Self::Move(Direction::UpRight) => ["upright1", "upright2"],
            Self::Move(Direction::DownLeft) => ["dwleft1", "dwleft2"],
            Self::Move(Direction::DownRight) => ["dwright1", "dwright2"],
            Self::Scratch(Wall::Up) => ["utogi1", "utogi2"],
            Self::Scratch(Wall::Down) => ["dtogi1", "dtogi2"],
            Self::Scratch(Wall::Left) => ["ltogi1", "ltogi2"],
            Self::Scratch(Wall::Right) => ["rtogi1", "rtogi2"],
        }
    }

    #[must_use]
    pub fn is_moving(self) -> bool {
        matches!(self, Self::Move(_))
    }
}

/// Tunables. The defaults are oneko's own for the cat.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Pixels travelled per tick.
    pub speed: f64,
    /// Cursor jitter, in pixels, that does not count as movement. Without it
    /// the animal twitches whenever the mouse is nudged.
    pub idle_space: i32,
    /// Multiplies every idle timer. Above 1 the animal nods off sooner.
    pub sleepiness: f64,
    /// Whether it claws at screen edges instead of just standing there.
    pub scratch_walls: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            speed: 13.0,
            idle_space: 6,
            sleepiness: 1.0,
            scratch_walls: true,
        }
    }
}

/// The animal.
#[derive(Debug)]
pub struct Neko {
    config: Config,
    /// Screen size in unscaled pixels; the animal is kept inside it.
    bounds: (i32, i32),
    /// Top-left of the sprite.
    x: i32,
    y: i32,
    state: State,
    /// Frames since the state was entered; drives the two-frame animation.
    tick_count: u32,
    /// Half that, roughly - the unit the state timeouts are counted in.
    state_count: u32,
    mouse: (i32, i32),
    prev_mouse: (i32, i32),
    move_dx: i32,
    move_dy: i32,
    /// Where it was at the end of the previous tick.
    last_position: (i32, i32),
}

impl Neko {
    #[must_use]
    pub fn new(config: Config, bounds: (i32, i32)) -> Self {
        let start = (
            bounds.0 / 2 - SPRITE_SIZE / 2,
            bounds.1 / 2 - SPRITE_SIZE / 2,
        );
        Self {
            config,
            bounds,
            x: start.0,
            y: start.1,
            state: State::Stop,
            tick_count: 0,
            state_count: 0,
            mouse: start,
            prev_mouse: start,
            move_dx: 0,
            move_dy: 0,
            last_position: start,
        }
    }

    #[must_use]
    pub fn position(&self) -> (i32, i32) {
        (self.x, self.y)
    }

    #[must_use]
    pub fn state(&self) -> State {
        self.state
    }

    /// The frame to draw right now. `Sleep` breathes at a quarter of the rate
    /// so the two sleeping frames read as slow breathing rather than a flicker.
    #[must_use]
    pub fn frame(&self) -> &'static str {
        let index = if self.state == State::Sleep {
            (self.tick_count >> 2) & 1
        } else {
            self.tick_count & 1
        };
        self.state.frames()[index as usize]
    }

    /// Resizes the screen, clamping the animal back inside it.
    pub fn set_bounds(&mut self, bounds: (i32, i32)) {
        self.bounds = bounds;
        self.clamp_into_bounds();
    }

    /// Drops the animal straight into `Sleep` - used when the compositor says
    /// the session went idle, or when cursor updates dry up.
    pub fn sleep_now(&mut self) {
        if self.state != State::Sleep {
            self.set_state(State::Sleep);
        }
    }

    /// Advances one tick. `cursor` is the pointer in unscaled screen pixels;
    /// pass the last known position when there is no fresh one.
    pub fn tick(&mut self, cursor: (i32, i32)) {
        self.last_position = (self.x, self.y);
        self.calc_dx_dy(cursor);
        self.advance_counters();

        match self.state {
            State::Stop => {
                if self.cursor_moved() {
                    self.set_state(State::Awake);
                } else if self.state_count >= self.timeout(STOP_TIME) {
                    let next = self.wall_to_scratch().map_or(State::Jare, State::Scratch);
                    self.set_state(next);
                }
            }
            State::Jare => self.idle_step(JARE_TIME, State::Kaki),
            State::Kaki => self.idle_step(KAKI_TIME, State::Akubi),
            State::Akubi => self.idle_step(AKUBI_TIME, State::Sleep),
            State::Scratch(_) => self.idle_step(TOGI_TIME, State::Kaki),
            State::Sleep => {
                if self.cursor_moved() {
                    self.set_state(State::Awake);
                }
            }
            State::Awake => {
                if self.state_count >= self.timeout(AWAKE_TIME) {
                    self.face_the_cursor();
                }
            }
            State::Move(_) => {
                self.x += self.move_dx;
                self.y += self.move_dy;
                self.face_the_cursor();
                // Pinned against an edge and not actually getting anywhere
                // means it has arrived as close as it is going to get.
                if self.clamp_into_bounds() && (self.x, self.y) == self.last_position {
                    self.set_state(State::Stop);
                }
            }
        }
    }

    /// The shared shape of the idle chain: wake on movement, otherwise move on
    /// to the next pose once the timer runs out.
    fn idle_step(&mut self, time: u32, next: State) {
        if self.cursor_moved() {
            self.set_state(State::Awake);
        } else if self.state_count >= self.timeout(time) {
            self.set_state(next);
        }
    }

    fn set_state(&mut self, state: State) {
        self.tick_count = 0;
        self.state_count = 0;
        self.state = state;
    }

    /// oneko counts animation frames and bumps the state timer on every other
    /// one, so a state's "time" is in units of two frames.
    fn advance_counters(&mut self) {
        self.tick_count = self.tick_count.wrapping_add(1);
        if self.tick_count.is_multiple_of(2) {
            self.state_count = self.state_count.saturating_add(1);
        }
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped into u32 range first"
    )]
    fn timeout(&self, base: u32) -> u32 {
        if self.config.sleepiness <= 0.0 {
            return u32::MAX;
        }
        let scaled = f64::from(base) / self.config.sleepiness;
        // Never zero: a state that times out instantly would never be seen.
        (scaled.round().clamp(1.0, f64::from(u32::MAX)) as u32).max(1)
    }

    /// True once the cursor has moved further than the idle deadzone.
    fn cursor_moved(&self) -> bool {
        let space = self.config.idle_space;
        (self.prev_mouse.0 - self.mouse.0).abs() > space
            || (self.prev_mouse.1 - self.mouse.1).abs() > space
    }

    /// Aims at the cursor and caps the step at `speed` pixels.
    fn calc_dx_dy(&mut self, cursor: (i32, i32)) {
        self.prev_mouse = self.mouse;
        self.mouse = cursor;

        // The animal chases with the middle of its bottom edge - that is where
        // its paws are, and it is what makes it look like it lands on the
        // cursor rather than covering it.
        let dx = f64::from(self.mouse.0 - self.x - SPRITE_SIZE / 2);
        let dy = f64::from(self.mouse.1 - self.y - SPRITE_SIZE);
        let length = dx.hypot(dy);

        (self.move_dx, self.move_dy) = if length == 0.0 {
            (0, 0)
        } else if length <= self.config.speed {
            (truncate(dx), truncate(dy))
        } else {
            (
                truncate(self.config.speed * dx / length),
                truncate(self.config.speed * dy / length),
            )
        };
    }

    /// Picks the one of eight directions closest to the current heading.
    fn face_the_cursor(&mut self) {
        let next = match self.heading() {
            None => State::Stop,
            Some(direction) => State::Move(direction),
        };
        if self.state != next {
            self.set_state(next);
        }
    }

    fn heading(&self) -> Option<Direction> {
        if (self.move_dx, self.move_dy) == (0, 0) {
            return None;
        }
        let dx = f64::from(self.move_dx);
        // Screen y grows downwards; flip it so the angle is the usual one.
        let dy = f64::from(-self.move_dy);
        let sin_theta = dy / dx.hypot(dy);
        let rightwards = self.move_dx > 0;

        Some(if sin_theta > SIN_PI_PER_8_TIMES_3 {
            Direction::Up
        } else if sin_theta > SIN_PI_PER_8 {
            if rightwards {
                Direction::UpRight
            } else {
                Direction::UpLeft
            }
        } else if sin_theta > -SIN_PI_PER_8 {
            if rightwards {
                Direction::Right
            } else {
                Direction::Left
            }
        } else if sin_theta > -SIN_PI_PER_8_TIMES_3 {
            if rightwards {
                Direction::DownRight
            } else {
                Direction::DownLeft
            }
        } else {
            Direction::Down
        })
    }

    /// Which edge it is pinned against while still trying to go further.
    fn wall_to_scratch(&self) -> Option<Wall> {
        if !self.config.scratch_walls {
            return None;
        }
        if self.move_dx < 0 && self.x <= 0 {
            Some(Wall::Left)
        } else if self.move_dx > 0 && self.x >= self.bounds.0 - SPRITE_SIZE {
            Some(Wall::Right)
        } else if self.move_dy < 0 && self.y <= 0 {
            Some(Wall::Up)
        } else if self.move_dy > 0 && self.y >= self.bounds.1 - SPRITE_SIZE {
            Some(Wall::Down)
        } else {
            None
        }
    }

    /// Clamps into the screen, reporting whether it had to.
    fn clamp_into_bounds(&mut self) -> bool {
        let max_x = (self.bounds.0 - SPRITE_SIZE).max(0);
        let max_y = (self.bounds.1 - SPRITE_SIZE).max(0);
        let clamped = (self.x.clamp(0, max_x), self.y.clamp(0, max_y));
        let hit_edge = clamped != (self.x, self.y);
        (self.x, self.y) = clamped;
        hit_edge
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDS: (i32, i32) = (1000, 800);

    fn neko() -> Neko {
        Neko::new(Config::default(), BOUNDS)
    }

    /// Ticks with a fixed cursor until `predicate` holds, or gives up.
    fn tick_until(neko: &mut Neko, cursor: (i32, i32), predicate: impl Fn(&Neko) -> bool) -> u32 {
        for ticks in 1..=1000 {
            neko.tick(cursor);
            if predicate(neko) {
                return ticks;
            }
        }
        panic!("condition never held; stuck in {:?}", neko.state());
    }

    #[test]
    fn starts_stopped_in_the_middle() {
        let neko = neko();
        assert_eq!(neko.state(), State::Stop);
        assert_eq!(neko.position(), (484, 384));
        assert_eq!(neko.frame(), "mati2");
    }

    #[test]
    fn a_still_cursor_walks_the_whole_idle_chain_into_sleep() {
        let mut neko = neko();
        let cursor = neko.position();

        for expected in [State::Jare, State::Kaki, State::Akubi, State::Sleep] {
            tick_until(&mut neko, cursor, |n| n.state() == expected);
        }
        assert_eq!(neko.state(), State::Sleep);

        // And it stays asleep as long as nothing moves.
        for _ in 0..100 {
            neko.tick(cursor);
        }
        assert_eq!(neko.state(), State::Sleep);
    }

    #[test]
    fn moving_the_cursor_wakes_it_and_it_runs_over() {
        let mut neko = neko();
        let start = neko.position();
        tick_until(&mut neko, start, |n| n.state() == State::Sleep);

        let target = (900, 700);
        neko.tick(target);
        assert_eq!(neko.state(), State::Awake);

        tick_until(&mut neko, target, |n| n.state().is_moving());
        assert_eq!(neko.state(), State::Move(Direction::DownRight));

        tick_until(&mut neko, target, |n| n.state() == State::Stop);
        let (x, y) = neko.position();
        // It stops with the cursor under the middle of its bottom edge.
        assert!((x + SPRITE_SIZE / 2 - target.0).abs() <= 2, "x = {x}");
        assert!((y + SPRITE_SIZE - target.1).abs() <= 2, "y = {y}");
    }

    #[test]
    fn jitter_inside_the_deadzone_does_not_wake_it() {
        let mut neko = neko();
        let cursor = neko.position();
        tick_until(&mut neko, cursor, |n| n.state() == State::Sleep);

        let space = Config::default().idle_space;
        // Deltas between consecutive samples are what counts, so nudge it one
        // deadzone at a time.
        for offset in [space, 0, space, 0] {
            neko.tick((cursor.0 + offset, cursor.1 + offset));
            assert_eq!(neko.state(), State::Sleep, "offset {offset}");
        }
        neko.tick((cursor.0 + space + 1, cursor.1));
        assert_eq!(neko.state(), State::Awake);
    }

    #[test]
    fn every_direction_is_reachable() {
        use Direction::{Down, DownLeft, DownRight, Left, Right, Up, UpLeft, UpRight};

        // Cursor placed far away in each compass direction from the centre.
        let cases = [
            ((500, 0), Up),
            ((500, 800), Down),
            ((0, 400), Left),
            ((1000, 400), Right),
            ((0, 0), UpLeft),
            ((1000, 0), UpRight),
            ((0, 800), DownLeft),
            ((1000, 800), DownRight),
        ];
        for (cursor, expected) in cases {
            let mut neko = neko();
            tick_until(&mut neko, cursor, |n| n.state().is_moving());
            assert_eq!(neko.state(), State::Move(expected), "cursor {cursor:?}");
        }
    }

    #[test]
    fn it_scratches_the_wall_the_cursor_is_hiding_behind() {
        // The cursor sits in the very corner, so the animal ends up pinned
        // against the left edge still wanting to go further left.
        let mut neko = neko();
        tick_until(&mut neko, (0, 400), |n| {
            matches!(n.state(), State::Scratch(_))
        });
        assert_eq!(neko.state(), State::Scratch(Wall::Left));
        assert_eq!(neko.frame(), "ltogi1");

        // Scratching gives up after a while and turns into grooming.
        tick_until(&mut neko, (0, 400), |n| n.state() == State::Kaki);
    }

    #[test]
    fn wall_scratching_can_be_turned_off() {
        let config = Config {
            scratch_walls: false,
            ..Config::default()
        };
        let mut neko = Neko::new(config, BOUNDS);
        tick_until(&mut neko, (0, 400), |n| n.state() == State::Jare);
        assert!(!matches!(neko.state(), State::Scratch(_)));
    }

    #[test]
    fn it_never_leaves_the_screen() {
        let mut neko = neko();
        for cursor in [(-500, -500), (5000, 5000), (0, 0), (1000, 800)] {
            for _ in 0..200 {
                neko.tick(cursor);
                let (x, y) = neko.position();
                assert!((0..=BOUNDS.0 - SPRITE_SIZE).contains(&x), "x = {x}");
                assert!((0..=BOUNDS.1 - SPRITE_SIZE).contains(&y), "y = {y}");
            }
        }
    }

    #[test]
    fn resizing_the_screen_pulls_it_back_in() {
        let mut neko = neko();
        tick_until(&mut neko, (990, 790), |n| n.state() == State::Stop);
        neko.set_bounds((400, 300));
        let (x, y) = neko.position();
        assert_eq!((x, y), (400 - SPRITE_SIZE, 300 - SPRITE_SIZE));
    }

    #[test]
    fn sleepiness_scales_the_idle_timers() {
        let sleepy = Config {
            sleepiness: 4.0,
            ..Config::default()
        };
        let mut lazy = neko();
        let mut sleepy = Neko::new(sleepy, BOUNDS);
        let cursor = lazy.position();

        let lazy_ticks = tick_until(&mut lazy, cursor, |n| n.state() == State::Sleep);
        let sleepy_ticks = tick_until(&mut sleepy, cursor, |n| n.state() == State::Sleep);
        assert!(
            sleepy_ticks < lazy_ticks,
            "sleepy {sleepy_ticks} should beat {lazy_ticks}",
        );
    }

    #[test]
    fn sleep_now_short_circuits_the_idle_chain() {
        let mut neko = neko();
        neko.sleep_now();
        assert_eq!(neko.state(), State::Sleep);

        // It still wakes up normally afterwards.
        neko.tick((900, 700));
        assert_eq!(neko.state(), State::Awake);
    }

    #[test]
    fn sleeping_breathes_at_a_quarter_of_the_animation_rate() {
        let mut neko = neko();
        let cursor = neko.position();
        tick_until(&mut neko, cursor, |n| n.state() == State::Sleep);

        let frames: Vec<_> = (0..8)
            .map(|_| {
                neko.tick(cursor);
                neko.frame()
            })
            .collect();
        assert_eq!(
            frames,
            [
                "sleep1", "sleep1", "sleep1", "sleep2", "sleep2", "sleep2", "sleep2", "sleep1"
            ],
        );
    }

    #[test]
    fn every_state_names_two_real_frames() {
        let states = [
            State::Stop,
            State::Jare,
            State::Kaki,
            State::Akubi,
            State::Sleep,
            State::Awake,
            State::Move(Direction::Up),
            State::Scratch(Wall::Down),
        ];
        for state in states {
            for frame in state.frames() {
                assert!(!frame.is_empty(), "{state:?}");
            }
        }
    }
}
