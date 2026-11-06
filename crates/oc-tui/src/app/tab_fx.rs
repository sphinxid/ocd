//! The per-tab motion of `component/session-tabs.tsx` (2.2.5): the `TabPulse`s of the vertical rail (the separators
//! above each tab and below the last, the title row, the detail row; #60338 made them span the full rail width) and
//! of the horizontal strip, the number color's ignition and sweep brightening, and the glow levels and hues.
//!
//! Each tab's state lives while the tab is drawn in that layout (a remount in upstream starts it over). The pulses
//! advance by the time between frames: upstream ticks them at 20 fps by elapsed time, so both show the same state at
//! the same moment within a frame.

use super::App;
use crate::theme::{Rgba, Theme, tint};
use crate::ui::tab_pulse::{Edge, GLOW_TAIL, PulseColors, Rgbf, TabPulse, Tween, Voice};
use ratatui::buffer::Buffer;
use std::time::Instant;

/// A tab's status as `session-tabs.tsx` derives it (`statuses()`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FxStatus {
    pub attention: bool,
    pub unread: Option<&'static str>,
    pub busy: bool,
    pub prompt_pulse: u64,
    /// `sessionTabComplete(unread, busy)`.
    pub complete: bool,
    /// `busy && !attention`.
    pub runs: bool,
    pub glows: bool,
}

impl FxStatus {
    fn voice(&self) -> Voice {
        Voice { active: self.runs, prompt_pulse: self.prompt_pulse, complete: self.complete && !self.attention, glow: self.glows }
    }
}

/// `tabFeedbackColor`.
pub fn feedback_color(st: &FxStatus, t: &Theme) -> Option<Rgba> {
    if st.attention {
        return Some(t.accent(200));
    }
    if st.unread == Some("error") {
        return Some(t.text.feedback.error.base);
    }
    None
}

/// The vertical rail's state for one tab.
#[derive(Clone, Debug)]
pub struct RailFx {
    pub above: TabPulse,
    pub below: TabPulse,
    pub title: TabPulse,
    pub detail: TabPulse,
    glow_level: Tween,
    prev_glow_level: Tween,
    number_glow: Tween,
    title_glow: Tween,
    ignition: Tween,
    was_running: bool,
    prompt: u64,
    last_hue: Option<Rgba>,
    last_prev_hue: Option<Rgba>,
}

/// The horizontal strip's state for one tab.
#[derive(Clone, Debug)]
pub struct StripFx {
    pub pulse: TabPulse,
    glow_level: Tween,
    ignition: Tween,
    was_running: bool,
    prompt: u64,
}

/// `createGlowLevel`: 0.7 while dimmed (the selected tab needing attention), else 1, over 0.2 s.
fn glow_target(dimmed: bool) -> f64 {
    if dimmed { 0.7 } else { 1.0 }
}

/// `createNumberIgnition`: a run starting or stopping, or a new prompt, lights the number (0.85) and lets it fade
/// over 0.7 s.
fn ignite(ignition: &mut Tween, was_running: &mut bool, prompt: &mut u64, st: &FxStatus, now: f64, animations: bool) {
    if st.runs != *was_running || st.prompt_pulse != *prompt {
        ignition.jump(0.85);
        ignition.animate(0.0, now, animations);
    }
    *was_running = st.runs;
    *prompt = st.prompt_pulse;
}

const SHIMMER_DURATION: f64 = 1200.0;
const SHIMMER_FADE: f64 = 240.0;
const ARRIVAL_DURATION: f64 = 450.0;
const WIPE_FEATHER: f64 = 4.0;

/// `TitleShimmerRenderable` (`component/title-shimmer.tsx`): while a tab's title is being generated, a light band
/// sweeps its characters (they fade toward the backdrop away from the band); the new title then wipes in over the
/// old one (450 ms, 4-cell feather).
#[derive(Clone, Debug, Default)]
pub struct TitleShimmer {
    rename: Option<(String, bool)>,
    pending_title: Option<String>,
    enabled: bool,
    elapsed: f64,
    blend: f64,
    fresh: bool,
    live: bool,
    arrival: Option<f64>,
    /// The last painted title cells (symbol, fg, modifiers), the wipe's old side.
    previous: Option<Vec<(String, ratatui::style::Color, ratatui::style::Modifier)>>,
}

impl TitleShimmer {
    fn new() -> TitleShimmer {
        TitleShimmer { enabled: true, fresh: true, ..Default::default() }
    }

    fn shimmering(&self) -> bool {
        self.rename.as_ref().is_some_and(|(title, pending)| *pending && Some(title) == self.pending_title.as_ref())
    }

    pub fn animating(&self) -> bool {
        self.enabled && (self.shimmering() || self.arrival.is_some() || self.blend > 0.0)
    }

    fn changed(&mut self) {
        if !self.live && self.animating() {
            self.fresh = true;
        }
        self.live = self.animating();
        if !self.animating() {
            self.previous = None;
        }
    }

    /// The `rename` and `enabled` props.
    fn set(&mut self, title: &str, pending: bool, enabled: bool) {
        if enabled != self.enabled {
            self.enabled = enabled;
            if !enabled {
                self.arrival = None;
                self.blend = 0.0;
            }
            self.changed();
        }
        let current = self.rename.clone();
        if current.as_ref().is_some_and(|(t, p)| t == title && *p == pending) {
            return;
        }
        let was_pending = current.as_ref().is_some_and(|(_, p)| *p);
        if pending && !was_pending {
            if self.pending_title.as_deref() != Some(title) {
                self.blend = 0.0;
            }
            self.pending_title = Some(title.to_string());
            if self.blend == 0.0 {
                self.elapsed = 0.0;
            }
            self.arrival = None;
            self.previous = None;
        }
        // Only an automatic rename replaces the last painted title with a wipe.
        if current.as_ref().map(|(t, _)| t.as_str()) != Some(title) {
            self.arrival = (was_pending && self.enabled && self.previous.is_some()).then_some(0.0);
            if self.arrival.is_none() {
                self.blend = 0.0;
            }
        }
        self.rename = Some((title.to_string(), pending));
        self.changed();
    }

    /// `renderSelf` over the title already drawn at (`x`, `y`), `width` cells: advance by `delta` and shade.
    fn render(&mut self, buf: &mut Buffer, x: u16, y: u16, width: usize, backdrop: Rgba, delta: f64) {
        if !self.animating() || width == 0 || y >= buf.area.bottom() {
            return;
        }
        let delta = if self.fresh { 0.0 } else { delta };
        self.fresh = false;
        self.elapsed = (self.elapsed + delta) % SHIMMER_DURATION;
        if let Some(a) = self.arrival {
            let a = a + delta;
            self.arrival = Some(a);
            if a >= ARRIVAL_DURATION {
                self.arrival = None;
                self.blend = 0.0;
            }
        }
        let towards = if self.shimmering() || self.arrival.is_some() { delta } else { -delta };
        self.blend = (self.blend + towards / SHIMMER_FADE).clamp(0.0, 1.0);
        self.live = self.animating();
        if !self.animating() {
            self.previous = None;
            return;
        }
        let width = width.min(buf.area.right().saturating_sub(x) as usize);
        let cells: Vec<(String, ratatui::style::Color, ratatui::style::Modifier)> =
            (0..width).map(|i| {
                let c = &buf[(x + i as u16, y)];
                (c.symbol().to_string(), c.fg, c.modifier)
            }).collect();
        let end = cells.iter().rposition(|(s, _, _)| !(s == " " || s.is_empty())).map(|p| p + 1).unwrap_or(0);
        let wipe_front = match (&self.arrival, &self.previous) {
            (Some(a), Some(prev)) => Some(-WIPE_FEATHER + crate::ui::tab_pulse::coast(a / ARRIVAL_DURATION) * (end.max(prev.len()) as f64 + WIPE_FEATHER * 2.0)),
            _ => None,
        };
        let cut = wipe_front.map(|f| f.round().clamp(0.0, width as f64) as usize).unwrap_or(0);
        if let (Some(_), Some(prev)) = (wipe_front, &self.previous) {
            for col in cut..width {
                let cell = &mut buf[(x + col as u16, y)];
                match prev.get(col) {
                    Some((s, f, m)) => {
                        cell.set_symbol(s);
                        cell.set_fg(*f);
                        cell.modifier = *m;
                    }
                    None => {
                        cell.set_symbol(" ");
                    }
                }
            }
        }
        if wipe_front.is_none() {
            self.previous = Some(cells[..end.max(1).min(cells.len())].to_vec());
        }
        let span = self.previous.as_ref().map(|p| p.len()).unwrap_or(end) as f64;
        let front = -4.0 + crate::ui::tab_pulse::coast(self.elapsed / SHIMMER_DURATION) * (span + 4.0 + 18.0);
        let level = crate::ui::tab_pulse::smootherstep(self.blend);
        let b = crate::ui::tab_pulse::Rgbf::of(backdrop);
        for col in 0..width {
            let old = wipe_front.is_none() || col >= cut;
            let mut visibility = if old { 1.0 - 0.6 * level * (1.0 - crate::ui::tab_pulse::intensity_at(col as f64, front, 4.0, 18.0)) } else { 1.0 };
            if let Some(w) = wipe_front {
                let distance = if old { col as f64 - w } else { w - (col as f64 + 1.0) };
                visibility *= crate::ui::tab_pulse::smootherstep((distance / WIPE_FEATHER).clamp(0.0, 1.0));
            }
            let strength = 1.0 - visibility;
            let cell = &mut buf[(x + col as u16, y)];
            if let ratatui::style::Color::Rgb(r, g, bl) = cell.fg {
                let mix = |c: u8, t: f64| c as f64 / 255.0 + (t - c as f64 / 255.0) * strength;
                cell.set_fg(crate::ui::tab_pulse::Rgbf { r: mix(r, b.r), g: mix(g, b.g), b: mix(bl, b.b) }.color());
            }
        }
    }
}

impl App {
    /// The title shimmer of a tab (`rail` or strip), after its title was drawn.
    #[allow(clippy::too_many_arguments)]
    pub fn tab_title_shimmer(&mut self, buf: &mut Buffer, rail: bool, id: &str, x: u16, y: u16, width: usize, backdrop: Rgba, delta: f64) {
        let title = self.store.session(id).and_then(|s| s.title.clone()).unwrap_or_default();
        let pending = self.title_generating.contains(id);
        let enabled = self.config.animations();
        let map = if rail { &mut self.rail_shimmer } else { &mut self.strip_shimmer };
        let fresh = !map.contains_key(id);
        let s = map.entry(id.to_string()).or_insert_with(TitleShimmer::new);
        s.set(&title, pending, enabled);
        s.render(buf, x, y, width, backdrop, if fresh { 0.0 } else { delta });
    }
}

/// What the rail draws for one tab this frame.
pub struct RailLook {
    pub number: Rgba,
    /// `titleGlow` level and its color (`glowColor()`), for `glowTextColor`.
    pub title_glow: (f64, Rgba),
}

/// What the strip draws for one tab this frame.
pub struct StripLook {
    pub number: Rgba,
}

impl App {
    /// Milliseconds on the motion clock.
    pub fn fx_now(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0
    }

    /// The time since the last frame's motion step, and start a new step.
    pub fn fx_step(&mut self) -> f64 {
        let now = Instant::now();
        let delta = self.fx_last.map(|l| now.duration_since(l).as_secs_f64() * 1000.0).unwrap_or(0.0);
        self.fx_last = Some(now);
        delta
    }

    /// The status `session-tabs.tsx` computes for a tab (`statuses()`), `active` being the routed tab.
    pub fn fx_status(&self, id: &str, active: Option<&str>) -> FxStatus {
        let st = self.tab_status(id);
        let attention = st.attention.is_some();
        // `isUnread` reads the root session only, also for the routed one.
        let unread = self.store.session(id).and_then(|s| {
            let idle = s.time.idle?;
            if s.time.viewed.is_some_and(|v| idle <= v) {
                return None;
            }
            Some(if s.outcome.as_deref() == Some("failed") { "error" } else { "activity" })
        });
        FxStatus {
            attention,
            unread,
            busy: st.busy,
            prompt_pulse: self.prompt_pulses.get(id).copied().unwrap_or(0),
            complete: unread == Some("activity") && !st.busy,
            runs: st.busy && !attention,
            glows: attention || (Some(id) != active && !st.busy && unread.is_some()),
        }
    }

    /// `isUnread`: the session went idle after it was last viewed.
    pub fn session_unread(&self, id: &str) -> bool {
        self.store.session(id).is_some_and(|s| s.time.idle.is_some_and(|idle| s.time.viewed.is_none_or(|v| idle > v)))
    }

    /// The viewed-state acknowledgement (`context/session-tabs.tsx`): while the terminal has focus, the routed
    /// session's root, once unread, is reported viewed up to its idle time (`POST /api/session/:id/view`), once
    /// per watermark; a failure retries after 250 ms, doubling up to 5 s. It runs with tabs disabled too.
    pub fn acknowledge_view(&mut self) {
        if self.view_focus != Some(true) {
            return;
        }
        if let Some(at) = self.view_retry.0 {
            if Instant::now() < at {
                return;
            }
            self.view_retry.0 = None;
        }
        let Some(sid) = self.session_id().filter(|s| *s != "dummy").map(|s| s.to_string()) else { return };
        let root = self.store.root(&sid);
        let Some(idle) = self.store.session(&root).and_then(|s| s.time.idle) else { return };
        if !self.session_unread(&root) || self.view_acked.get(&root) == Some(&idle) {
            return;
        }
        self.view_acked.insert(root.clone(), idle);
        let c = self.client.clone();
        let r2 = root.clone();
        self.spawn(async move { c.view(&r2, idle).await }, move |app, r| match r {
            Ok(()) => app.view_retry = (None, 0),
            Err(_) => {
                if app.view_acked.get(&root) != Some(&idle) {
                    return;
                }
                app.view_acked.remove(&root);
                if app.view_retry.0.is_some() {
                    return;
                }
                let delay = (250u64 << app.view_retry.1.min(10)).min(5_000);
                app.view_retry = (Some(Instant::now() + std::time::Duration::from_millis(delay)), app.view_retry.1 + 1);
            }
        });
    }

    /// Whether a tab pulse or a tab motion still moves.
    pub fn tab_fx_animating(&self) -> bool {
        let now = self.fx_now();
        self.rail_fx.values().any(|f| f.above.live() || f.below.live() || f.title.live() || f.detail.live() || f.ignition.moving(now) || f.glow_level.moving(now) || f.number_glow.moving(now) || f.title_glow.moving(now))
            || self.strip_fx.values().any(|f| f.pulse.live() || f.ignition.moving(now) || f.glow_level.moving(now))
            || self.rail_shimmer.values().chain(self.strip_shimmer.values()).any(|s| s.animating())
    }

    /// One rail tab: update its motion, paint its pulses, and return its colors. `y` is the title row; the separator
    /// above is drawn when `above` is inside the rail, the one below only for the last tab.
    #[allow(clippy::too_many_arguments)]
    pub fn rail_tab_fx(&mut self, buf: &mut Buffer, t: &Theme, x: u16, y: u16, width: u16, id: &str, prev: Option<&str>, last: bool, selected: bool, tab_bg: Rgba, delta: f64, clip: (u16, u16)) -> RailLook {
        let animations = self.config.animations();
        let active = self.session_id().map(|s| self.store.root(s));
        let st = self.fx_status(id, active.as_deref());
        let pst = prev.map(|p| self.fx_status(p, active.as_deref())).unwrap_or_default();
        let prev_selected = prev.is_some() && prev == active.as_deref();
        let now = self.fx_now();
        let fresh = !self.rail_fx.contains_key(id);
        // OpenTUI's solid renderer constructs an element with its id only and then applies each prop through its
        // setter, so a tab mounting while it runs starts its sweep with the edge flash, and a glowing one ignites.
        // `createNumberIgnition` starts from the mount state; the title and number glows ease in from 0.
        let fx = self.rail_fx.entry(id.to_string()).or_insert_with(|| RailFx {
            above: TabPulse::new(Some(Edge::Above), true, Voice::default(), Voice::default()),
            below: TabPulse::new(Some(Edge::Below), true, Voice::default(), Voice::default()),
            title: TabPulse::new(None, true, Voice::default(), Voice::default()),
            detail: TabPulse::new(None, true, Voice::default(), Voice::default()),
            glow_level: Tween::new(glow_target(selected && st.attention), 0.2),
            prev_glow_level: Tween::new(glow_target(prev_selected && pst.attention), 0.2),
            number_glow: Tween::new(0.0, 0.4),
            title_glow: Tween::new(0.0, 0.4),
            ignition: Tween::new(0.0, 0.7),
            was_running: st.runs,
            prompt: st.prompt_pulse,
            last_hue: None,
            last_prev_hue: None,
        });
        // A tab mounted this frame starts from its mount state (no time passed under it).
        if !fresh {
            for p in [&mut fx.above, &mut fx.below, &mut fx.title, &mut fx.detail] {
                p.advance(delta);
            }
        }
        let v = st.voice();
        fx.above.set(animations, v, pst.voice());
        fx.below.set(animations, v, Voice::default());
        fx.title.set(animations, v, v);
        fx.detail.set(animations, v, v);
        fx.glow_level.animate(glow_target(selected && st.attention), now, animations);
        fx.prev_glow_level.animate(glow_target(prev_selected && pst.attention), now, animations);
        fx.number_glow.animate(if st.attention || st.unread == Some("error") || st.complete { 1.0 } else { 0.0 }, now, animations);
        fx.title_glow.animate(if st.glows { 1.0 } else { 0.0 }, now, animations);
        ignite(&mut fx.ignition, &mut fx.was_running, &mut fx.prompt, &st, now, animations);

        // `glowHue`, latched so a resolving glow fades in the hue it lit with.
        let unread_color = t.accent(200);
        let hue = match feedback_color(&st, t) {
            Some(f) => *fx.last_hue.insert(f),
            None if st.unread.is_some() => *fx.last_hue.insert(unread_color),
            None => fx.last_hue.unwrap_or(unread_color),
        };
        let prev_hue = match feedback_color(&pst, t) {
            Some(f) => *fx.last_prev_hue.insert(f),
            None if pst.unread.is_some() => *fx.last_prev_hue.insert(unread_color),
            None => fx.last_prev_hue.unwrap_or(unread_color),
        };
        let background = t.background.raised.base;
        let pulse_bg = tint(background, tab_bg, tab_bg.a as f64 / 255.0);
        let glow_level = fx.glow_level.value(now);
        let prev_glow_level = fx.prev_glow_level.value(now);
        let f = Rgbf::of;
        let base = t.text.base;
        let w = width as usize;
        let inside = |row: u16| row >= clip.0 && row < clip.1;
        // The separator above (`top=-1`, `▄`): the lower half is this tab, the upper half the previous one.
        let sep_lower = tint(background, hue, 0.12 * glow_level);
        let sep_upper = tint(background, prev_hue, 0.1 * prev_glow_level);
        if let Some(row) = y.checked_sub(1).filter(|r| inside(*r)) {
            let inner = PulseColors { color: f(tint(background, base, 0.05)), glow: f(sep_lower), flash: f(tint(background, base, 0.22)), completion: f(sep_lower) };
            let outer = PulseColors { color: f(tint(background, base, 0.04)), glow: f(sep_upper), flash: f(tint(background, base, 0.18)), completion: f(sep_upper) };
            fx.above.render(buf, x, row, w, f(background), &inner, &outer, 8.0, 5.0, Some(8.0));
        }
        // Below the last tab (`top=2`, `▀`).
        if last && inside(y + 2) {
            let glow = tint(background, hue, 0.1 * glow_level);
            let inner = PulseColors { color: f(tint(background, base, 0.04)), glow: f(glow), flash: f(tint(background, base, 0.18)), completion: f(glow) };
            let outer = PulseColors { color: f(tint(background, base, 0.006)), glow: f(background), flash: f(tint(background, base, 0.18)), completion: f(background) };
            fx.below.render(buf, x, y + 2, w, f(background), &inner, &outer, 8.0, 5.0, Some(8.0));
        }
        // The title row (its `onLevel` is the sweep level under the number) and the detail row.
        let glow_color = tint(pulse_bg, hue, 0.45 * glow_level);
        if inside(y) {
            let c = PulseColors { color: f(tint(pulse_bg, base, 0.25)), glow: f(glow_color), flash: f(tint(pulse_bg, base, 0.7)), completion: f(glow_color) };
            fx.title.render(buf, x, y, w, f(pulse_bg), &c, &c, GLOW_TAIL, GLOW_TAIL, Some(8.0));
        }
        let sweep = fx.title.level(w);
        if inside(y + 1) {
            let detail_glow = tint(pulse_bg, hue, 0.25 * glow_level);
            let c = PulseColors { color: f(tint(pulse_bg, base, 0.13)), glow: f(detail_glow), flash: f(tint(pulse_bg, base, 0.42)), completion: f(detail_glow) };
            fx.detail.render(buf, x, y + 1, w, f(pulse_bg), &c, &c, 10.0, 10.0, Some(8.0));
        }
        // `numberColor`.
        let idle_number = tint(t.text.formfield.base, background, 0.55);
        let number_base = tint(idle_number, tint(base, pulse_bg, 0.25), if selected { 1.0 } else { 0.0 });
        let color = feedback_color(&st, t).unwrap_or_else(|| tint(number_base, hue, fx.number_glow.value(now)));
        let running = if st.runs { t.interactive(200) } else { color };
        let ignition = fx.ignition.value(now);
        let number = if sweep == 0.0 { tint(running, base, ignition) } else { tint(running, base, ignition.max(0.35 * sweep)) };
        RailLook { number, title_glow: (fx.title_glow.value(now), glow_color) }
    }

    /// One strip tab: update its motion, paint its pulse under the tab, and return the number color.
    #[allow(clippy::too_many_arguments)]
    pub fn strip_tab_fx(&mut self, buf: &mut Buffer, t: &Theme, x: u16, y: u16, width: u16, id: &str, selected: bool, background: Rgba, delta: f64) -> StripLook {
        let animations = self.config.animations();
        let active = self.session_id().map(|s| self.store.root(s));
        let st = self.fx_status(id, active.as_deref());
        let now = self.fx_now();
        let fresh = !self.strip_fx.contains_key(id);
        let fx = self.strip_fx.entry(id.to_string()).or_insert_with(|| StripFx {
            pulse: TabPulse::new(None, true, Voice::default(), Voice::default()),
            glow_level: Tween::new(glow_target(selected && st.attention), 0.2),
            ignition: Tween::new(0.0, 0.7),
            was_running: st.runs,
            prompt: st.prompt_pulse,
        });
        if !fresh {
            fx.pulse.advance(delta);
        }
        // The strip's glow: attention, or an unread tab that is not selected and idle.
        let glows = st.attention || (!selected && !st.busy && st.unread.is_some());
        let v = Voice { glow: glows, ..st.voice() };
        fx.pulse.set(animations, v, v);
        fx.glow_level.animate(glow_target(selected && st.attention), now, animations);
        ignite(&mut fx.ignition, &mut fx.was_running, &mut fx.prompt, &st, now, animations);
        let unread_color = t.accent(200);
        let feedback = feedback_color(&st, t);
        let glow_color = tint(background, feedback.unwrap_or(unread_color), fx.glow_level.value(now));
        let base = t.text.base;
        let f = Rgbf::of;
        let c = PulseColors { color: f(tint(background, base, 0.45)), glow: f(glow_color), flash: f(tint(background, base, 0.65)), completion: f(unread_color) };
        let w = width as usize;
        fx.pulse.render(buf, x, y, w, f(background), &c, &c, GLOW_TAIL, GLOW_TAIL, None);
        let sweep = fx.pulse.level(w);
        // `numberColor` of the strip (the selection and activity springs at rest).
        let idle_number = tint(t.text.formfield.base, t.background.raised.base, 0.55);
        let number_base = tint(idle_number, tint(base, background, 0.25), if selected { 1.0 } else { 0.0 });
        let activity = if st.complete { 1.0 } else { 0.0 };
        let color = if st.runs { t.interactive(200) } else { feedback.unwrap_or_else(|| tint(number_base, unread_color, activity)) };
        StripLook { number: tint(color, base, fx.ignition.value(now).max(0.15 * sweep)) }
    }

    /// The strip's glow color for title characters (`glowColor()`), when the tab glows.
    pub fn strip_glow_color(&self, t: &Theme, id: &str, selected: bool, background: Rgba) -> Option<Rgba> {
        let active = self.session_id().map(|s| self.store.root(s));
        let st = self.fx_status(id, active.as_deref());
        let glows = st.attention || (!selected && !st.busy && st.unread.is_some());
        if !glows {
            return None;
        }
        let level = self.strip_fx.get(id).map(|f| f.glow_level.value(self.fx_now())).unwrap_or(1.0);
        Some(tint(background, feedback_color(&st, t).unwrap_or(t.accent(200)), level))
    }
}

/// `glowTextColor(base, glow, index, width, level)`: a title character over the glow tinges toward it.
pub fn glow_text_color(base: Rgba, glow: Rgba, index: usize, width: usize, level: f64) -> Rgba {
    tint(base, glow, 0.12 * crate::ui::tab_pulse::unread_glow_intensity(index as f64, width as f64, GLOW_TAIL) * level)
}
