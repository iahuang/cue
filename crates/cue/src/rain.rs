//! Rain mode: the screen as a fogged-up window in the rain. Drops land,
//! bead up, and run down the glass in fits and starts, wiping clear
//! trails that slowly mist over again. Moving the mouse wipes the fog
//! like a finger would.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use opentui::{Attributes, Buffer, Rgba};

use crate::theme;

/// How far backgrounds turn to the fog's color, where it's thickest.
const FOG_BG: f32 = 0.5;
/// How far text fades into the fogged background, where it's thickest.
const FOG_TEXT: f32 = 0.45;
/// The haze the fog is, on a dark theme, and how much of it the fog color
/// is, the rest being the theme's background.
const HAZE_DARK: (Rgba, f32) = (Rgba::rgb(170, 182, 200), 0.3);
/// The same on a light theme.
const HAZE_LIGHT: (Rgba, f32) = (Rgba::WHITE, 0.55);
/// How thick the fog settles: its thinnest, and its thickest. The texture
/// varies between the two.
const FOG_DENSITY: (f32, f32) = (0.86, 1.0);
/// How often the rain moves.
const FRAME: Duration = Duration::from_millis(50);
/// How much of the fog comes back each second, once the window has
/// fogged up.
const MIST_RATE: f32 = 0.05;
/// How much comes back each second while it first fogs up.
const FOGGING_RATE: f32 = 0.4;
/// How long the window takes to fog up at first, in seconds.
const FOGGING: f32 = 2.5;
/// How long a drop's trail stays wet, in seconds.
const WET: f32 = 5.0;
/// Fog and wetness are shown in this many steps, so that a cell's colors
/// change only now and then as they come back, not every frame.
const LEVELS: f32 = 10.0;
/// Drops smaller than this stop running, and bead up where they are.
const STOPS: f32 = 0.45;
/// Drops this big cover two rows, and show what's behind them upside down.
const LENS: f32 = 1.3;
/// How many times a second the water in a trail shifts what it shows.
const RIPPLES: f32 = 4.0;

pub struct Rain {
    width: u32,
    height: u32,
    /// How thick each cell's fog is: 0 is clear glass.
    fog: Vec<f32>,
    /// How thick each cell's fog gets, left alone.
    texture: Vec<f32>,
    /// How recently a drop ran through each cell: 1 just now.
    wet: Vec<f32>,
    drops: Vec<Drop>,
    beads: Vec<Bead>,
    rng: u64,
    /// For what's random but has to stay put from frame to frame.
    seed: u64,
    last: Instant,
    /// Seconds since it started raining.
    age: f32,
}

/// A drop running down the glass, or stuck for a moment.
#[derive(Clone, Copy)]
struct Drop {
    x: u32,
    /// In rows, from the top: the bottom of the drop.
    y: f32,
    /// Rows a second.
    speed: f32,
    size: f32,
    /// Seconds until it gets going again.
    stall: f32,
    /// Which way it wanders: -1 or 1.
    drift: i32,
}

/// A droplet sitting on the glass, until a drop runs into it.
#[derive(Clone, Copy)]
struct Bead {
    x: u32,
    y: u32,
    size: f32,
}

impl Rain {
    pub fn new(now: Instant) -> Rain {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0x9e37_79b9, |time| time.as_nanos() as u64);
        Rain {
            width: 0,
            height: 0,
            fog: Vec::new(),
            texture: Vec::new(),
            wet: Vec::new(),
            drops: Vec::new(),
            beads: Vec::new(),
            rng: seed | 1,
            seed,
            last: now,
            age: 0.0,
        }
    }

    /// How long until the rain moves again.
    pub fn wakes_in(&self, now: Instant) -> Duration {
        FRAME.saturating_sub(now.saturating_duration_since(self.last))
    }

    /// Moves the rain on, on a screen `width` by `height`, if it's time to.
    /// Returns whether it moved.
    pub fn step(&mut self, now: Instant, width: u32, height: u32) -> bool {
        if (width, height) != (self.width, self.height) {
            self.resize(width, height);
        }
        let elapsed = now.saturating_duration_since(self.last);
        if elapsed < FRAME || self.fog.is_empty() {
            return false;
        }
        self.last = now;
        // After a pause, such as a suspend, pick up where it left off.
        let dt = elapsed.as_secs_f32().min(0.2);
        self.age += dt;
        self.mist(dt);
        self.land(dt);
        self.run(dt);
        true
    }

    /// Wipes the fog around cell (`x`, `y`), and the droplets there.
    pub fn wipe(&mut self, x: u32, y: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        for col in x.saturating_sub(1)..=(x + 1).min(self.width - 1) {
            let i = self.index(col, y);
            self.fog[i] = if col == x { 0.0 } else { self.fog[i] * 0.3 };
            self.wet[i] = self.wet[i].max(0.5);
        }
        self.beads
            .retain(|bead| !(bead.y == y && bead.x.abs_diff(x) <= 1));
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        let cells = width as usize * height as usize;
        self.texture = (0..cells)
            .map(|i| {
                let (x, y) = ((i % width as usize) as f32, (i / width as usize) as f32);
                // Cells are about twice as tall as they're wide.
                let (thin, thick) = FOG_DENSITY;
                thin + (thick - thin) * noise(self.rng, x / 12.0, y / 5.0)
            })
            .collect();
        // Fogged up already, unless it's only starting to.
        self.fog = match self.age > 0.0 {
            true => self.texture.clone(),
            false => vec![0.0; cells],
        };
        self.wet = vec![0.0; cells];
        self.drops.retain(|drop| drop.x < width);
        self.beads.retain(|bead| bead.x < width && bead.y < height);
    }

    fn index(&self, x: u32, y: u32) -> usize {
        y as usize * self.width as usize + x as usize
    }

    /// Lets the fog come back, and trails dry.
    fn mist(&mut self, dt: f32) {
        let rate = match self.age < FOGGING {
            true => FOGGING_RATE,
            false => MIST_RATE,
        };
        for ((fog, &texture), wet) in self.fog.iter_mut().zip(&self.texture).zip(&mut self.wet) {
            *fog = (*fog + rate * dt).min(texture.max(*fog));
            *wet = (*wet - dt / WET).max(0.0);
        }
    }

    /// Lands new rain on the glass: mostly droplets, which merge with any
    /// they land on, and now and then a drop big enough to run. Most of
    /// those land above the window and run into it from the top, so trails
    /// cover its top as much as its bottom.
    fn land(&mut self, dt: f32) {
        let cells = self.width as f32 * self.height as f32;
        let mut landing = cells / 600.0 * dt;
        while landing > 0.0 {
            if landing < 1.0 && self.random() >= landing {
                break;
            }
            landing -= 1.0;
            let x = self.below(self.width);
            let y = self.below(self.height);
            if self.random() < 0.15 {
                let y = match self.random() < 0.75 {
                    true => -self.random() * self.height as f32 * 0.5,
                    false => y as f32 + self.random(),
                };
                let drop = Drop {
                    x,
                    y,
                    speed: 0.0,
                    size: 1.0 + 0.8 * self.random(),
                    stall: 0.2 + 0.6 * self.random(),
                    drift: self.sign(),
                };
                self.drops.push(drop);
                continue;
            }
            let size = 0.15 + 0.5 * self.random();
            let near = self
                .beads
                .iter()
                .position(|bead| bead.y == y && bead.x.abs_diff(x) <= 1);
            match near {
                Some(i) => {
                    self.beads[i].size += size;
                    // Heavy enough to run.
                    if self.beads[i].size > 1.0 {
                        let bead = self.beads.remove(i);
                        let drift = self.sign();
                        self.drops.push(Drop {
                            x: bead.x,
                            y: bead.y as f32 + 0.5,
                            speed: 0.0,
                            size: bead.size,
                            stall: 0.3,
                            drift,
                        });
                    }
                }
                None => self.beads.push(Bead { x, y, size }),
            }
        }
        // The oldest evaporate, so the glass doesn't fill up.
        let most = (cells / 18.0) as usize;
        if self.beads.len() > most {
            self.beads.drain(..self.beads.len() - most);
        }
    }

    /// Runs the drops down the glass: they speed up, stick for a moment,
    /// wander, pick up the droplets in their way, and leave some behind.
    fn run(&mut self, dt: f32) {
        let mut i = 0;
        while i < self.drops.len() {
            let mut drop = self.drops[i];
            if drop.stall > 0.0 {
                drop.stall -= dt;
                if drop.y >= 0.0 {
                    let row = drop.y as u32;
                    if row < self.height {
                        let at = self.index(drop.x, row);
                        self.fog[at] = 0.0;
                    }
                }
            } else {
                let gravity = 5.0 + 9.0 * drop.size;
                drop.speed = (drop.speed + gravity * dt).min(3.0 + 12.0 * drop.size);
                // Smaller drops catch on the glass more.
                if self.random() < dt * 0.5 / drop.size {
                    drop.stall = 0.08 + 0.5 * self.random();
                    drop.speed = 0.0;
                }
                let from = drop.y.floor() as i64;
                drop.y += drop.speed * dt;
                for row in from + 1..=drop.y.floor() as i64 {
                    let roll = self.random();
                    if roll < 0.13 {
                        if roll >= 0.1 {
                            drop.drift = -drop.drift;
                        }
                        let x = drop.x as i64 + drop.drift as i64;
                        drop.x = x.clamp(0, self.width as i64 - 1) as u32;
                    }
                    if (0..self.height as i64).contains(&row) {
                        self.trail(&mut drop, row as u32);
                    }
                    if drop.size < STOPS {
                        break;
                    }
                }
            }
            if drop.size < STOPS {
                self.drops.swap_remove(i);
                if (0.0..self.height as f32).contains(&drop.y) {
                    self.beads.push(Bead {
                        x: drop.x,
                        y: drop.y as u32,
                        size: drop.size,
                    });
                }
            } else if drop.y >= self.height as f32 + 1.0 {
                self.drops.swap_remove(i);
            } else {
                self.drops[i] = drop;
                i += 1;
            }
        }
    }

    /// Runs `drop` through `row`: wipes the fog there and picks up the
    /// droplets, losing a little water to the glass.
    fn trail(&mut self, drop: &mut Drop, row: u32) {
        let at = self.index(drop.x, row);
        self.fog[at] = 0.0;
        self.wet[at] = 1.0;
        let big = drop.size > 1.4;
        let side = drop.x as i64 + drop.drift as i64;
        if big && (0..self.width as i64).contains(&side) {
            let at = self.index(side as u32, row);
            self.fog[at] *= 0.35;
            self.wet[at] = self.wet[at].max(0.6);
        }
        let reach = if big { 1 } else { 0 };
        let mut picked = 0.0;
        self.beads.retain(|bead| {
            let hit = bead.y == row && bead.x.abs_diff(drop.x) <= reach;
            if hit {
                picked += bead.size * 0.7;
            }
            !hit
        });
        drop.size = (drop.size + picked - 0.004).min(2.5);
        if row > 0 && self.random() < 0.07 {
            let size = 0.12 + 0.15 * self.random();
            drop.size -= size * 0.6;
            self.beads.push(Bead {
                x: drop.x,
                y: row - 1,
                size,
            });
        }
    }

    /// Fogs `frame` over, and draws the rain on it.
    pub fn draw(&self, frame: &Buffer) {
        if self.fog.is_empty() || (frame.width(), frame.height()) != (self.width, self.height) {
            return;
        }
        let colors = theme::colors();
        let light = colors.light;
        let (haze, share) = match light {
            true => HAZE_LIGHT,
            false => HAZE_DARK,
        };
        let fog_color = theme::mix(colors.bg, haze, share);
        // Water on the glass: catching the light, against a dark screen,
        // or casting shade, against a light one.
        let (shine, streak) = match light {
            true => (Rgba::rgb(110, 125, 145), Rgba::rgb(185, 195, 210)),
            false => (Rgba::WHITE, Rgba::BLACK),
        };
        let glint = theme::mix(fog_color, shine, 0.45);

        self.refract(frame);

        // Droplets see through the fog a little.
        let mut clear = vec![0.0f32; self.fog.len()];
        for bead in &self.beads {
            let at = self.index(bead.x, bead.y);
            clear[at] = clear[at].max((bead.size * 0.8).min(0.8));
        }
        let step = |value: f32| (value * LEVELS).round() / LEVELS;
        frame.map_colors(0, 0, self.width, self.height, |x, y, fg, bg| {
            let at = self.index(x, y);
            let fog = step(self.fog[at] * (1.0 - clear[at]));
            let wet = step(self.wet[at]) * (1.0 - fog);
            if fog == 0.0 && wet == 0.0 {
                return (fg, bg);
            }
            let mut bg = theme::mix(bg, fog_color, fog * FOG_BG);
            if wet > 0.0 {
                bg = theme::mix(bg, streak, wet * 0.1);
            }
            (theme::mix(fg, bg, fog * FOG_TEXT), bg)
        });

        for bead in &self.beads {
            let glyph = match bead.size {
                size if size < 0.35 => '·',
                size if size < 0.8 => '•',
                _ => '●',
            };
            let fg = theme::mix(glint, shine, (bead.size * 0.3).min(0.3));
            self.dab(
                frame,
                bead.x,
                bead.y,
                glyph,
                fg,
                shine,
                0.12 * bead.size.min(1.0),
            );
        }
        for drop in &self.drops {
            if drop.y < 0.0 || drop.y >= self.height as f32 {
                continue;
            }
            let row = drop.y as u32;
            if drop.size >= LENS && row > 0 {
                self.invert(frame, drop.x, row - 1, row);
                self.dab(frame, drop.x, row - 1, ' ', glint, shine, 0.15);
            }
            let glyph = if drop.size >= 1.0 { '●' } else { '•' };
            self.dab(
                frame,
                drop.x,
                row,
                glyph,
                theme::mix(glint, shine, 0.5),
                shine,
                0.3,
            );
        }
    }

    /// Lights cell (`x`, `y`) with `share` of `shine`, and if it's blank,
    /// draws `glyph` there in `fg`.
    #[allow(clippy::too_many_arguments)]
    fn dab(&self, frame: &Buffer, x: u32, y: u32, glyph: char, fg: Rgba, shine: Rgba, share: f32) {
        if glyph != ' ' && frame.char_at(x, y).is_some_and(blank) {
            let bg = frame.bg_at(x, y).unwrap_or(Rgba::BLACK);
            let bg = theme::mix(bg, shine, share);
            frame.draw_char(glyph, x, y, fg, bg, Attributes::NONE);
        } else {
            frame.map_colors(x, y, 1, 1, |_, _, fg, bg| {
                (theme::mix(fg, shine, share), theme::mix(bg, shine, share))
            });
        }
    }

    /// Bends what shows through the water in the trails: a wet cell shows
    /// the character next to it, or above or below it, the more often the
    /// wetter it is, shifting a few times a second as the water moves.
    fn refract(&self, frame: &Buffer) {
        let mut bent = Vec::new();
        for y in 0..self.height {
            for x in 0..self.width {
                let wet = self.wet[self.index(x, y)];
                let Some(here) = frame.char_at(x, y).filter(|_| wet > 0.0) else {
                    continue;
                };
                // Each cell ripples on its own beat.
                let phase = (hash(self.seed, x, y, 0) & 0xff) as f32 / 256.0;
                let tick = (self.age * RIPPLES + phase) as u64;
                let roll = hash(self.seed, x, y, tick + 1);
                if (roll & 0xffff) as f32 / 65536.0 >= wet * 0.6 {
                    continue;
                }
                // Mostly sideways, across the trail.
                let (dx, dy) = match (roll >> 16) % 6 {
                    0 | 1 => (-1, 0),
                    2 | 3 => (1, 0),
                    4 => (0, -1),
                    _ => (0, 1),
                };
                let (Ok(from_x), Ok(from_y)) =
                    (u32::try_from(x as i64 + dx), u32::try_from(y as i64 + dy))
                else {
                    continue;
                };
                let Some((ch, fg, _, attributes)) = plain_cell(frame, from_x, from_y) else {
                    continue;
                };
                // Up and down only within text, which would otherwise spill
                // into the blank lines around it.
                if dy != 0 && (blank(ch) || blank(here)) {
                    continue;
                }
                bent.push((x, y, ch, fg, attributes));
            }
        }
        // All read before any is written, so none shows one already bent.
        for (x, y, ch, fg, attributes) in bent {
            let bg = frame.bg_at(x, y).unwrap_or(Rgba::BLACK);
            frame.draw_char(ch, x, y, fg, bg, attributes);
        }
    }

    /// Swaps the characters of rows `top` and `bottom` in column `x`, as a
    /// drop over them shows them, if both are plain ones.
    fn invert(&self, frame: &Buffer, x: u32, top: u32, bottom: u32) {
        let (Some(upper), Some(lower)) = (plain_cell(frame, x, top), plain_cell(frame, x, bottom))
        else {
            return;
        };
        frame.draw_char(lower.0, x, top, lower.1, upper.2, lower.3);
        frame.draw_char(upper.0, x, bottom, upper.1, lower.2, upper.3);
    }

    /// A random number in [0, 1).
    fn random(&mut self) -> f32 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        let value = self.rng.wrapping_mul(0x2545_f491_4f6c_dd1d);
        (value >> 40) as f32 / (1u64 << 24) as f32
    }

    /// A random number below `n`, which isn't 0.
    fn below(&mut self, n: u32) -> u32 {
        ((self.random() * n as f32) as u32).min(n - 1)
    }

    fn sign(&mut self) -> i32 {
        if self.random() < 0.5 {
            -1
        } else {
            1
        }
    }
}

/// Whether `ch` shows nothing: a space, or the NUL of a cell nothing was
/// drawn in.
fn blank(ch: char) -> bool {
    ch == '\0' || ch.is_whitespace()
}

/// Cell (`x`, `y`)'s character, colors, and attributes, if the character
/// is a plain one.
fn plain_cell(frame: &Buffer, x: u32, y: u32) -> Option<(char, Rgba, Rgba, Attributes)> {
    Some((
        frame.char_at(x, y)?,
        frame.fg_at(x, y)?,
        frame.bg_at(x, y)?,
        frame.attributes_at(x, y)?,
    ))
}

/// Random bits that are always the same for the same arguments.
fn hash(seed: u64, x: u32, y: u32, z: u64) -> u64 {
    let mut h = seed
        ^ (x as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ (y as u64).wrapping_mul(0xc2b2_ae3d_27d4_eb4f)
        ^ z.wrapping_mul(0x1656_67b1_9e37_79f9);
    h ^= h >> 29;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 32;
    h
}

/// Smooth value noise in [0, 1] at (`x`, `y`), for the fog's texture.
fn noise(seed: u64, x: f32, y: f32) -> f32 {
    // The lattice is at whole cells, never negative.
    let hash =
        |x: i64, y: i64| (hash(seed, x as u32, y as u32, 0) >> 40) as f32 / (1u64 << 24) as f32;
    let (x0, y0) = (x.floor(), y.floor());
    let smooth = |t: f32| t * t * (3.0 - 2.0 * t);
    let (tx, ty) = (smooth(x - x0), smooth(y - y0));
    let (x0, y0) = (x0 as i64, y0 as i64);
    let top = hash(x0, y0) + (hash(x0 + 1, y0) - hash(x0, y0)) * tx;
    let bottom = hash(x0, y0 + 1) + (hash(x0 + 1, y0 + 1) - hash(x0, y0 + 1)) * tx;
    top + (bottom - top) * ty
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_fogs_up_and_drops_clear_it() {
        let start = Instant::now();
        let mut rain = Rain::new(start);
        rain.step(start, 80, 24);
        let mut now = start;
        for _ in 0..200 {
            now += FRAME;
            rain.step(now, 80, 24);
        }
        let fogged = rain.fog.iter().filter(|&&fog| fog > 0.7).count();
        assert!(fogged > rain.fog.len() / 2, "mostly fogged: {fogged}");
        rain.drops.push(Drop {
            x: 10,
            y: 0.5,
            speed: 0.0,
            size: 1.5,
            stall: 0.0,
            drift: 1,
        });
        for _ in 0..40 {
            now += FRAME;
            rain.step(now, 80, 24);
        }
        let cleared = (1..10)
            .filter(|&y| (4..=16).any(|x| rain.fog[rain.index(x, y)] < 0.5))
            .count();
        assert!(cleared >= 5, "a trail down from the top: {cleared}");
    }

    #[test]
    fn trails_dont_spill_text_into_blank_lines() {
        use opentui::{OwnedBuffer, WidthMethod};
        let start = Instant::now();
        let mut rain = Rain::new(start);
        rain.step(start, 20, 3);
        let frame = OwnedBuffer::new(20, 3, false, WidthMethod::Unicode, "rain").unwrap();
        rain.wet.fill(1.0);
        for tick in 0..50 {
            rain.age = tick as f32 / RIPPLES;
            frame.clear(Rgba::BLACK);
            frame.draw_text(
                "abcdefghijklmnopqrst",
                0,
                0,
                Rgba::WHITE,
                None,
                Attributes::NONE,
            );
            frame.draw_text(
                "\0\0\0\0\0\0\0\0\0\0",
                0,
                1,
                Rgba::WHITE,
                None,
                Attributes::NONE,
            );
            rain.refract(&frame);
            let text = frame.to_text(true);
            let row = text.lines().nth(1).unwrap();
            assert!(row.chars().all(blank), "tick {tick}: {text:?}");
        }
    }

    #[test]
    fn wiping_clears_the_fog() {
        let start = Instant::now();
        let mut rain = Rain::new(start);
        rain.step(start, 20, 5);
        rain.fog.fill(1.0);
        rain.wipe(5, 2);
        assert_eq!(rain.fog[rain.index(5, 2)], 0.0);
        assert!(rain.fog[rain.index(4, 2)] < 0.5);
        assert_eq!(rain.fog[rain.index(5, 1)], 1.0);
    }
}
