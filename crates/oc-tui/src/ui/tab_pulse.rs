//! `component/tab-pulse.tsx` (2.2.5): the session-tab activity pulse. A one-row strip of background cells (or, with
//! an edge, half-block separator cells) that a running session sweeps across, that flashes when a run starts, stops
//! or a prompt arrives, glows while the tab is unread or needs attention, and pulses once when a run completes.
//! Since #60338 every pulse spans the full tab (rail) width.
//!
//! The state machines and the per-cell color math are a direct port; colors are blended in 0..1 floats like
//! OpenTUI's `RGBA` and rounded to 8 bits when drawn.

use crate::theme::Rgba;
use ratatui::buffer::Buffer;
use ratatui::style::Color;

pub fn clamp(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

pub fn smootherstep(value: f64) -> f64 {
    value * value * value * (value * (value * 6.0 - 15.0) + 10.0)
}

const RUN_DURATION: f64 = 2_800.0;
const RUN_ATTACK: f64 = 450.0;
pub const RUN_HEAD: f64 = 4.0;
pub const RUN_TAIL: f64 = 18.0;
const RUN_FADE_OUT: f64 = 500.0;
const COMPLETION_DURATION: f64 = 1_200.0;
const COMPLETION_ATTACK: f64 = 0.12;
const COMPLETION_OPACITY: f64 = 0.18;
const EDGE_FLASH_DURATION: f64 = 800.0;
const EDGE_FLASH_ATTACK: f64 = 0.1;
const EDGE_FLASH_OPACITY: f64 = 0.1;
const PROMPT_FLASH_SCALE: f64 = 2.0;
const GLOW_IGNITION_DURATION: f64 = 600.0;
const GLOW_IGNITION_PEAK: f64 = 1.5;
const GLOW_IGNITION_ATTACK: f64 = 0.3;
const GLOW_FADE_OUT: f64 = 200.0;
const GLOW_RELEASE_DURATION: f64 = 900.0;
const GLOW_RELEASE_ATTACK: f64 = 0.12;
const GLOW_RELEASE_PEAK: f64 = 1.25;
pub const GLOW_TAIL: f64 = 12.0;
const GLOW_OPACITY: f64 = 0.16;

pub fn intensity_at(index: f64, front: f64, head: f64, tail: f64) -> f64 {
    let distance = front - index;
    if distance < 0.0 { smootherstep(clamp(1.0 + distance / head)) } else { smootherstep(clamp(1.0 - distance / tail)) }
}

pub fn coast(value: f64) -> f64 {
    let ramp = 0.2;
    if value < ramp {
        return (value * value) / (2.0 * ramp * (1.0 - ramp));
    }
    if value > 1.0 - ramp {
        return 1.0 - ((1.0 - value) * (1.0 - value)) / (2.0 * ramp * (1.0 - ramp));
    }
    (value - ramp / 2.0) / (1.0 - ramp)
}

fn fade_out(progress: f64) -> f64 {
    1.0 - smootherstep(progress)
}

/// Rise to peak over the attack fraction, then settle to rest over the remainder.
fn attack_decay(progress: f64, attack: f64, peak: f64, rest: f64) -> f64 {
    if progress < attack {
        peak * smootherstep(clamp(progress / attack))
    } else {
        peak - (peak - rest) * smootherstep(clamp((progress - attack) / (1.0 - attack)))
    }
}

pub fn completion_pulse_opacity(progress: f64) -> f64 {
    attack_decay(progress, COMPLETION_ATTACK, 1.0, 0.0)
}

pub fn glow_ignition_level(progress: f64) -> f64 {
    attack_decay(progress, GLOW_IGNITION_ATTACK, GLOW_IGNITION_PEAK, 1.0)
}

fn glow_intensity_at(index: f64, tail: f64) -> f64 {
    smootherstep(clamp(1.0 - (index - 1.0).max(0.0) / tail))
}

pub fn tab_flash_intensity(index: f64, tail: f64) -> f64 {
    glow_intensity_at(index, tail)
}

pub fn unread_glow_intensity(index: f64, width: f64, maximum_tail: f64) -> f64 {
    let tail = maximum_tail.min((width - 2.0).max(1.0));
    glow_intensity_at(index, tail)
}

fn glow_release_spread(progress: f64, tail: f64, width: f64) -> f64 {
    tail + smootherstep(progress) * width
}

const GLOW_RELEASE_DRAIN_FRACTION: f64 = GLOW_FADE_OUT / GLOW_RELEASE_DURATION;

fn glow_release_drain(progress: f64) -> f64 {
    fade_out(clamp(progress / GLOW_RELEASE_DRAIN_FRACTION))
}

fn glow_release_swell(progress: f64) -> f64 {
    attack_decay(progress, GLOW_RELEASE_ATTACK, GLOW_RELEASE_PEAK, 0.0)
}

fn edge_flash_shape(progress: f64) -> f64 {
    attack_decay(progress, EDGE_FLASH_ATTACK, 1.0, 0.0)
}

/// A color as OpenTUI's float RGBA.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rgbf {
    pub r: f64,
    pub g: f64,
    pub b: f64,
}

impl Rgbf {
    pub fn of(c: Rgba) -> Rgbf {
        Rgbf { r: c.r as f64 / 255.0, g: c.g as f64 / 255.0, b: c.b as f64 / 255.0 }
    }

    /// The terminal color OpenTUI writes for these floats.
    pub fn color(self) -> Color {
        let q = |v: f64| (v * 255.0).round().clamp(0.0, 255.0) as u8;
        Color::Rgb(q(self.r), q(self.g), q(self.b))
    }
}

#[allow(clippy::too_many_arguments)]
pub fn blend_tab_pulse_color(background: Rgbf, glow_color: Rgbf, running_color: Rgbf, flash_color: Rgbf, completion_color: Rgbf, glow: f64, running: f64, flash: f64, completion: f64) -> Rgbf {
    let mut o = if glow == 0.0 {
        background
    } else {
        Rgbf {
            r: background.r + (glow_color.r - background.r) * glow,
            g: background.g + (glow_color.g - background.g) * glow,
            b: background.b + (glow_color.b - background.b) * glow,
        }
    };
    for (c, k) in [(running_color, running), (flash_color, flash), (completion_color, completion)] {
        if k != 0.0 {
            o.r += (c.r - o.r) * k;
            o.g += (c.g - o.g) * k;
            o.b += (c.b - o.b) * k;
        }
    }
    o
}

/// A one-shot animation clock: `level()` follows `shape` over `duration`, scaled by the value passed to start.
#[derive(Clone, Debug)]
struct Envelope {
    clock: Option<f64>,
    scale: f64,
    duration: f64,
    shape: fn(f64) -> f64,
}

impl Envelope {
    fn new(duration: f64, shape: fn(f64) -> f64) -> Envelope {
        Envelope { clock: None, scale: 1.0, duration, shape }
    }
    fn start(&mut self, scale: f64) {
        if self.clock.is_some() {
            return;
        }
        self.clock = Some(0.0);
        self.scale = scale;
    }
    fn restart(&mut self, scale: f64) {
        self.clock = Some(0.0);
        self.scale = scale;
    }
    fn stop(&mut self) {
        self.clock = None;
    }
    fn advance(&mut self, delta: f64) {
        let Some(c) = self.clock else { return };
        let c = c + delta;
        self.clock = if c >= self.duration { None } else { Some(c) };
    }
    fn active(&self) -> bool {
        self.clock.is_some()
    }
    fn level(&self) -> f64 {
        match self.clock {
            None => 0.0,
            Some(c) => self.scale * (self.shape)(c / self.duration),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    Idle,
    Attack,
    Sustain,
    Release,
}

/// A gated stage: trigger runs the attack and holds sustain at 1; release fades from the level at note-off.
#[derive(Clone, Debug)]
struct Gated {
    phase: Phase,
    clock: f64,
    release_scale: f64,
    attack: (f64, fn(f64) -> f64),
    release: (f64, fn(f64) -> f64),
}

impl Gated {
    fn new(attack: (f64, fn(f64) -> f64), release: (f64, fn(f64) -> f64), sustain: bool) -> Gated {
        Gated { phase: if sustain { Phase::Sustain } else { Phase::Idle }, clock: 0.0, release_scale: 1.0, attack, release }
    }
    fn trigger(&mut self) {
        self.phase = Phase::Attack;
        self.clock = 0.0;
    }
    fn release(&mut self) {
        if self.phase == Phase::Idle {
            return;
        }
        self.release_scale = self.level();
        self.phase = Phase::Release;
        self.clock = 0.0;
    }
    fn settle(&mut self, sustain: bool) {
        self.phase = if sustain { Phase::Sustain } else { Phase::Idle };
        self.clock = 0.0;
    }
    fn advance(&mut self, delta: f64) {
        if matches!(self.phase, Phase::Idle | Phase::Sustain) {
            return;
        }
        self.clock += delta;
        let duration = if self.phase == Phase::Attack { self.attack.0 } else { self.release.0 };
        if self.clock < duration {
            return;
        }
        self.phase = if self.phase == Phase::Attack { Phase::Sustain } else { Phase::Idle };
        self.clock = 0.0;
    }
    fn level(&self) -> f64 {
        match self.phase {
            Phase::Idle => 0.0,
            Phase::Sustain => 1.0,
            Phase::Attack => (self.attack.1)(self.clock / self.attack.0),
            Phase::Release => self.release_scale * (self.release.1)(self.clock / self.release.0),
        }
    }
    fn animating(&self) -> bool {
        matches!(self.phase, Phase::Attack | Phase::Release)
    }
    fn idle(&self) -> bool {
        self.phase == Phase::Idle
    }
    fn release_progress(&self) -> Option<f64> {
        (self.phase == Phase::Release).then(|| self.clock / self.release.0)
    }
}

/// The inputs of one pulse voice.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Voice {
    pub active: bool,
    pub prompt_pulse: u64,
    pub complete: bool,
    pub glow: bool,
}

#[derive(Clone, Debug)]
struct PulseState {
    enabled: bool,
    v: Voice,
    completion_pending: bool,
    sweep_clock: f64,
    run: Gated,
    glow: Gated,
    completion: Envelope,
    flash: Envelope,
}

impl PulseState {
    fn new(enabled: bool, v: Voice) -> PulseState {
        let mut s = PulseState {
            enabled,
            v,
            completion_pending: false,
            sweep_clock: 0.0,
            run: Gated::new((RUN_ATTACK, smootherstep), (RUN_FADE_OUT, fade_out), false),
            // A glow that exists at mount holds sustain without an ignition flash, even when animations are off.
            glow: Gated::new((GLOW_IGNITION_DURATION, glow_ignition_level), (GLOW_RELEASE_DURATION, glow_release_drain), v.glow),
            completion: Envelope::new(COMPLETION_DURATION, completion_pulse_opacity),
            flash: Envelope::new(EDGE_FLASH_DURATION, edge_flash_shape),
        };
        if s.enabled && s.v.active {
            s.run.trigger();
        }
        s
    }

    fn live(&self) -> bool {
        self.enabled && (self.v.active || self.run.animating() || self.glow.animating() || self.completion.active() || self.flash.active())
    }
    fn running(&self) -> f64 {
        self.run.level()
    }
    fn completion(&self) -> f64 {
        self.completion.level() * COMPLETION_OPACITY
    }
    fn flash(&self) -> f64 {
        self.flash.level() * EDGE_FLASH_OPACITY
    }
    fn glow_level(&self) -> f64 {
        self.glow.level()
    }
    fn glow_release_progress(&self) -> Option<f64> {
        self.glow.release_progress()
    }
    fn glow_release_swell(&self) -> f64 {
        let Some(progress) = self.glow.release_progress() else { return 0.0 };
        glow_release_swell(progress) * self.glow.release_scale.max(1.0)
    }

    fn set_enabled(&mut self, value: bool) {
        if value == self.enabled {
            return;
        }
        self.enabled = value;
        if !value {
            self.run.settle(false);
            self.glow.settle(self.v.glow);
            self.completion.stop();
            self.flash.stop();
            self.completion_pending = false;
        } else if self.v.active {
            self.run.trigger();
        }
    }

    fn set_active(&mut self, value: bool) {
        if value == self.v.active {
            return;
        }
        self.v.active = value;
        if !self.enabled {
            return;
        }
        if value {
            self.sweep_clock = 0.0;
            self.run.trigger();
            self.completion.stop();
            self.completion_pending = false;
        } else {
            self.run.release();
            self.completion_pending = true;
        }
        self.flash.start(1.0);
    }

    fn set_prompt_pulse(&mut self, value: u64) {
        if value == self.v.prompt_pulse {
            return;
        }
        self.v.prompt_pulse = value;
        if self.enabled {
            self.flash.restart(PROMPT_FLASH_SCALE);
        }
    }

    fn set_complete(&mut self, value: bool) {
        if value == self.v.complete {
            return;
        }
        self.v.complete = value;
        if !value {
            self.completion.stop();
            self.completion_pending = false;
        }
        if value && self.completion_pending {
            self.completion_pending = false;
            if self.enabled {
                self.completion.start(1.0);
            }
        }
    }

    fn set_glow(&mut self, value: bool) {
        if value == self.v.glow {
            return;
        }
        self.v.glow = value;
        if !self.enabled {
            self.glow.settle(value);
            return;
        }
        if value {
            self.glow.trigger();
        } else {
            self.glow.release();
        }
    }

    /// The setters in the order the renderable's props apply them.
    fn set(&mut self, v: Voice) {
        self.set_active(v.active);
        self.set_prompt_pulse(v.prompt_pulse);
        self.set_complete(v.complete);
        self.set_glow(v.glow);
    }

    fn advance(&mut self, delta: f64) {
        if !self.live() {
            return;
        }
        // The sweep keeps coasting from note-on through the release fade.
        if !self.run.idle() {
            self.sweep_clock += delta;
        }
        self.run.advance(delta);
        self.glow.advance(delta);
        self.completion.advance(delta);
        self.flash.advance(delta);
        if !self.completion_pending {
            return;
        }
        if self.v.complete {
            self.completion_pending = false;
            self.completion.start(1.0);
            return;
        }
        if self.run.release_progress().is_none() {
            self.completion_pending = false;
        }
    }

    fn fronts(&self, width: f64) -> [f64; 2] {
        let cycles = self.sweep_clock / RUN_DURATION;
        let progress = cycles % 1.0;
        let start = -RUN_HEAD;
        let end = width - 1.0 + RUN_TAIL;
        let second = if cycles < 0.5 { 0.0 } else { (cycles + 0.5) % 1.0 };
        [start + coast(progress) * (end - start), start + coast(second) * (end - start)]
    }
}

/// Which half-block separator a pulse draws, if any (`edge`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    /// `▄`: the lower half is this tab's top, the upper half the previous tab's bottom.
    Above,
    /// `▀`: below the last tab.
    Below,
}

/// The colors a pulse blends (`color`, `glowColor`, `flashColor`, `completionColor` and the `outer*` ones).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PulseColors {
    pub color: Rgbf,
    pub glow: Rgbf,
    pub flash: Rgbf,
    pub completion: Rgbf,
}

/// `TabPulseRenderable`.
#[derive(Clone, Debug)]
pub struct TabPulse {
    enabled: bool,
    edge: Option<Edge>,
    inner: PulseState,
    outer: PulseState,
    /// The last level reported to `onLevel` (quantized to 1/32).
    last_level: f64,
}

impl TabPulse {
    pub fn new(edge: Option<Edge>, enabled: bool, inner: Voice, outer: Voice) -> TabPulse {
        TabPulse { enabled, edge, inner: PulseState::new(enabled, inner), outer: PulseState::new(enabled && edge.is_some(), outer), last_level: 0.0 }
    }

    /// Apply the current props (`enabled`, then the inner and outer voices).
    pub fn set(&mut self, enabled: bool, inner: Voice, outer: Voice) {
        if enabled != self.enabled {
            self.enabled = enabled;
            self.inner.set_enabled(enabled);
            self.outer.set_enabled(enabled && self.edge.is_some());
        }
        self.inner.set(inner);
        self.outer.set(outer);
    }

    /// Whether the pulse animates (`animationFps` is set).
    pub fn live(&self) -> bool {
        self.inner.live() || self.outer.live()
    }

    /// `onAnimationTick(deltaTime)` (milliseconds).
    pub fn advance(&mut self, delta: f64) {
        self.inner.advance(delta);
        self.outer.advance(delta);
    }

    /// `onLevel`: the running sweep's intensity at the tab number's cell (index 1), quantized to 1/32.
    pub fn level(&mut self, width: usize) -> f64 {
        let running = self.inner.running();
        let level = if running == 0.0 || width == 0 {
            0.0
        } else {
            let f = self.inner.fronts(width as f64);
            intensity_at(1.0, f[0], RUN_HEAD, RUN_TAIL).max(intensity_at(1.0, f[1], RUN_HEAD, RUN_TAIL)) * running
        };
        let quantized = (level * 32.0).round() / 32.0;
        self.last_level = quantized;
        quantized
    }

    /// `renderSelf`: paint `width` cells at (`x`, `y`). Nothing is drawn while every stage is idle (the cells keep
    /// what is underneath). Without an edge a cell gets the blended background; with one, the half block with the
    /// inner color as its foreground and the outer one as its background.
    #[allow(clippy::too_many_arguments)]
    pub fn render(&self, buf: &mut Buffer, x: u16, y: u16, width: usize, background: Rgbf, inner: &PulseColors, outer: &PulseColors, glow_tail: f64, outer_glow_tail: f64, flash_tail: Option<f64>) {
        if width == 0 {
            return;
        }
        let (i, o) = (&self.inner, &self.outer);
        let (running, completion, flash, glow_level, release) = (i.running(), i.completion(), i.flash(), i.glow_level(), i.glow_release_progress());
        let (o_running, o_completion, o_flash, o_glow_level, o_release) = (o.running(), o.completion(), o.flash(), o.glow_level(), o.glow_release_progress());
        if glow_level == 0.0 && release.is_none() && running == 0.0 && completion == 0.0 && flash == 0.0 && o_glow_level == 0.0 && o_release.is_none() && o_running == 0.0 && o_completion == 0.0 && o_flash == 0.0 {
            return;
        }
        let w = width as f64;
        let fronts = (running != 0.0).then(|| i.fronts(w));
        let o_fronts = (o_running != 0.0).then(|| o.fronts(w));
        let glow_tail = glow_tail.min((w - 2.0).max(1.0));
        let o_glow_tail = outer_glow_tail.min((w - 2.0).max(1.0));
        let swell = i.glow_release_swell();
        let spread = if swell == 0.0 { 0.0 } else { glow_release_spread(release.unwrap_or(0.0), glow_tail, w) };
        let o_swell = o.glow_release_swell();
        let o_spread = if o_swell == 0.0 { 0.0 } else { glow_release_spread(o_release.unwrap_or(0.0), o_glow_tail, w) };
        let flash_tail = flash_tail.map(|t| t.min((w - 2.0).max(1.0)));
        let area = buf.area;
        for index in 0..width {
            let cx = x as usize + index;
            if cx >= area.right() as usize || y >= area.bottom() || y < area.top() || cx < area.left() as usize {
                continue;
            }
            let k = index as f64;
            let sweep = |f: Option<[f64; 2]>, r: f64| match f {
                None => 0.0,
                Some(f) => intensity_at(k, f[0], RUN_HEAD, RUN_TAIL).max(intensity_at(k, f[1], RUN_HEAD, RUN_TAIL)) * 0.14 * r,
            };
            let glow = (if glow_level == 0.0 { 0.0 } else { glow_intensity_at(k, glow_tail) * GLOW_OPACITY * glow_level })
                .max(if swell == 0.0 { 0.0 } else { glow_intensity_at(k, spread) * GLOW_OPACITY * swell });
            let fl = match flash_tail {
                None => flash,
                Some(t) => flash * tab_flash_intensity(k, t),
            };
            let c = blend_tab_pulse_color(background, inner.glow, inner.color, inner.flash, inner.completion, glow, sweep(fronts, running), fl, completion);
            let cell = &mut buf[(cx as u16, y)];
            let Some(edge) = self.edge else {
                cell.set_symbol(" ");
                cell.set_fg(Color::Reset);
                cell.set_bg(c.color());
                continue;
            };
            let o_glow = (if o_glow_level == 0.0 { 0.0 } else { glow_intensity_at(k, o_glow_tail) * GLOW_OPACITY * o_glow_level })
                .max(if o_swell == 0.0 { 0.0 } else { glow_intensity_at(k, o_spread) * GLOW_OPACITY * o_swell });
            let o_fl = match flash_tail {
                None => o_flash,
                Some(t) => o_flash * tab_flash_intensity(k, t),
            };
            let oc = blend_tab_pulse_color(background, outer.glow, outer.color, outer.flash, outer.completion, o_glow, sweep(o_fronts, o_running), o_fl, o_completion);
            cell.set_symbol(if edge == Edge::Above { "▄" } else { "▀" });
            cell.set_fg(c.color());
            cell.set_bg(oc.color());
        }
    }
}

/// `tween` (`ui/animation.ts`): a value easing (smoothstep) toward its target over `duration` seconds; with
/// animations off it jumps.
#[derive(Clone, Copy, Debug)]
pub struct Tween {
    from: f64,
    target: f64,
    started: f64,
    duration: f64,
}

impl Tween {
    pub fn new(value: f64, duration: f64) -> Tween {
        Tween { from: value, target: value, started: 0.0, duration }
    }

    pub fn value(&self, now: f64) -> f64 {
        let progress = ((now - self.started) / 1000.0 / self.duration.max(0.001)).clamp(0.0, 1.0);
        let eased = progress * progress * (3.0 - 2.0 * progress);
        self.from + (self.target - self.from) * eased
    }

    pub fn moving(&self, now: f64) -> bool {
        self.value(now) != self.target
    }

    /// `animate(next)` at `now` (milliseconds); `enabled` false jumps.
    pub fn animate(&mut self, next: f64, now: f64, enabled: bool) {
        if next == self.target {
            return;
        }
        if !enabled {
            return self.jump(next);
        }
        self.from = self.value(now);
        self.target = next;
        self.started = now;
    }

    pub fn jump(&mut self, next: f64) {
        self.from = next;
        self.target = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // tab-pulse.test.tsx: the shared helpers.
    #[test]
    fn shapes_match_upstream() {
        assert_eq!(smootherstep(0.0), 0.0);
        assert_eq!(smootherstep(1.0), 1.0);
        assert!((coast(0.5) - 0.5).abs() < 1e-12);
        assert_eq!(completion_pulse_opacity(0.0), 0.0);
        assert!((completion_pulse_opacity(COMPLETION_ATTACK) - 1.0).abs() < 1e-12);
        assert!((glow_ignition_level(GLOW_IGNITION_ATTACK) - GLOW_IGNITION_PEAK).abs() < 1e-12);
        assert!((glow_ignition_level(1.0) - 1.0).abs() < 1e-12);
        // The sweep peaks at its front and fades over the head ahead and the tail behind.
        assert_eq!(intensity_at(10.0, 10.0, RUN_HEAD, RUN_TAIL), 1.0);
        assert_eq!(intensity_at(14.0, 10.0, RUN_HEAD, RUN_TAIL), 0.0);
        assert_eq!(intensity_at(-8.0, 10.0, RUN_HEAD, RUN_TAIL), 0.0);
        // The unread glow falls off from the second cell over at most 12 cells (fewer on a narrow tab).
        assert_eq!(unread_glow_intensity(1.0, 40.0, GLOW_TAIL), 1.0);
        assert_eq!(unread_glow_intensity(13.0, 40.0, GLOW_TAIL), 0.0);
        assert_eq!(unread_glow_intensity(4.0, 5.0, GLOW_TAIL), 0.0);
    }

    #[test]
    fn a_run_sweeps_flashes_and_completes() {
        let bg = Rgbf::of(Rgba::rgb(20, 20, 20));
        let white = Rgbf::of(Rgba::rgb(255, 255, 255));
        let colors = PulseColors { color: white, glow: white, flash: white, completion: white };
        let mut p = TabPulse::new(None, true, Voice::default(), Voice::default());
        assert!(!p.live());
        let mut buf = Buffer::empty(ratatui::layout::Rect::new(0, 0, 30, 1));
        p.render(&mut buf, 0, 0, 30, bg, &colors, &colors, GLOW_TAIL, GLOW_TAIL, None);
        assert_eq!(buf[(0, 0)].bg, Color::Reset, "idle pulses draw nothing");
        p.set(true, Voice { active: true, ..Default::default() }, Voice::default());
        assert!(p.live());
        // The run's attack takes 450 ms; the start flash lasts 800 ms.
        p.advance(450.0);
        assert_eq!(p.inner.running(), 1.0);
        assert!(p.inner.flash() > 0.0);
        p.advance(400.0);
        assert_eq!(p.inner.flash(), 0.0);
        p.render(&mut buf, 0, 0, 30, bg, &colors, &colors, GLOW_TAIL, GLOW_TAIL, None);
        let lit: Vec<u16> = (0..30).filter(|x| buf[(*x, 0)].bg != bg.color()).collect();
        assert!(!lit.is_empty() && lit.len() < 30, "{lit:?}");
        // Stopping fades the run out over 500 ms; the completion pulse waits for `complete`.
        p.set(true, Voice::default(), Voice::default());
        p.set(true, Voice { complete: true, ..Default::default() }, Voice::default());
        assert!(p.inner.completion.active());
        p.advance(1_200.0);
        assert!(!p.live());
        assert_eq!(p.level(30), 0.0);
    }

    #[test]
    fn edges_draw_half_blocks_and_glows_hold_without_animations() {
        let bg = Rgbf::of(Rgba::rgb(20, 20, 20));
        let hue = Rgbf::of(Rgba::rgb(250, 178, 131));
        let colors = PulseColors { color: hue, glow: hue, flash: hue, completion: hue };
        let mut p = TabPulse::new(Some(Edge::Above), false, Voice { glow: true, ..Default::default() }, Voice::default());
        let mut buf = Buffer::empty(ratatui::layout::Rect::new(0, 0, 20, 1));
        p.render(&mut buf, 0, 0, 20, bg, &colors, &colors, 8.0, 5.0, Some(8.0));
        assert_eq!(buf[(0, 0)].symbol(), "▄");
        assert_eq!(buf[(0, 0)].bg, bg.color());
        assert_ne!(buf[(1, 0)].fg, bg.color());
        // Past the tail the glow is gone.
        assert_eq!(buf[(15, 0)].fg, bg.color());
        p.set(false, Voice::default(), Voice::default());
        assert!(!p.live());
        let mut t = Tween::new(0.0, 0.7);
        t.jump(0.85);
        t.animate(0.0, 1000.0, true);
        assert_eq!(t.value(1000.0), 0.85);
        assert!((t.value(1350.0) - 0.425).abs() < 1e-9);
        assert_eq!(t.value(1700.0), 0.0);
    }
}
