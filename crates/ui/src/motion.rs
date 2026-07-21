//! Animation kit — the comet motion catalog as reusable helpers over gpui
//! [`Animation`]/[`AnimationExt`].
//!
//! Catalog (docs/research/feature-inventory.md §1.12):
//! - `fade-in`   0.5s  cubic-bezier(0.16,1,0.3,1), translateY 4→0 (entrances)
//! - `fade-quick` 0.15s
//! - `menu-in`   0.14s scale 0.96 + translateY −2 (popovers)
//! - `dialog-in` 0.18s scale 0.96→1
//! - `splash-out` 0.5s opacity + translateY −6, 0.15s delay
//! - `comet-pulse` 2.4s staggered cell opacity 0.08→1, scale 0.9→1 (loaders)
//! - `gradient-spin-pulse` 750ms per-cell phase wave (working indicator)
//! - 200ms ease-out width/height transitions (sidebar/panes)
//!
//! Custom easing is a closure over gpui's `Fn(f32) -> f32` easing shape; CSS
//! `cubic-bezier()` is evaluated exactly by [`CubicBezier`].
//!
//! Reduced motion: gpui's `App::reduce_motion` flag is honored *automatically* by
//! every `with_animation` element — oneshot animations snap to their end state,
//! repeating ones to their start state, and no frames are scheduled. The
//! [`set_reduced_motion`]/[`reduced_motion`] wrappers make it a single global
//! switch; pure helpers take the flag explicitly where they run outside elements.
//!
//! translateY is implemented as a relative-position `top` inset: taffy applies
//! relative insets after layout, so — like a CSS transform — siblings never move.
//! gpui has no scale transform for `div`s at the pinned rev (only `svg`
//! transformations), so `menu-in`/`dialog-in` approximate their scale component
//! with fade + translate; see the module report in ARCHITECTURE §4 follow-ups.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use gpui::{
    Animation, AnimationElement, App, ElementId, Hsla, IntoElement, Rgba, SharedString, Styled,
    Window, px,
};

pub use gpui::AnimationExt;

// ---------------------------------------------------------------------------
// Cubic bezier
// ---------------------------------------------------------------------------

/// A CSS `cubic-bezier(x1, y1, x2, y2)` timing function (endpoints fixed at
/// (0,0) and (1,1)). Evaluation solves x(t) = input by Newton iteration with a
/// bisection fallback — the standard UnitBezier approach.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CubicBezier {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl CubicBezier {
    pub const fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Self {
        Self { x1, y1, x2, y2 }
    }

    fn coefficients(a: f32, b: f32) -> (f32, f32, f32) {
        let c = 3.0 * a;
        let bb = 3.0 * (b - a) - c;
        let aa = 1.0 - c - bb;
        (aa, bb, c)
    }

    fn sample_x(&self, t: f32) -> f32 {
        let (a, b, c) = Self::coefficients(self.x1, self.x2);
        ((a * t + b) * t + c) * t
    }

    fn sample_y(&self, t: f32) -> f32 {
        let (a, b, c) = Self::coefficients(self.y1, self.y2);
        ((a * t + b) * t + c) * t
    }

    fn sample_x_derivative(&self, t: f32) -> f32 {
        let (a, b, c) = Self::coefficients(self.x1, self.x2);
        (3.0 * a * t + 2.0 * b) * t + c
    }

    /// Curve parameter `t` for a given progress `x` (both 0..1).
    fn solve_t_for_x(&self, x: f32) -> f32 {
        // Newton–Raphson.
        let mut t = x;
        for _ in 0..8 {
            let err = self.sample_x(t) - x;
            if err.abs() < 1e-6 {
                return t;
            }
            let d = self.sample_x_derivative(t);
            if d.abs() < 1e-6 {
                break;
            }
            t -= err / d;
        }
        // Bisection fallback (x(t) is monotonic for valid CSS beziers).
        let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
        for _ in 0..32 {
            let mid = (lo + hi) / 2.0;
            if self.sample_x(mid) < x {
                lo = mid
            } else {
                hi = mid
            }
        }
        (lo + hi) / 2.0
    }

    /// Eased output for input progress `x ∈ [0,1]` (clamped).
    pub fn eval(&self, x: f32) -> f32 {
        if x <= 0.0 {
            return 0.0;
        }
        if x >= 1.0 {
            return 1.0;
        }
        // f32 rounding can push sample_y a hair past 1.0 (observed 1.000000119
        // near the end of menu animations); gpui's animation element asserts
        // `delta ∈ [0,1]` and aborts, so clamp the output hard.
        self.sample_y(self.solve_t_for_x(x)).clamp(0.0, 1.0)
    }

    /// This curve as a gpui easing closure.
    pub fn easing(self) -> impl Fn(f32) -> f32 + 'static {
        move |x| self.eval(x)
    }
}

/// comet's signature entrance curve — CSS `cubic-bezier(0.16, 1, 0.3, 1)`.
pub const EASE_OUT_EXPO: CubicBezier = CubicBezier::new(0.16, 1.0, 0.3, 1.0);
/// CSS `ease-out` — width/height transitions.
pub const EASE_OUT: CubicBezier = CubicBezier::new(0.0, 0.0, 0.58, 1.0);
/// CSS `ease` — quick fades, menu/dialog pops.
pub const EASE: CubicBezier = CubicBezier::new(0.25, 0.1, 0.25, 1.0);
/// Sidebar resort glide — CSS `cubic-bezier(0.22, 1, 0.36, 1)` (used from M3b).
pub const EASE_RESORT: CubicBezier = CubicBezier::new(0.22, 1.0, 0.36, 1.0);
/// CSS `ease-in-out` — the transcript scroll glide (browser smooth-scroll
/// shape: gentle start, cruise, gentle landing).
pub const EASE_IN_OUT: CubicBezier = CubicBezier::new(0.42, 0.0, 0.58, 1.0);

// ---------------------------------------------------------------------------
// Motion specs (the catalog)
// ---------------------------------------------------------------------------

/// One catalog entry: duration + optional delay + curve. The delay is folded into
/// the gpui animation timeline (gpui `Animation` has no native delay): the
/// animation runs for `delay + duration` and [`progress`](Self::progress) holds 0
/// until the delay has elapsed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MotionSpec {
    pub duration_ms: u64,
    pub delay_ms: u64,
    pub curve: CubicBezier,
}

impl MotionSpec {
    pub const fn new(duration_ms: u64, curve: CubicBezier) -> Self {
        Self {
            duration_ms,
            delay_ms: 0,
            curve,
        }
    }

    pub const fn with_delay(mut self, delay_ms: u64) -> Self {
        self.delay_ms = delay_ms;
        self
    }

    /// Wall-clock span of the whole timeline (delay + duration).
    pub fn total(&self) -> Duration {
        Duration::from_millis(self.delay_ms + self.duration_ms)
    }

    /// Eased progress (0..1) for a raw timeline delta (0..1 across [`total`](Self::total)).
    /// Pure — unit-testable without a window.
    pub fn progress(&self, raw_delta: f32) -> f32 {
        let total = (self.delay_ms + self.duration_ms) as f32;
        if total <= 0.0 || self.duration_ms == 0 {
            return 1.0;
        }
        let t =
            (raw_delta.clamp(0.0, 1.0) * total - self.delay_ms as f32) / self.duration_ms as f32;
        self.curve.eval(t.clamp(0.0, 1.0))
    }

    /// A oneshot gpui [`Animation`] for this spec (delay folded in).
    /// Wall-clock span honors [`speed_scale`] (measurement knob).
    pub fn animation(&self) -> Animation {
        let spec = *self;
        Animation::new(spec.total().mul_f32(speed_scale())).with_easing(move |d| spec.progress(d))
    }

    /// A repeating gpui [`Animation`] with linear easing over the raw period —
    /// for the pulse/wave loaders whose per-cell easing happens in the animator.
    pub fn repeating(&self) -> Animation {
        Animation::new(self.total()).repeat()
    }
}

/// Entrances: 0.5s expo-out fade + 4px rise.
pub const FADE_IN: MotionSpec = MotionSpec::new(500, EASE_OUT_EXPO);
/// Quick fade: 0.15s.
pub const FADE_QUICK: MotionSpec = MotionSpec::new(150, EASE);
/// Popover-in: 0.14s (scale 0.96 approximated, translateY −2).
pub const MENU_IN: MotionSpec = MotionSpec::new(140, EASE);
/// Dialog-in: 0.18s (scale 0.96→1 approximated).
pub const DIALOG_IN: MotionSpec = MotionSpec::new(180, EASE);
/// Boot splash exit: 0.5s fade + 6px lift after a 0.15s hold.
pub const SPLASH_OUT: MotionSpec = MotionSpec::new(500, EASE).with_delay(150);
/// Sidebar / pane width+height transitions: 200ms ease-out.
pub const RESIZE: MotionSpec = MotionSpec::new(200, EASE_OUT);
/// Terminal tab drag-reorder sliding transforms: 150ms (§1.10).
pub const TAB_SLIDE: MotionSpec = MotionSpec::new(150, EASE_OUT);
/// Diff-pane per-file collapse: 180ms height (§1.11).
pub const COLLAPSE: MotionSpec = MotionSpec::new(180, EASE_OUT);
/// Diff-pane chevron rotate: 200ms (§1.11; approximated as a crossfade — gpui
/// divs have no rotation transform at the pinned rev, same caveat as scale).
pub const CHEVRON: MotionSpec = MotionSpec::new(200, EASE);
/// Rail-tick / scroll-to-row glide: 500ms ease-in-out over the whole distance
/// (Electron parity — the original rail rode the browser's native smooth
/// scroll, a fixed-duration gentle ease, never percent-of-remaining).
pub const SCROLL_GLIDE: MotionSpec = MotionSpec::new(500, EASE_IN_OUT);
/// Tailwind's default transition curve — CSS `cubic-bezier(0.4, 0, 0.2, 1)`
/// (`transition-colors` et al. carry it unless overridden; comet never does).
pub const EASE_TAILWIND: CubicBezier = CubicBezier::new(0.4, 0.0, 0.2, 1.0);
/// CSS `transition-colors` default: 150ms over [`EASE_TAILWIND`] — the temporal
/// blend every interactive hover wash rides in the original.
pub const HOVER_FADE: MotionSpec = MotionSpec::new(150, EASE_TAILWIND);
/// Comet loader pulse period: 2.4s.
pub const COMET_PULSE: MotionSpec = MotionSpec::new(2400, EASE);
/// Gradient matrix spinner wave period: 750ms.
pub const GRADIENT_SPIN: MotionSpec = MotionSpec::new(750, EASE);

// ---------------------------------------------------------------------------
// Element helpers (paint-layer entrances/exits)
// ---------------------------------------------------------------------------

/// Standard entrance: opacity 0→1 + translateY 4→0 over [`FADE_IN`].
pub fn fade_in<E>(id: impl Into<ElementId>, element: E) -> AnimationElement<E>
where
    E: Styled + IntoElement + 'static,
{
    element.with_animation(id, FADE_IN.animation(), |el, t| {
        el.relative().opacity(t).top(px(4.0 * (1.0 - t)))
    })
}

/// Quick opacity-only fade over [`FADE_QUICK`].
pub fn fade_quick<E>(id: impl Into<ElementId>, element: E) -> AnimationElement<E>
where
    E: Styled + IntoElement + 'static,
{
    element.with_animation(id, FADE_QUICK.animation(), |el, t| el.opacity(t))
}

/// Popover entrance: fade + translateY −2→0 over [`MENU_IN`].
/// (comet also scales 0.96→1; divs have no scale transform in gpui — approximated.)
pub fn menu_in<E>(id: impl Into<ElementId>, element: E) -> AnimationElement<E>
where
    E: Styled + IntoElement + 'static,
{
    element.with_animation(id, MENU_IN.animation(), |el, t| {
        el.relative()
            .opacity(0.3 + 0.7 * t)
            .top(px(-2.0 * (1.0 - t)))
    })
}

/// Dialog entrance over [`DIALOG_IN`] (scale approximated with fade + 2px rise).
pub fn dialog_in<E>(id: impl Into<ElementId>, element: E) -> AnimationElement<E>
where
    E: Styled + IntoElement + 'static,
{
    element.with_animation(id, DIALOG_IN.animation(), |el, t| {
        el.relative().opacity(t).top(px(2.0 * (1.0 - t)))
    })
}

/// Boot-splash exit: hold 150ms, then fade out + lift 6px over 500ms.
pub fn splash_out<E>(id: impl Into<ElementId>, element: E) -> AnimationElement<E>
where
    E: Styled + IntoElement + 'static,
{
    element.with_animation(id, SPLASH_OUT.animation(), |el, t| {
        el.opacity(1.0 - t).top(px(-6.0 * t))
    })
}

// ---------------------------------------------------------------------------
// Loader math (pure; rendered by crate::loaders)
// ---------------------------------------------------------------------------

/// Comet-pulse floor opacity.
pub const PULSE_MIN_OPACITY: f32 = 0.08;
/// Comet-pulse floor scale.
pub const PULSE_MIN_SCALE: f32 = 0.9;
/// Per-cell stagger of the comet pulse, as a fraction of the 2.4s period
/// (comet delays each cell by 0.15s).
pub const PULSE_STAGGER: f32 = 0.15 / 2.4;

/// Shift a repeating raw delta by a per-cell stagger, wrapping into [0,1).
pub fn staggered_phase(raw_delta: f32, index: usize, stagger: f32) -> f32 {
    (raw_delta - index as f32 * stagger).rem_euclid(1.0)
}

/// Cosine pulse: 0 at phase 0, 1 at phase 0.5, back to 0 at phase 1.
pub fn pulse_wave(phase: f32) -> f32 {
    0.5 - 0.5 * (phase * std::f32::consts::TAU).cos()
}

/// Comet loader cell opacity for a phase: 0.08 → 1 → 0.08.
pub fn pulse_opacity(phase: f32) -> f32 {
    PULSE_MIN_OPACITY + (1.0 - PULSE_MIN_OPACITY) * pulse_wave(phase)
}

/// Comet loader cell scale for a phase: 0.9 → 1 → 0.9.
pub fn pulse_scale(phase: f32) -> f32 {
    PULSE_MIN_SCALE + (1.0 - PULSE_MIN_SCALE) * pulse_wave(phase)
}

/// Gradient-matrix spinner wave: intensity (0..1) of cell `wave_index` out of
/// `wave_count` diagonals, at raw delta `raw_delta` of the 750ms period. The wave
/// front travels across diagonals once per period.
pub fn matrix_wave(raw_delta: f32, wave_index: usize, wave_count: usize) -> f32 {
    let count = wave_count.max(1) as f32;
    pulse_wave(staggered_phase(raw_delta, wave_index, 1.0 / count))
}

/// Gradient-spin cell opacity for a local phase `t` (0..1 of the period),
/// ported from comet's `gradient-spin-pulse` keyframes: full at the cycle
/// start, easing down to `dim` by 45%, resting at `dim` until 92%, then rising
/// back to full — the per-cell phase offset sweeps this pulse across the grid.
pub fn gspin_opacity(t: f32, dim: f32) -> f32 {
    let t = t.rem_euclid(1.0);
    if t < 0.45 {
        lerp(1.0, dim, t / 0.45)
    } else if t < 0.92 {
        dim
    } else {
        lerp(dim, 1.0, (t - 0.92) / 0.08)
    }
}

/// Linear interpolation (layout tweens).
pub fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

// ---------------------------------------------------------------------------
// Hover color fades (CSS `transition-colors` parity)
// ---------------------------------------------------------------------------
//
// gpui `.hover()` styles snap by construction — the style applies the frame
// the pointer enters. The original comet puts Tailwind `transition-colors`
// (150ms, cubic-bezier(0.4, 0, 0.2, 1)) on every interactive wash, so hover
// states FADE. This is the manual-drive tween for that (the shell `WidthTween`
// pattern — never `with_animation`, whose element-id-keyed clock replays on
// remount): a per-element-key hover progress, advanced from wall time on each
// evaluation, with the render tail requesting frames while any fade is
// mid-flight.
//
// The store is a main-thread `thread_local` rather than a gpui Global so the
// many free-function element builders (window-control buttons, popover menu
// rows, markdown code blocks) can blend colors without threading `cx` through
// every signature. All access happens on the UI thread (element builders,
// mouse listeners, the render tail).
//
// Staleness: an element that unmounts mid-hover never gets its leave event, so
// entries are stamped with a frame counter on every read and pruned by
// [`hover_fades_active`] (the once-per-frame tick) when a full frame passes
// without a read — a reopened menu never inherits a dead entry's wash.

/// One element's hover fade: progress runs `origin → target` over
/// [`HOVER_FADE`], re-anchored at `origin` whenever the pointer flips
/// direction mid-flight so the blend is continuous.
#[derive(Debug, Clone, Copy)]
struct FadeEntry {
    origin: f32,
    target: f32,
    started: Instant,
    /// Frame counter at the last read (liveness stamp — see module notes).
    seen: u64,
}

impl FadeEntry {
    fn value(&self, now: Instant, duration: Duration) -> f32 {
        let elapsed = now.saturating_duration_since(self.started);
        if duration.is_zero() || elapsed >= duration {
            return self.target;
        }
        let raw = elapsed.as_secs_f32() / duration.as_secs_f32();
        lerp(self.origin, self.target, HOVER_FADE.curve.eval(raw))
    }

    fn settled(&self, now: Instant, duration: Duration) -> bool {
        self.origin == self.target || now.saturating_duration_since(self.started) >= duration
    }
}

/// Per-key hover progress store. Pure core (explicit `now`) — unit-testable;
/// the thread-local wrappers below feed it wall time.
#[derive(Default)]
pub struct HoverFades {
    entries: HashMap<String, FadeEntry>,
    frame: u64,
}

impl HoverFades {
    fn duration() -> Duration {
        HOVER_FADE.total().mul_f32(speed_scale())
    }

    /// Pointer entered (`hovered`) or left the element behind `key`. Reduced
    /// motion snaps straight to the endpoint.
    pub fn set_at(&mut self, key: &str, hovered: bool, reduced: bool, now: Instant) {
        let target = if hovered { 1.0 } else { 0.0 };
        let duration = Self::duration();
        let current = self
            .entries
            .get(key)
            .map(|e| e.value(now, duration))
            .unwrap_or(0.0);
        if target == 0.0 && !self.entries.contains_key(key) {
            return; // never-hovered element reporting a leave — nothing to do
        }
        let origin = if reduced { target } else { current };
        let seen = self.frame;
        self.entries.insert(
            key.to_string(),
            FadeEntry {
                origin,
                target,
                started: now,
                seen,
            },
        );
    }

    /// Hover progress (0..1) for `key` at `now`; stamps liveness.
    pub fn value_at(&mut self, key: &str, now: Instant) -> f32 {
        let frame = self.frame;
        match self.entries.get_mut(key) {
            Some(entry) => {
                entry.seen = frame;
                entry.value(now, Self::duration())
            }
            None => 0.0,
        }
    }

    /// Once-per-frame bookkeeping: advance the frame counter, prune entries
    /// that settled back to rest or went a full frame unread (unmounted), and
    /// report whether any fade is still mid-flight (→ keep frames coming).
    pub fn tick_at(&mut self, now: Instant) -> bool {
        self.frame += 1;
        let frame = self.frame;
        let duration = Self::duration();
        let mut active = false;
        self.entries.retain(|_, entry| {
            // Unread through the whole previous frame: the element unmounted
            // (its leave event will never come) — drop the entry.
            if entry.seen + 1 < frame {
                return false;
            }
            let settled = entry.settled(now, duration);
            if !settled {
                active = true;
            }
            // Settled at rest — steady state, indistinguishable from absent.
            !(settled && entry.target == 0.0)
        });
        active
    }
}

thread_local! {
    static HOVER_FADES: RefCell<HoverFades> = RefCell::new(HoverFades::default());
}

/// Hover progress (0..1) for `key` this frame.
pub fn hover_t(key: &str) -> f32 {
    HOVER_FADES.with(|fades| fades.borrow_mut().value_at(key, Instant::now()))
}

/// Record a hover flip for `key` (reduced motion snaps).
pub fn set_hover(key: &str, hovered: bool, reduced: bool) {
    HOVER_FADES.with(|fades| {
        fades
            .borrow_mut()
            .set_at(key, hovered, reduced, Instant::now())
    });
}

/// An `.on_hover` listener driving the fade for `key` — pair with
/// [`hover_t`]/[`hover_blend`] reads of the same key in the same element.
pub fn hover_listener(
    key: impl Into<SharedString>,
) -> impl Fn(&bool, &mut Window, &mut App) + 'static {
    let key = key.into();
    move |hovered, window, cx| {
        set_hover(&key, *hovered, reduced_motion(cx));
        // Event-dispatch context: `request_animation_frame` is draw-phase-only
        // (it resolves the current view) — `refresh` marks the whole window
        // dirty, the root render re-evaluates the blend and keeps frames
        // coming via its tail while the fade is mid-flight.
        window.refresh();
    }
}

/// Frame-drive hook: call ONCE per window frame (the shell render tail); true
/// while any hover fade is mid-flight and frames must keep coming.
pub fn hover_fades_active() -> bool {
    HOVER_FADES.with(|fades| fades.borrow_mut().tick_at(Instant::now()))
}

/// Blend two colors by `t` the way the browser transitions them: component
/// interpolation in sRGB with premultiplied alpha — a wash fading in from
/// transparent brightens without passing through grey.
pub fn mix(from: Hsla, to: Hsla, t: f32) -> Hsla {
    let t = t.clamp(0.0, 1.0);
    if t <= 0.0 {
        return from;
    }
    if t >= 1.0 {
        return to;
    }
    let (f, g) = (Rgba::from(from), Rgba::from(to));
    let a = lerp(f.a, g.a, t);
    if a <= f32::EPSILON {
        // Both endpoints (effectively) transparent — carry the target's hue.
        return Hsla::from(Rgba { a: 0.0, ..g });
    }
    Hsla::from(Rgba {
        r: lerp(f.r * f.a, g.r * g.a, t) / a,
        g: lerp(f.g * f.a, g.g * g.a, t) / a,
        b: lerp(f.b * f.a, g.b * g.a, t) / a,
        a,
    })
}

/// The standard hover blend: rest → hover color at `key`'s current progress.
pub fn hover_blend(key: &str, rest: Hsla, hover: Hsla) -> Hsla {
    mix(rest, hover, hover_t(key))
}

// ---------------------------------------------------------------------------
// Reduced motion
// ---------------------------------------------------------------------------

/// Dev/measurement knob (`COMET_MOTION_SCALE`, default 1): stretches every
/// catalog timeline by this factor — e.g. `COMET_MOTION_SCALE=10` slows the
/// 200ms pane tweens to 2s so screenshot bursts can sample the geometry
/// per frame. Read once; never set in production.
pub fn speed_scale() -> f32 {
    static SCALE: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *SCALE.get_or_init(|| {
        std::env::var("COMET_MOTION_SCALE")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|s| s.is_finite())
            .map(|s| s.clamp(0.01, 100.0))
            .unwrap_or(1.0)
    })
}

/// Global reduced-motion flag. gpui snaps every `with_animation` element when
/// set (end state for oneshots, rest state for loops) and schedules no frames.
pub fn set_reduced_motion(cx: &mut App, reduced: bool) {
    cx.set_reduce_motion(reduced);
}

/// Read the global reduced-motion flag.
pub fn reduced_motion(cx: &App) -> bool {
    cx.reduce_motion()
}

#[cfg(test)]
mod tests {
    #[test]
    fn eval_never_escapes_unit_interval_dense_sweep() {
        // Regression: f32 rounding produced 1.000000119 near the tail of
        // EASE_OUT_EXPO, tripping gpui's `delta ∈ [0,1]` assert (SIGABRT on
        // the user's machine). Sweep densely, including the values right
        // below 1.0 where Newton lands closest to the endpoint.
        for curve in [EASE_OUT_EXPO, EASE_OUT, EASE, EASE_RESORT, EASE_IN_OUT] {
            for i in 0..=100_000u32 {
                let x = i as f32 / 100_000.0;
                let y = curve.eval(x);
                assert!((0.0..=1.0).contains(&y), "eval({x}) = {y} escaped [0,1]");
            }
            for x in [0.999_999f32, 0.999_999_9, 1.0 - f32::EPSILON] {
                let y = curve.eval(x);
                assert!((0.0..=1.0).contains(&y), "eval({x}) = {y} escaped [0,1]");
            }
        }
    }

    use super::*;

    fn assert_close(actual: f32, expected: f32, tol: f32, ctx: &str) {
        assert!(
            (actual - expected).abs() <= tol,
            "{ctx}: got {actual}, expected {expected} ±{tol}"
        );
    }

    #[test]
    fn bezier_linear_is_identity() {
        let linear = CubicBezier::new(0.0, 0.0, 1.0, 1.0);
        for x in [0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0] {
            assert_close(linear.eval(x), x, 1e-4, "linear");
        }
    }

    #[test]
    fn bezier_known_values() {
        // References computed independently with 80-step bisection.
        let cases: [(&str, CubicBezier, [f32; 5]); 3] = [
            (
                "expo",
                EASE_OUT_EXPO,
                [0.494391, 0.825622, 0.971779, 0.997677, 0.999878],
            ),
            (
                "ease-out",
                EASE_OUT,
                [0.160572, 0.378138, 0.684643, 0.906535, 0.982973],
            ),
            (
                "ease",
                EASE,
                [0.094796, 0.408511, 0.802403, 0.960459, 0.994316],
            ),
        ];
        for (name, curve, expected) in cases {
            for (x, want) in [0.1, 0.25, 0.5, 0.75, 0.9].into_iter().zip(expected) {
                assert_close(curve.eval(x), want, 1e-3, name);
            }
        }
    }

    #[test]
    fn bezier_endpoints_and_clamping() {
        for curve in [EASE_OUT_EXPO, EASE_OUT, EASE, EASE_RESORT, EASE_IN_OUT] {
            assert_eq!(curve.eval(0.0), 0.0);
            assert_eq!(curve.eval(1.0), 1.0);
            assert_eq!(curve.eval(-0.5), 0.0);
            assert_eq!(curve.eval(1.5), 1.0);
        }
    }

    #[test]
    fn bezier_is_monotonic_for_catalog_curves() {
        for curve in [EASE_OUT_EXPO, EASE_OUT, EASE, EASE_RESORT, EASE_IN_OUT] {
            let mut last = 0.0;
            for i in 0..=100 {
                let y = curve.eval(i as f32 / 100.0);
                assert!(y >= last - 1e-4, "monotonicity violated at {i}");
                last = y;
            }
        }
    }

    #[test]
    fn spec_delay_holds_then_runs() {
        // SPLASH_OUT: 150ms delay + 500ms run = 650ms total.
        assert_eq!(SPLASH_OUT.total(), Duration::from_millis(650));
        assert_eq!(SPLASH_OUT.progress(0.0), 0.0);
        // Still inside the delay window at raw 0.2 (130ms < 150ms).
        assert_eq!(SPLASH_OUT.progress(0.2), 0.0);
        // Fully done at the end; clamped beyond.
        assert_eq!(SPLASH_OUT.progress(1.0), 1.0);
        assert_eq!(SPLASH_OUT.progress(2.0), 1.0);
        // Midway through the run: raw 0.65 → 272.5ms into the 500ms run.
        let mid = SPLASH_OUT.progress(0.65);
        assert!(mid > 0.0 && mid < 1.0);
        // No-delay specs pass straight through the curve.
        assert_close(
            FADE_IN.progress(0.5),
            EASE_OUT_EXPO.eval(0.5),
            1e-6,
            "no-delay",
        );
    }

    #[test]
    fn catalog_timings_match_comet() {
        assert_eq!(FADE_IN.duration_ms, 500);
        assert_eq!(FADE_QUICK.duration_ms, 150);
        assert_eq!(MENU_IN.duration_ms, 140);
        assert_eq!(DIALOG_IN.duration_ms, 180);
        assert_eq!((SPLASH_OUT.duration_ms, SPLASH_OUT.delay_ms), (500, 150));
        assert_eq!(RESIZE.duration_ms, 200);
        assert_eq!(TAB_SLIDE.duration_ms, 150);
        assert_eq!(COLLAPSE.duration_ms, 180);
        assert_eq!(CHEVRON.duration_ms, 200);
        assert_eq!(COMET_PULSE.duration_ms, 2400);
        assert_eq!(GRADIENT_SPIN.duration_ms, 750);
        assert_eq!(EASE_OUT_EXPO, CubicBezier::new(0.16, 1.0, 0.3, 1.0));
    }

    #[test]
    fn pulse_wave_endpoints() {
        assert_close(pulse_wave(0.0), 0.0, 1e-6, "wave start");
        assert_close(pulse_wave(0.5), 1.0, 1e-6, "wave peak");
        assert_close(pulse_wave(1.0), 0.0, 1e-6, "wave end");
        assert_close(pulse_opacity(0.0), 0.08, 1e-6, "opacity floor");
        assert_close(pulse_opacity(0.5), 1.0, 1e-6, "opacity peak");
        assert_close(pulse_scale(0.0), 0.9, 1e-6, "scale floor");
        assert_close(pulse_scale(0.5), 1.0, 1e-6, "scale peak");
    }

    #[test]
    fn stagger_wraps_and_orders_cells() {
        // Cell 0 at delta 0 is at phase 0; later cells lag by the stagger.
        assert_close(staggered_phase(0.0, 0, PULSE_STAGGER), 0.0, 1e-6, "cell 0");
        assert_close(
            staggered_phase(0.0, 1, PULSE_STAGGER),
            1.0 - PULSE_STAGGER,
            1e-5,
            "cell 1 wraps",
        );
        // A full period later the phase is identical.
        assert_close(
            staggered_phase(0.3, 2, PULSE_STAGGER),
            staggered_phase(0.3 + 1.0, 2, PULSE_STAGGER),
            2e-6,
            "periodic",
        );
        // Matrix wave peaks travel: diagonal k peaks when the front reaches it.
        let peak0 = matrix_wave(0.5, 0, 5);
        assert_close(peak0, 1.0, 1e-5, "diag 0 peak at half period");
    }

    #[test]
    fn lerp_basics() {
        assert_eq!(lerp(208.0, 400.0, 0.0), 208.0);
        assert_eq!(lerp(208.0, 400.0, 1.0), 400.0);
        assert_eq!(lerp(0.0, 10.0, 0.5), 5.0);
    }

    #[test]
    fn hover_fade_ramps_and_reverses_continuously() {
        let mut fades = HoverFades::default();
        let t0 = Instant::now();
        let ms = |m: u64| t0 + Duration::from_millis(m);

        // Enter: 0 at the flip, mid-flight strictly between, 1 at 150ms.
        fades.set_at("pill", true, false, t0);
        assert_eq!(fades.value_at("pill", t0), 0.0);
        let mid = fades.value_at("pill", ms(75));
        assert!(mid > 0.0 && mid < 1.0, "mid-flight enter: {mid}");
        assert_eq!(fades.value_at("pill", ms(150)), 1.0);
        assert_eq!(fades.value_at("pill", ms(400)), 1.0, "clamps past the end");

        // Leave mid-flight re-anchors at the current value — no jump.
        fades.set_at("pill", true, false, t0);
        let at_flip = fades.value_at("pill", ms(75));
        fades.set_at("pill", false, false, ms(75));
        let after_flip = fades.value_at("pill", ms(75));
        assert!(
            (after_flip - at_flip).abs() < 1e-4,
            "continuity: {at_flip} vs {after_flip}"
        );
        let falling = fades.value_at("pill", ms(140));
        assert!(falling < after_flip, "fades back down");
        assert_eq!(fades.value_at("pill", ms(225)), 0.0, "lands at rest");
    }

    #[test]
    fn hover_fade_reduced_motion_snaps() {
        let mut fades = HoverFades::default();
        let t0 = Instant::now();
        fades.set_at("row", true, true, t0);
        assert_eq!(fades.value_at("row", t0), 1.0, "enter snaps to 1");
        fades.set_at("row", false, true, t0);
        assert_eq!(fades.value_at("row", t0), 0.0, "leave snaps to 0");
    }

    #[test]
    fn hover_fade_leave_without_enter_is_inert() {
        let mut fades = HoverFades::default();
        let t0 = Instant::now();
        fades.set_at("ghost", false, false, t0);
        assert!(fades.entries.is_empty(), "no entry for a leave-only key");
        assert_eq!(fades.value_at("ghost", t0), 0.0);
    }

    #[test]
    fn hover_tick_reports_flight_and_prunes() {
        let mut fades = HoverFades::default();
        let t0 = Instant::now();
        let ms = |m: u64| t0 + Duration::from_millis(m);

        fades.set_at("a", true, false, t0);
        // Mid-flight: active, frames must keep coming (read each frame).
        assert!(fades.tick_at(ms(50)));
        fades.value_at("a", ms(50));
        assert!(fades.tick_at(ms(100)));
        fades.value_at("a", ms(100));
        // Settled hovered (still read): no more frames needed, entry kept.
        assert!(!fades.tick_at(ms(200)));
        fades.value_at("a", ms(200));
        assert_eq!(fades.value_at("a", ms(250)), 1.0);

        // Leave → fades → settles at rest → entry evicted.
        fades.set_at("a", false, false, ms(250));
        assert!(fades.tick_at(ms(300)));
        fades.value_at("a", ms(300));
        assert!(!fades.tick_at(ms(500)), "settled at rest");
        assert!(fades.entries.is_empty(), "rest entries are pruned");
    }

    #[test]
    fn hover_tick_evicts_unread_entries() {
        // An element that unmounts mid-hover never sends its leave — a full
        // frame without a read drops the entry so a remount starts clean.
        let mut fades = HoverFades::default();
        let t0 = Instant::now();
        let ms = |m: u64| t0 + Duration::from_millis(m);
        fades.set_at("menu-row", true, false, t0);
        fades.tick_at(ms(16));
        fades.value_at("menu-row", ms(16)); // frame 1: mounted, read
        fades.tick_at(ms(32)); // frame 2: unmounted — no read
        fades.tick_at(ms(48)); // frame 3: a full unread frame has passed
        assert!(fades.entries.is_empty(), "unread entry evicted");
        assert_eq!(fades.value_at("menu-row", ms(64)), 0.0);
    }

    #[test]
    fn mix_endpoints_and_transparent_blend() {
        let rest = crate::theme::neutral(0.235);
        let hover = crate::theme::neutral(0.29);
        assert_eq!(mix(rest, hover, 0.0), rest);
        assert_eq!(mix(rest, hover, 1.0), hover);
        assert_eq!(mix(rest, hover, -1.0), rest, "t clamps low");
        assert_eq!(mix(rest, hover, 2.0), hover, "t clamps high");

        // Opaque blend: lightness moves monotonically between the endpoints.
        let mid = mix(rest, hover, 0.5);
        assert!(mid.l > rest.l && mid.l < hover.l, "mid lightness {}", mid.l);

        // Transparent → wash: alpha ramps, hue stays the wash's (premultiplied
        // — never a darkened grey mid-fade).
        let wash = crate::theme::white_alpha(0.06);
        let half = mix(gpui::transparent_black(), wash, 0.5);
        assert!((half.a - 0.03).abs() < 1e-4, "alpha midpoint {}", half.a);
        let half_rgba = Rgba::from(half);
        assert!(
            half_rgba.r > 0.99 && half_rgba.g > 0.99 && half_rgba.b > 0.99,
            "white wash keeps its hue: {half_rgba:?}"
        );
    }

    #[test]
    fn hover_spec_matches_tailwind_transition_colors() {
        assert_eq!(HOVER_FADE.duration_ms, 150);
        assert_eq!(HOVER_FADE.delay_ms, 0);
        assert_eq!(EASE_TAILWIND, CubicBezier::new(0.4, 0.0, 0.2, 1.0));
    }

    #[test]
    fn gspin_pulse_shape() {
        // Full at the cycle start, dim through the rest band, rising at the tail.
        assert_close(gspin_opacity(0.0, 0.1), 1.0, 1e-6, "cycle start");
        assert_close(gspin_opacity(0.45, 0.1), 0.1, 1e-6, "fully dim");
        assert_close(gspin_opacity(0.9, 0.1), 0.1, 1e-6, "rest band");
        assert_close(gspin_opacity(1.0, 0.1), 1.0, 1e-6, "wraps to full");
        let mid_fall = gspin_opacity(0.2, 0.1);
        assert!(mid_fall > 0.1 && mid_fall < 1.0, "eases down");
        let mid_rise = gspin_opacity(0.96, 0.1);
        assert!(mid_rise > 0.1 && mid_rise < 1.0, "eases up");
    }
}
