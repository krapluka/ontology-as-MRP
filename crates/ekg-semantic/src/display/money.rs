//! Exact money for display: a monetary literal is parsed as a fixed-point decimal with six
//! fractional digits and summed as an integer, never as a float, so a revenue or cost total
//! carries no binary rounding drift. Rendered with two decimals.

/// Fractional digits kept (micro-units of the currency).
const SCALE_DIGITS: usize = 6;
/// `10^SCALE_DIGITS`.
const SCALE: i128 = 1_000_000;
/// Half a cent in micro-units, for rounding to two decimals.
const HALF_CENT: i128 = 5_000;
/// One cent in micro-units.
const CENT: i128 = 10_000;

/// A monetary amount in micro-units of its (separately carried) currency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Money(i128);

impl Money {
    /// Parse a plain decimal literal (`-?digits[.digits]`, as an `xsd:decimal` is written).
    /// Digits beyond the sixth fractional place are rounded half away from zero. `None` for
    /// anything else (an exponent, a stray character, an empty value).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        let (negative, digits) = match value.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, value.strip_prefix('+').unwrap_or(value)),
        };
        let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
        let all_digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
        if whole.is_empty() || !all_digits(whole) || !all_digits(fraction) {
            return None;
        }
        let whole: i128 = whole.parse().ok()?;
        let kept = &fraction[..fraction.len().min(SCALE_DIGITS)];
        let mut micros: i128 = if kept.is_empty() {
            0
        } else {
            kept.parse().ok()?
        };
        for _ in kept.len()..SCALE_DIGITS {
            micros *= 10;
        }
        if fraction
            .as_bytes()
            .get(SCALE_DIGITS)
            .is_some_and(|d| *d >= b'5')
        {
            micros += 1;
        }
        let amount = whole.checked_mul(SCALE)?.checked_add(micros)?;
        Some(Self(if negative { -amount } else { amount }))
    }

    /// The sum, or `None` on overflow.
    #[must_use]
    pub fn checked_add(self, other: Self) -> Option<Self> {
        self.0.checked_add(other.0).map(Self)
    }

    /// The difference, or `None` on overflow.
    #[must_use]
    pub fn checked_sub(self, other: Self) -> Option<Self> {
        self.0.checked_sub(other.0).map(Self)
    }

    /// Render with two decimals, rounding half away from zero (`20700.10`).
    #[must_use]
    pub fn render(self) -> String {
        let magnitude = self.0.unsigned_abs();
        let cents = (magnitude + HALF_CENT.unsigned_abs()) / CENT.unsigned_abs();
        let sign = if self.0 < 0 && cents > 0 { "-" } else { "" };
        format!("{sign}{}.{:02}", cents / 100, cents % 100)
    }
}

/// Sum monetary literals exactly. `None` if any literal is not a plain decimal or the sum
/// overflows: a total over an unreadable value is never served.
#[must_use]
pub fn sum<'a>(values: impl IntoIterator<Item = &'a str>) -> Option<Money> {
    values
        .into_iter()
        .try_fold(Money::default(), |total, value| {
            total.checked_add(Money::parse(value)?)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_are_exact_where_floats_drift() {
        // 0.1 + 0.2 is 0.30000000000000004 in f64; the fixed-point sum is exactly 0.30.
        assert_eq!(
            sum(["0.1", "0.2"]).map(Money::render).as_deref(),
            Some("0.30")
        );
        assert_eq!(
            sum(["20700.10", "27600.20"]).map(Money::render).as_deref(),
            Some("48300.30")
        );
        // Many small amounts add up without accumulated error.
        let many = vec!["0.01"; 10_000];
        assert_eq!(sum(many).map(Money::render).as_deref(), Some("100.00"));
    }

    #[test]
    fn literals_parse_as_written_or_are_refused() {
        assert_eq!(
            Money::parse("12500").map(Money::render).as_deref(),
            Some("12500.00")
        );
        assert_eq!(
            Money::parse("12500.0").map(Money::render).as_deref(),
            Some("12500.00")
        );
        assert_eq!(
            Money::parse("-3.505").map(Money::render).as_deref(),
            Some("-3.51")
        );
        assert_eq!(
            Money::parse("0.004").map(Money::render).as_deref(),
            Some("0.00")
        );
        // Beyond six places, the seventh digit rounds.
        assert_eq!(Money::parse("1.0000005"), Money::parse("1.000001"));
        for bad in ["", "abc", "1e4", "1.2.3", ".5", "12 500", "NaN"] {
            assert_eq!(Money::parse(bad), None, "{bad:?} is not a money literal");
        }
        assert_eq!(
            sum(["10.00", "oops"]),
            None,
            "one unreadable value voids the sum"
        );
    }
}
