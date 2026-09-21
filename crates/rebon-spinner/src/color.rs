//! Color helpers for spinner rows and message effects.
//!
//! Four operations: linear RGB interpolation with rounded channels,
//! `"rgb(r,g,b)"` formatting, HSL hue conversion for the voice-mode waveform
//! palette, and `"rgb(r,g,b)"` parsing. Theme lookup and any cache of resolved
//! colors belong to the caller.

/// An RGB color: three 0-255 sRGB channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgbColor {
    /// Red component, 0-255.
    pub r: u8,
    /// Green component, 0-255.
    pub g: u8,
    /// Blue component, 0-255.
    pub b: u8,
}

impl RgbColor {
    /// Construct a new color from raw components.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// Linearly interpolate between two RGB colors.
///
/// `t` is the position along the gradient: `0.0` returns `c1`, `1.0`
/// returns `c2`. Out-of-range `t` is **not** clamped; callers pass a
/// value in `[0, 1]`.
///
/// Each channel is computed as `round(a + (b - a) * t)` and clamped to
/// `[0, 255]` after rounding so the conversion to `u8` is total. Rounding is
/// half-away-from-zero, not banker's rounding.
pub fn interpolate_color(c1: RgbColor, c2: RgbColor, t: f64) -> RgbColor {
    fn lerp_u8(a: u8, b: u8, t: f64) -> u8 {
        let v = (a as f64 + (b as f64 - a as f64) * t).round();
        v.clamp(0.0, 255.0) as u8
    }
    RgbColor {
        r: lerp_u8(c1.r, c2.r, t),
        g: lerp_u8(c1.g, c2.g, t),
        b: lerp_u8(c1.b, c2.b, t),
    }
}

/// Format an RGB color as `"rgb(r,g,b)"`. Channels are unpadded, so the
/// string stays stable for golden comparisons.
pub fn to_rgb_string(c: RgbColor) -> String {
    format!("rgb({},{},{})", c.r, c.g, c.b)
}

/// HSL hue (0-360) to RGB, with saturation `0.7` and lightness `0.6`
/// — the voice-mode waveform parameters. Hue is wrapped into `[0, 360)`
/// by taking `h % 360` and adding `360` when the result is negative.
pub fn hue_to_rgb(hue: f64) -> RgbColor {
    let mut h = hue % 360.0;
    if h < 0.0 {
        h += 360.0;
    }
    let s: f64 = 0.7;
    let l: f64 = 0.6;
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - (((h / 60.0) % 2.0) - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = if h < 60.0 {
        (c, x, 0.0)
    } else if h < 120.0 {
        (x, c, 0.0)
    } else if h < 180.0 {
        (0.0, c, x)
    } else if h < 240.0 {
        (0.0, x, c)
    } else if h < 300.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    RgbColor {
        r: ((r + m) * 255.0).round() as u8,
        g: ((g + m) * 255.0).round() as u8,
        b: ((b + m) * 255.0).round() as u8,
    }
}

/// Parse a `"rgb(r,g,b)"` color string. Tolerates whitespace inside the
/// parens; rejects any other format, more than three channels, and any
/// channel above 255. Returns `None` on non-matching input.
pub fn parse_rgb(s: &str) -> Option<RgbColor> {
    let body = s.strip_prefix("rgb(")?.strip_suffix(')')?;
    let mut parts = body.split(',');
    let r = parts.next()?.trim().parse::<u32>().ok()?;
    let g = parts.next()?.trim().parse::<u32>().ok()?;
    let b = parts.next()?.trim().parse::<u32>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    if r > 255 || g > 255 || b > 255 {
        return None;
    }
    Some(RgbColor {
        r: r as u8,
        g: g as u8,
        b: b as u8,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolate_zero_returns_first() {
        let a = RgbColor::new(10, 20, 30);
        let b = RgbColor::new(200, 100, 50);
        assert_eq!(interpolate_color(a, b, 0.0), a);
    }

    #[test]
    fn interpolate_one_returns_second() {
        let a = RgbColor::new(10, 20, 30);
        let b = RgbColor::new(200, 100, 50);
        assert_eq!(interpolate_color(a, b, 1.0), b);
    }

    #[test]
    fn interpolate_half_is_midpoint_rounded() {
        let a = RgbColor::new(0, 0, 0);
        let b = RgbColor::new(255, 100, 50);
        let mid = interpolate_color(a, b, 0.5);
        assert_eq!(mid, RgbColor::new(128, 50, 25));
    }

    #[test]
    fn interpolate_uses_math_round_not_floor() {
        // 0 + (1 - 0) * 0.5 = 0.5; half-up rounding yields 1;
        // f64::round gives 1.0 here — it rounds half away from zero, so
        // banker's rounding does not apply.
        let a = RgbColor::new(0, 0, 0);
        let b = RgbColor::new(1, 1, 1);
        assert_eq!(interpolate_color(a, b, 0.5), RgbColor::new(1, 1, 1));
    }

    #[test]
    fn interpolate_clamps_high_t_does_not_panic() {
        let a = RgbColor::new(0, 0, 0);
        let b = RgbColor::new(100, 100, 100);
        // t > 1: the lerp is not clamped, but we cap at 255 so the u8
        // conversion stays total.
        let r = interpolate_color(a, b, 10.0);
        assert_eq!(r, RgbColor::new(255, 255, 255));
    }

    #[test]
    fn interpolate_negative_t_does_not_panic() {
        let a = RgbColor::new(100, 100, 100);
        let b = RgbColor::new(200, 200, 200);
        let r = interpolate_color(a, b, -10.0);
        // 100 + (200-100)*-10 = -900; clamped to 0.
        assert_eq!(r, RgbColor::new(0, 0, 0));
    }

    #[test]
    fn to_rgb_string_no_padding() {
        let s = to_rgb_string(RgbColor::new(1, 2, 3));
        assert_eq!(s, "rgb(1,2,3)");
    }

    #[test]
    fn to_rgb_string_max() {
        let s = to_rgb_string(RgbColor::new(255, 255, 255));
        assert_eq!(s, "rgb(255,255,255)");
    }

    #[test]
    fn parse_rgb_strict() {
        assert_eq!(parse_rgb("rgb(1,2,3)"), Some(RgbColor::new(1, 2, 3)));
    }

    #[test]
    fn parse_rgb_with_spaces() {
        assert_eq!(
            parse_rgb("rgb( 10, 20, 30 )"),
            Some(RgbColor::new(10, 20, 30))
        );
    }

    #[test]
    fn parse_rgb_round_trip() {
        let c = RgbColor::new(123, 45, 67);
        assert_eq!(parse_rgb(&to_rgb_string(c)), Some(c));
    }

    #[test]
    fn parse_rgb_rejects_garbage() {
        assert_eq!(parse_rgb(""), None);
        assert_eq!(parse_rgb("hello"), None);
        assert_eq!(parse_rgb("rgb()"), None);
        assert_eq!(parse_rgb("rgb(1,2)"), None);
        assert_eq!(parse_rgb("rgb(1,2,3,4)"), None);
        assert_eq!(parse_rgb("rgba(1,2,3,1)"), None);
    }

    #[test]
    fn parse_rgb_rejects_overflow() {
        assert_eq!(parse_rgb("rgb(256,0,0)"), None);
        assert_eq!(parse_rgb("rgb(0,256,0)"), None);
        assert_eq!(parse_rgb("rgb(0,0,256)"), None);
    }

    #[test]
    fn hue_to_rgb_red_at_zero() {
        // Hue 0, s=0.7, l=0.6 → red-leaning warm color.
        let c = hue_to_rgb(0.0);
        // Pinned values from running hue_to_rgb by hand:
        // c = (1 - |2*0.6 - 1|) * 0.7 = (1 - 0.2) * 0.7 = 0.56
        // x = 0.56 * (1 - |((0/60)%2)-1|) = 0.56 * 0 = 0
        // m = 0.6 - 0.56/2 = 0.32
        // r = (0.56 + 0.32) * 255 = 0.88 * 255 = 224.4 → 224
        // g = (0 + 0.32) * 255 = 0.32 * 255 = 81.6 → 82
        // b = (0 + 0.32) * 255 = 0.32 * 255 = 81.6 → 82
        assert_eq!(c, RgbColor::new(224, 82, 82));
    }

    #[test]
    fn hue_to_rgb_wraps_negative() {
        // -10 should be the same as 350.
        assert_eq!(hue_to_rgb(-10.0), hue_to_rgb(350.0));
    }

    #[test]
    fn hue_to_rgb_wraps_above_360() {
        // 370 should be the same as 10.
        assert_eq!(hue_to_rgb(370.0), hue_to_rgb(10.0));
    }

    #[test]
    fn hue_to_rgb_six_sectors_distinct() {
        // Each 60° sector should produce a distinct dominant component
        // ordering.
        let red = hue_to_rgb(0.0);
        let yellow = hue_to_rgb(60.0);
        let green = hue_to_rgb(120.0);
        let cyan = hue_to_rgb(180.0);
        let blue = hue_to_rgb(240.0);
        let magenta = hue_to_rgb(300.0);
        assert!(red.r > red.b, "red should be R-dominant");
        assert!(yellow.r > yellow.b && yellow.g > yellow.b);
        assert!(green.g > green.r);
        assert!(cyan.g > cyan.r && cyan.b > cyan.r);
        assert!(blue.b > blue.g);
        assert!(magenta.r > magenta.g && magenta.b > magenta.g);
    }
}
