//! Frame-accurate time. All positions are integer ticks in a rational
//! timebase; floating point seconds are only used for display.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// A reduced fraction `num / den` with `den > 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

/// A point in time: `ticks * timebase` seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

    /// Converts to another timebase, rounding to the nearest tick
    /// (ties away from zero), like FFmpeg's `av_rescale_q`.
    pub fn rescale(self, timebase: Rational) -> Self {
        let num = self.ticks as i128 * self.timebase.num as i128 * timebase.den as i128;
        let den = self.timebase.den as i128 * timebase.num as i128;
        let (num, den) = if den < 0 { (-num, -den) } else { (num, den) };
        let half = den / 2;
        let ticks = if num >= 0 {
            (num + half) / den
        } else {
            (num - half) / den
        };
        Self::new(ticks as i64, timebase)
    }

    pub fn as_seconds(self) -> f64 {
        self.ticks as f64 * self.timebase.as_f64()
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

/// Formats a frame number as non-drop-frame `HH:MM:SS:FF` timecode,
/// using the frame rate rounded to whole frames per second.
pub fn timecode(frame: i64, fps: Rational) -> String {
    let nominal = ((fps.num + fps.den / 2) / fps.den).max(1) as u64;
    let sign = if frame < 0 { "-" } else { "" };
    let frame = frame.unsigned_abs();
    let (secs, ff) = (frame / nominal, frame % nominal);
    format!(
        "{sign}{:02}:{:02}:{:02}:{:02}",
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        ff
    )
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
    fn rescale_rounds_to_nearest() {
        let ntsc = Rational::new(1001, 30000);
        let ms = Rational::new(1, 1000);
        // Frame 1 at 29.97 fps is 33.366 ms.
        assert_eq!(Time::new(1, ntsc).rescale(ms).ticks, 33);
        assert_eq!(Time::new(-1, ntsc).rescale(ms).ticks, -33);
        assert_eq!(Time::new(30000, ms).rescale(ntsc).ticks, 899);
    }

    #[test]
    fn comparison_across_timebases() {
        let a = Time::new(1, Rational::new(1, 25));
        let b = Time::new(40, Rational::new(1, 1000));
        assert_eq!(a.cmp(&b), Ordering::Equal);
        assert!(Time::new(2, Rational::new(1, 25)) > b);
    }

    #[test]
    fn timecode_formatting() {
        let fps = Rational::new(25, 1);
        assert_eq!(timecode(0, fps), "00:00:00:00");
        assert_eq!(timecode(25 * 3661 + 3, fps), "01:01:01:03");
        assert_eq!(timecode(-26, fps), "-00:00:01:01");
        assert_eq!(timecode(30, Rational::new(30000, 1001)), "00:00:01:00");
    }
}
