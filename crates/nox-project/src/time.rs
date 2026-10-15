//! Frame-accurate time. All positions are integer ticks in a rational
//! timebase; floating point seconds are only used for display.

use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use std::ops::{Add, AddAssign, Neg, Sub, SubAssign};

/// A reduced fraction `num / den` with `den > 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rational {
    pub num: i64,
    pub den: i64,
}

impl Rational {
    pub const fn new(num: i64, den: i64) -> Self {
        assert!(den != 0, "rational with zero denominator");
        let (num, den) = if den < 0 { (-num, -den) } else { (num, den) };
        let g = gcd(num.unsigned_abs(), den as u64) as i64;
        let g = if g == 0 { 1 } else { g };
        Self {
            num: num / g,
            den: den / g,
        }
    }

    pub fn inverse(self) -> Self {
        Self::new(self.den, self.num)
    }

    pub fn as_f64(self) -> f64 {
        self.num as f64 / self.den as f64
    }

    /// The closest fraction to `value` with a denominator of at most
    /// `max_den`, by continued fractions. `None` for non-finite values or
    /// if the result doesn't fit in `i64`.
    pub fn approximate(value: f64, max_den: i64) -> Option<Self> {
        if !value.is_finite() || value.abs() >= i64::MAX as f64 {
            return None;
        }
        // Convergents h/k of the continued fraction expansion.
        let (mut h0, mut h1, mut k0, mut k1) = (0i128, 1i128, 1i128, 0i128);
        let mut x = value;
        loop {
            let a = x.floor();
            let (h2, k2) = (a as i128 * h1 + h0, a as i128 * k1 + k0);
            if k2 > max_den as i128 {
                break;
            }
            (h0, h1, k0, k1) = (h1, h2, k1, k2);
            let frac = x - a;
            if frac.abs() < 1e-12 || (h1 as f64 / k1 as f64 - value).abs() <= value.abs() * 1e-15 {
                break;
            }
            x = 1.0 / frac;
        }
        (k1 != 0 && h1.abs() <= i64::MAX as i128).then(|| Self::new(h1 as i64, k1 as i64))
    }
}

impl PartialOrd for Rational {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Rational {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.num as i128 * other.den as i128).cmp(&(other.num as i128 * self.den as i128))
    }
}

const fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

fn gcd128(mut a: i128, mut b: i128) -> i128 {
    (a, b) = (a.abs(), b.abs());
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// A point in time (or a duration): `ticks * timebase` seconds.
///
/// Equality, ordering and hashing compare the value, so the same instant in
/// different timebases is equal. Addition and subtraction are exact: the
/// result uses the coarsest timebase both operands are multiples of.
#[derive(Debug, Clone, Copy)]
pub struct Time {
    pub ticks: i64,
    pub timebase: Rational,
}

impl Time {
    pub const fn new(ticks: i64, timebase: Rational) -> Self {
        Self { ticks, timebase }
    }

    pub const fn zero(timebase: Rational) -> Self {
        Self::new(0, timebase)
    }

    /// Frame `frame` at `fps` frames per second.
    pub fn from_frames(frame: i64, fps: Rational) -> Self {
        Self::new(frame, fps.inverse())
    }

    /// `seconds` in ticks of `timebase`, rounded to the nearest tick.
    pub fn from_seconds(seconds: f64, timebase: Rational) -> Self {
        Self::new((seconds / timebase.as_f64()).round() as i64, timebase)
    }

    /// Converts to another timebase, rounding to the nearest tick
    /// (ties away from zero), like FFmpeg's `av_rescale_q`.
    pub fn rescale(self, timebase: Rational) -> Self {
        let (num, den) = self.ratio_to(timebase);
        let half = den / 2;
        let ticks = if num >= 0 {
            (num + half) / den
        } else {
            (num - half) / den
        };
        Self::new(ticks as i64, timebase)
    }

    /// Converts to another timebase, rounding towards negative infinity.
    pub fn rescale_floor(self, timebase: Rational) -> Self {
        let (num, den) = self.ratio_to(timebase);
        Self::new(num.div_euclid(den) as i64, timebase)
    }

    /// Converts to another timebase if that is exact.
    pub fn rescale_exact(self, timebase: Rational) -> Option<Self> {
        let (num, den) = self.ratio_to(timebase);
        (num % den == 0).then(|| Self::new((num / den) as i64, timebase))
    }

    /// The whole frame at `fps` that contains this time.
    pub fn frame(self, fps: Rational) -> i64 {
        self.rescale_floor(fps.inverse()).ticks
    }

    /// `self` in units of `timebase` as the fraction `num / den`, `den > 0`.
    fn ratio_to(self, timebase: Rational) -> (i128, i128) {
        let num = self.ticks as i128 * self.timebase.num as i128 * timebase.den as i128;
        let den = self.timebase.den as i128 * timebase.num as i128;
        if den < 0 { (-num, -den) } else { (num, den) }
    }

    pub fn as_seconds(self) -> f64 {
        self.ticks as f64 * self.timebase.as_f64()
    }

    pub fn is_zero(self) -> bool {
        self.ticks == 0
    }

    pub fn is_negative(self) -> bool {
        self.ticks < 0
    }

    pub fn is_positive(&self) -> bool {
        self.ticks > 0
    }

    pub fn min(self, other: Self) -> Self {
        if other < self { other } else { self }
    }

    pub fn max(self, other: Self) -> Self {
        if other > self { other } else { self }
    }

    /// Both operands in the coarsest common timebase.
    fn align(self, other: Self) -> (i128, i128, Rational) {
        if self.timebase == other.timebase {
            return (self.ticks as i128, other.ticks as i128, self.timebase);
        }
        // For reduced p/q and r/s that is gcd(p, r) / lcm(q, s).
        let (a, b) = (self.timebase, other.timebase);
        let num = gcd128(a.num as i128, b.num as i128);
        let lcm = a.den as i128 / gcd128(a.den as i128, b.den as i128) * b.den as i128;
        let lhs = self.ticks as i128 * (a.num as i128 / num) * (lcm / a.den as i128);
        let rhs = other.ticks as i128 * (b.num as i128 / num) * (lcm / b.den as i128);
        let timebase = Rational::new(
            num.try_into().expect("timebase overflow"),
            lcm.try_into().expect("timebase overflow"),
        );
        (lhs, rhs, timebase)
    }

    /// The value as a reduced fraction of seconds, for hashing.
    fn seconds_fraction(self) -> (i128, i128) {
        let num = self.ticks as i128 * self.timebase.num as i128;
        let den = self.timebase.den as i128;
        let g = gcd128(num, den).max(1);
        (num / g, den / g)
    }
}

fn ticks(value: i128) -> i64 {
    value.try_into().expect("time overflow")
}

impl Add for Time {
    type Output = Time;
    fn add(self, rhs: Time) -> Time {
        // Zero takes the other operand's timebase, so that sums starting
        // from zero don't drift to a finer timebase.
        if rhs.ticks == 0 {
            return self;
        }
        if self.ticks == 0 {
            return rhs;
        }
        let (a, b, timebase) = self.align(rhs);
        Time::new(ticks(a + b), timebase)
    }
}

impl Sub for Time {
    type Output = Time;
    fn sub(self, rhs: Time) -> Time {
        if rhs.ticks == 0 {
            return self;
        }
        if self.ticks == 0 {
            return -rhs;
        }
        let (a, b, timebase) = self.align(rhs);
        Time::new(ticks(a - b), timebase)
    }
}

impl Neg for Time {
    type Output = Time;
    fn neg(self) -> Time {
        Time::new(-self.ticks, self.timebase)
    }
}

impl AddAssign for Time {
    fn add_assign(&mut self, rhs: Time) {
        *self = *self + rhs;
    }
}

impl SubAssign for Time {
    fn sub_assign(&mut self, rhs: Time) {
        *self = *self - rhs;
    }
}

impl PartialEq for Time {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Time {}

impl Hash for Time {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.seconds_fraction().hash(state);
    }
}

impl PartialOrd for Time {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Time {
    fn cmp(&self, other: &Self) -> Ordering {
        let lhs = self.ticks as i128 * self.timebase.num as i128 * other.timebase.den as i128;
        let rhs = other.ticks as i128 * other.timebase.num as i128 * self.timebase.den as i128;
        lhs.cmp(&rhs)
    }
}

/// A half-open interval `[start, start + duration)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimeRange {
    pub start: Time,
    pub duration: Time,
}

impl TimeRange {
    pub const fn new(start: Time, duration: Time) -> Self {
        Self { start, duration }
    }

    pub fn from_start_end(start: Time, end: Time) -> Self {
        Self::new(start, end - start)
    }

    /// The exclusive end, `start + duration`.
    pub fn end(self) -> Time {
        self.start + self.duration
    }

    pub fn contains(self, time: Time) -> bool {
        self.start <= time && time < self.end()
    }

    pub fn overlaps(self, other: TimeRange) -> bool {
        self.start < other.end() && other.start < self.end()
    }
}

/// Formats a frame number as non-drop-frame `HH:MM:SS:FF` timecode,
/// using the frame rate rounded to whole frames per second.
pub fn timecode(frame: i64, fps: Rational) -> String {
    format_timecode(frame, fps, false)
}

/// Whether `fps` is an NTSC rate (30000/1001, 60000/1001, …) that has a
/// drop-frame timecode.
pub fn supports_drop_frame(fps: Rational) -> bool {
    fps.den == 1001 && fps.num % 30000 == 0
}

/// Formats a frame number as timecode. With `drop_frame` and an NTSC rate
/// this is SMPTE drop-frame timecode `HH:MM:SS;FF`: frame numbers 0 and 1
/// (0–3 at 59.94) are skipped at the start of every minute except every
/// tenth, which keeps the timecode within 3.6 ms per hour of wall time.
/// Other rates ignore `drop_frame`.
pub fn format_timecode(frame: i64, fps: Rational, drop_frame: bool) -> String {
    let nominal = nominal_fps(fps);
    let drop_frame = drop_frame && supports_drop_frame(fps);
    let sign = if frame < 0 { "-" } else { "" };
    let mut frame = frame.unsigned_abs();
    if drop_frame {
        let dropped = nominal / 15; // per minute
        let per_minute = nominal * 60 - dropped;
        let per_ten_minutes = nominal * 600 - dropped * 9;
        let (tens, rest) = (frame / per_ten_minutes, frame % per_ten_minutes);
        frame += dropped * 9 * tens;
        if rest >= dropped {
            frame += dropped * ((rest - dropped) / per_minute);
        }
    }
    let (secs, ff) = (frame / nominal, frame % nominal);
    format!(
        "{sign}{:02}:{:02}:{:02}{}{:02}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        if drop_frame { ';' } else { ':' },
        ff
    )
}

/// Parses `HH:MM:SS:FF` (or `HH:MM:SS;FF` for drop-frame) to a frame
/// number. Fewer fields are allowed (`SS:FF`, `FF`). Drop-frame labels that
/// don't exist (e.g. `00:01:00;00`) are rejected.
pub fn parse_timecode(text: &str, fps: Rational) -> Option<i64> {
    let text = text.trim();
    let (negative, text) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let drop_frame = text.contains(';') && supports_drop_frame(fps);
    let fields = text
        .split([':', ';', '.'])
        .map(|f| f.parse::<u64>().ok())
        .collect::<Option<Vec<_>>>()?;
    if fields.is_empty() || fields.len() > 4 {
        return None;
    }
    let mut hmsf = [0u64; 4];
    hmsf[4 - fields.len()..].copy_from_slice(&fields);
    let [h, m, s, f] = hmsf;
    let nominal = nominal_fps(fps);
    if m >= 60 || s >= 60 || f >= nominal {
        return None;
    }
    let mut frame = (h * 3600 + m * 60 + s) * nominal + f;
    if drop_frame {
        let dropped = nominal / 15;
        if s == 0 && f < dropped && m % 10 != 0 {
            return None;
        }
        let minutes = h * 60 + m;
        frame -= dropped * (minutes - minutes / 10);
    }
    let frame = i64::try_from(frame).ok()?;
    Some(if negative { -frame } else { frame })
}

/// The frame rate rounded to whole frames, as used for timecode labels.
pub fn nominal_fps(fps: Rational) -> u64 {
    ((fps.num + fps.den / 2) / fps.den).max(1) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rational_is_reduced_and_normalized() {
        assert_eq!(Rational::new(2, 4), Rational::new(1, 2));
        assert_eq!(Rational::new(1, -25), Rational { num: -1, den: 25 });
    }

    #[test]
    fn rational_approximation() {
        let ntsc = Rational::approximate(30000.0 / 1001.0, 1_000_000);
        assert_eq!(ntsc, Some(Rational::new(30000, 1001)));
        assert_eq!(
            Rational::approximate(24.0, 1000),
            Some(Rational::new(24, 1))
        );
        assert_eq!(Rational::approximate(0.1, 1000), Some(Rational::new(1, 10)));
        assert_eq!(
            Rational::approximate(-2.5, 1000),
            Some(Rational::new(-5, 2))
        );
        assert_eq!(Rational::approximate(f64::NAN, 1000), None);
    }

    #[test]
    fn rescale_rounds_to_nearest() {
        let ntsc = Rational::new(1001, 30000);
        let ms = Rational::new(1, 1000);
        // Frame 1 at 29.97 fps is 33.366 ms.
        assert_eq!(Time::new(1, ntsc).rescale(ms).ticks, 33);
        assert_eq!(Time::new(-1, ntsc).rescale(ms).ticks, -33);
        assert_eq!(Time::new(30000, ms).rescale(ntsc).ticks, 899);
    }

    #[test]
    fn rescale_floor_and_exact() {
        let ms = Rational::new(1, 1000);
        let fps = Rational::new(25, 1);
        assert_eq!(Time::new(79, ms).frame(fps), 1);
        assert_eq!(Time::new(-1, ms).frame(fps), -1);
        assert_eq!(
            Time::new(80, ms)
                .rescale_exact(fps.inverse())
                .map(|t| t.ticks),
            Some(2)
        );
        assert_eq!(Time::new(79, ms).rescale_exact(fps.inverse()), None);
    }

    #[test]
    fn comparison_across_timebases() {
        let a = Time::new(1, Rational::new(1, 25));
        let b = Time::new(40, Rational::new(1, 1000));
        assert_eq!(a.cmp(&b), Ordering::Equal);
        assert_eq!(a, b);
        assert!(Time::new(2, Rational::new(1, 25)) > b);

        use std::collections::hash_map::DefaultHasher;
        let hash = |t: Time| {
            let mut h = DefaultHasher::new();
            t.hash(&mut h);
            h.finish()
        };
        assert_eq!(hash(a), hash(b));
    }

    #[test]
    fn exact_arithmetic_across_timebases() {
        let frame_25 = Time::new(1, Rational::new(1, 25));
        let frame_ntsc = Time::new(1, Rational::new(1001, 30000));
        let sum = frame_25 + frame_ntsc;
        // 1/25 + 1001/30000 = 1200/30000 + 1001/30000
        assert_eq!(sum, Time::new(2201, Rational::new(1, 30000)));
        assert_eq!(sum - frame_ntsc, frame_25);
        let same = Time::new(3, Rational::new(1, 24)) + Time::new(2, Rational::new(1, 24));
        assert_eq!((same.ticks, same.timebase), (5, Rational::new(1, 24)));
        // 1/24 and 1/48 align to 1/48 rather than something finer.
        let mixed = Time::new(1, Rational::new(1, 24)) + Time::new(1, Rational::new(1, 48));
        assert_eq!((mixed.ticks, mixed.timebase), (3, Rational::new(1, 48)));
        // Zero adopts the other timebase.
        let zero = Time::zero(Rational::new(1, 24));
        let frame = Time::new(1, Rational::new(1, 25));
        assert_eq!((zero + frame).timebase, frame.timebase);
        assert_eq!((frame + zero).timebase, frame.timebase);
        assert_eq!((zero - frame).timebase, frame.timebase);
        assert_eq!(zero - frame, -frame);
    }

    #[test]
    fn time_range() {
        let fps = Rational::new(24, 1);
        let range = TimeRange::new(Time::from_frames(10, fps), Time::from_frames(5, fps));
        assert_eq!(range.end(), Time::from_frames(15, fps));
        assert!(range.contains(Time::from_frames(10, fps)));
        assert!(!range.contains(Time::from_frames(15, fps)));
        let next = TimeRange::new(range.end(), Time::from_frames(1, fps));
        assert!(!range.overlaps(next));
    }

    #[test]
    fn timecode_formatting() {
        let fps = Rational::new(25, 1);
        assert_eq!(timecode(0, fps), "00:00:00:00");
        assert_eq!(timecode(25 * 3661 + 3, fps), "01:01:01:03");
        assert_eq!(timecode(-26, fps), "-00:00:01:01");
        assert_eq!(timecode(30, Rational::new(30000, 1001)), "00:00:01:00");
    }

    #[test]
    fn drop_frame_timecode() {
        let fps = Rational::new(30000, 1001);
        let df = |frame| format_timecode(frame, fps, true);
        assert_eq!(df(0), "00:00:00;00");
        assert_eq!(df(1799), "00:00:59;29");
        assert_eq!(df(1800), "00:01:00;02");
        assert_eq!(df(17981), "00:09:59;29");
        assert_eq!(df(17982), "00:10:00;00");
        assert_eq!(df(17984), "00:10:00;02");
        // One hour of 29.97 is 107892 frames and exactly 01:00:00;00.
        assert_eq!(df(107892), "01:00:00;00");

        let fps60 = Rational::new(60000, 1001);
        assert_eq!(format_timecode(3599, fps60, true), "00:00:59;59");
        assert_eq!(format_timecode(3600, fps60, true), "00:01:00;04");
        assert_eq!(format_timecode(215784, fps60, true), "01:00:00;00");

        // Non-NTSC rates have no drop-frame timecode.
        assert_eq!(
            format_timecode(25, Rational::new(25, 1), true),
            "00:00:01:00"
        );
    }

    #[test]
    fn timecode_parsing_round_trips() {
        for fps in [
            Rational::new(25, 1),
            Rational::new(24000, 1001),
            Rational::new(30000, 1001),
            Rational::new(60000, 1001),
        ] {
            for drop_frame in [false, true] {
                for frame in (0..300_000).step_by(997).chain([1799, 1800, 17982, -5]) {
                    let text = format_timecode(frame, fps, drop_frame);
                    assert_eq!(parse_timecode(&text, fps), Some(frame), "{text} at {fps:?}");
                }
            }
        }
        let ntsc = Rational::new(30000, 1001);
        assert_eq!(parse_timecode("00:01:00;00", ntsc), None);
        assert_eq!(parse_timecode("00:01:00:00", ntsc), Some(1800));
        assert_eq!(parse_timecode("1:00", Rational::new(25, 1)), Some(25));
        assert_eq!(parse_timecode("00:00:00:25", Rational::new(25, 1)), None);
        assert_eq!(parse_timecode("x", Rational::new(25, 1)), None);
    }
}
