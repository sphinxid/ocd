//! One-cell motions (`src/ui/one-cell-motion.ts`, `component/one-cell-spinner.tsx`): the startup
//! seed, the work spinners and the frame/level at a given elapsed time.

/// Row-major 2x4 octant masks that Unicode maps to older block glyphs.
const OCTANTS: [(u32, char); 26] = [
    (0x00, ' '),
    (0x01, '\u{1cea8}'),
    (0x02, '\u{1ceab}'),
    (0x03, '\u{1fb82}'),
    (0x05, '\u{2598}'),
    (0x0a, '\u{259d}'),
    (0x0f, '\u{2580}'),
    (0x14, '\u{1fbe6}'),
    (0x28, '\u{1fbe7}'),
    (0x3f, '\u{1fb85}'),
    (0x40, '\u{1cea3}'),
    (0x50, '\u{2596}'),
    (0x55, '\u{258c}'),
    (0x5a, '\u{259e}'),
    (0x5f, '\u{259b}'),
    (0x80, '\u{1cea0}'),
    (0xa0, '\u{2597}'),
    (0xa5, '\u{259a}'),
    (0xaa, '\u{2590}'),
    (0xaf, '\u{259c}'),
    (0xc0, '\u{2582}'),
    (0xf0, '\u{2584}'),
    (0xf5, '\u{2599}'),
    (0xfa, '\u{259f}'),
    (0xfc, '\u{2586}'),
    (0xff, '\u{2588}'),
];

/// `octantGlyph(mask)`.
pub fn octant_glyph(mask: u32) -> String {
    if let Some((_, c)) = OCTANTS.iter().find(|(m, _)| *m == mask) {
        return c.to_string();
    }
    let below = OCTANTS.iter().filter(|(m, _)| *m < mask).count() as u32;
    char::from_u32(0x1cd00 + mask - below).map(|c| c.to_string()).unwrap_or_default()
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pace {
    pub initial: f64,
    pub r#final: f64,
    pub after: f64,
    pub duration: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Clip {
    pub frames: Vec<String>,
    pub interval: f64,
    pub levels: Option<Vec<f64>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Motion {
    pub frames: Vec<String>,
    pub interval: f64,
    pub levels: Option<Vec<f64>>,
    pub intro: Option<Clip>,
    pub once: bool,
    pub pace: Option<Pace>,
}

impl Motion {
    fn simple(frames: Vec<String>, interval: f64) -> Motion {
        Motion { frames, interval, levels: None, intro: None, once: false, pace: None }
    }

    /// Sequenced motions (intro, once or pace) are stepped by elapsed time; others cycle frames.
    pub fn sequenced(&self) -> bool {
        self.intro.is_some() || self.once || self.pace.is_some()
    }
}

pub struct Frame {
    pub glyph: String,
    pub level: f64,
    pub complete: bool,
}

/// `oneCellFrame(animation, elapsed)`.
pub fn one_cell_frame(m: &Motion, elapsed: f64) -> Frame {
    let intro_duration = m.intro.as_ref().map(|i| i.frames.len() as f64 * i.interval).unwrap_or(0.0);
    let intro = elapsed < intro_duration;
    let (frames, interval, levels) = if intro {
        let i = m.intro.as_ref().unwrap();
        (&i.frames, i.interval, i.levels.as_ref())
    } else {
        (&m.frames, m.interval, m.levels.as_ref())
    };
    let time = if intro {
        elapsed
    } else if let Some(p) = m.pace {
        let progress = ((elapsed - p.after) / p.duration).clamp(0.0, 1.0);
        (elapsed - intro_duration) * p.initial + (p.r#final - p.initial) * (p.duration * (progress.powi(3) - progress.powi(4) / 2.0) + (elapsed - p.after - p.duration).max(0.0))
    } else {
        elapsed - intro_duration
    };
    let n = frames.len() as f64;
    let frame = ((time * n) / (interval * n)).floor() as i64;
    let len = frames.len() as i64;
    let index = if !intro && m.once { frame.min(len - 1) } else { frame.rem_euclid(len) } as usize;
    Frame { glyph: frames[index].clone(), level: levels.and_then(|l| l.get(index).copied()).unwrap_or(1.0), complete: !intro && m.once && frame >= len - 1 }
}

fn seed_work_levels() -> Vec<f64> {
    (0..40).map(|i| 0.3 + (0.7 * (1.0 - ((i as f64 / 40.0) * 2.0 * std::f64::consts::PI).cos())) / 2.0).collect()
}

pub fn seed_work() -> Motion {
    Motion {
        frames: vec!["\u{25aa}".into(); 40],
        interval: 40.0,
        levels: Some(seed_work_levels()),
        intro: Some(Clip {
            frames: vec!["\u{25aa}".into(); 80],
            interval: 40.0,
            levels: Some(
                (0..80)
                    .map(|i| {
                        let phase = i as f64 / 80.0;
                        0.3 + 0.7 * if phase < 0.125 { phase / 0.125 } else if phase < 0.375 { 1.0 } else { ((1.0 - phase) / 0.625).powi(3) }
                    })
                    .collect(),
            ),
        }),
        once: false,
        pace: Some(Pace { initial: 1.25, r#final: 0.5, after: 30_000.0, duration: 30_000.0 }),
    }
}

/// `SEED_LAUNCH`: the startup seed (21 frames of 40 ms, `once`).
pub fn seed_launch() -> Motion {
    Motion {
        frames: (0..21).map(|i| if i < 10 { "\u{25ab}" } else { "\u{25aa}" }.to_string()).collect(),
        interval: 40.0,
        levels: Some(seed_work_levels()[..21].to_vec()),
        intro: None,
        once: true,
        pace: None,
    }
}

pub fn seed_mono() -> Motion {
    Motion::simple(vec!["-".into(), "\\".into(), "|".into(), "/".into()], 120.0)
}

const WORK_PACE: Pace = Pace { initial: 1.2, r#final: 0.96, after: 30_000.0, duration: 30_000.0 };
const LOWER: [u32; 6] = [2, 3, 5, 7, 6, 4];

fn masks(m: &[u32]) -> Vec<String> {
    m.iter().map(|x| octant_glyph(*x)).collect()
}

fn chars(s: &str) -> Vec<String> {
    s.chars().map(|c| c.to_string()).collect()
}

/// `WORK_SPINNERS[name]`.
pub fn work_spinner(name: &str) -> Motion {
    match name {
        "block-soft-sweep" => Motion { pace: Some(WORK_PACE), ..Motion::simple(masks(&[0x14, 0x14, 0x14, 0x1c, 0x3c, 0x38, 0x28, 0x28, 0x28, 0x38, 0x3c, 0x1c]), 100.0) },
        "block-low-comet" => {
            let mut frames = Vec::new();
            for (i, point) in LOWER.iter().enumerate() {
                let leading = (1 << point) | (1 << LOWER[(i + 5) % 6]);
                let full = leading | (1 << LOWER[(i + 4) % 6]);
                frames.extend(masks(&[full, full, full, leading, leading]));
            }
            Motion { pace: Some(WORK_PACE), ..Motion::simple(frames, 40.0) }
        }
        "block-low-duet" => {
            let mut frames = Vec::new();
            for (i, point) in LOWER[..3].iter().enumerate() {
                let heads = (1 << point) | (1 << LOWER[(i + 3) % 6]);
                let full = heads | (1 << LOWER[(i + 2) % 6]) | (1 << LOWER[(i + 5) % 6]);
                frames.extend(masks(&[full, full, full, heads]));
            }
            Motion::simple(frames, 60.0)
        }
        "block-shuttle" => Motion::simple(masks(&[0x14, 0x28]), 500.0),
        "block-bridge" => Motion::simple(masks(&[0x14, 0x14, 0x14, 0x3c, 0x28, 0x28, 0x28, 0x3c]), 120.0),
        "block-squeeze" => Motion::simple(masks(&[0x14, 0x14, 0x14, 0x10, 0x20, 0x28, 0x28, 0x28, 0x20, 0x10]), 100.0),
        "small-toggle" => Motion::simple(chars("\u{25ab}\u{25aa}"), 320.0),
        "square-toggle" => Motion::simple(chars("\u{25a1}\u{25a0}"), 500.0),
        "grow-shrink" => Motion::simple(chars("\u{25ab}\u{25ab}\u{25aa}\u{25a0}\u{25aa}"), 180.0),
        "quadrant-orbit" => Motion::simple(chars("\u{2598}\u{259d}\u{2597}\u{2596}"), 160.0),
        "crosshatch" => Motion::simple(chars("\u{25a7}\u{25a9}\u{25a8}\u{25a9}"), 240.0),
        "density-wave" => Motion::simple(chars("\u{2591}\u{2591}\u{2591}\u{2592}\u{2593}\u{2588}\u{2593}\u{2592}"), 160.0),
        "seed" => seed_work(),
        _ => Motion {
            levels: Some(vec![0.55, 0.85, 1.0, 0.85, 0.55, 0.55, 0.85, 1.0, 0.85, 0.55]),
            ..Motion::simple(masks(&[0x14, 0x14, 0x14, 0x14, 0x14, 0x28, 0x28, 0x28, 0x28, 0x28]), 100.0)
        },
    }
}

/// The glyph and alpha level of a `OneCellSpinner` `age` ms after it appeared. Non-sequenced
/// spinners cycle `frames` every `interval`, with `levels` as per-frame alpha (glow).
pub fn spinner_frame(m: &Motion, age: f64, glow: bool) -> (String, f64) {
    if m.sequenced() {
        let f = one_cell_frame(m, age);
        return (f.glyph, if glow { f.level } else { 1.0 });
    }
    let index = ((age / m.interval).floor() as i64).rem_euclid(m.frames.len() as i64) as usize;
    let level = if glow { m.levels.as_ref().and_then(|l| l.get(index).copied()).unwrap_or(1.0) } else { 1.0 };
    (m.frames[index].clone(), level)
}
