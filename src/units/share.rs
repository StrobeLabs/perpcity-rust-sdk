//! A fraction a caller chose, and the exact arithmetic of applying it.
//!
//! A strategy thinks in fractions: a quarter of the budget, half the band's
//! depth, this wallet's slice of the cohort. Applying one to a count is
//! where floats leak back into exact arithmetic — `(depth as f64 * 0.25)
//! as u128` is the shape every call site reaches for, and it rounds
//! differently at every site. [`Share`] is the fraction as a millionth, and
//! the scaling methods on the counts and deltas take it, so the truncation
//! happens once, in one direction, in one place.
//!
//! It is not a [`Ratio`](super::Ratio), though both are `u32` millionths on
//! the same scale. A `Ratio` is a word the contract stores and its domain
//! is the `uint24` the modules hold it in; a `Share` is the caller's own,
//! bounded by one because that is what a fraction of a whole is. The
//! provenance is the type, and a function that takes one does not take the
//! other.

use std::num::NonZeroUsize;

use alloy::primitives::U256;

use crate::errors::ValidationError;

use super::{BIGINT_1E6, F64_1E6};

/// The whole, in millionths.
const ONE_E6: u32 = 1_000_000;

/// A fraction of a whole, held as millionths: `250_000` is a quarter and
/// `1_000_000` is all of it.
///
/// Serialises as the fraction a person writes (`0.25`), because the places
/// that hold one are a strategy's configuration rather than a contract's
/// word, and deserialising runs the same bound as constructing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct Share(u32);

impl serde::Serialize for Share {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_f64(self.fraction())
    }
}

impl<'de> serde::Deserialize<'de> for Share {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::try_from(f64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

impl Share {
    /// None of it.
    pub const ZERO: Self = Self(0);

    /// All of it.
    pub const ONE: Self = Self(ONE_E6);

    /// From millionths.
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidShare`] past `1_000_000`: a share is of a
    /// whole, and there is no more than the whole.
    pub fn from_e6(e6: u32) -> Result<Self, ValidationError> {
        if e6 <= ONE_E6 {
            Ok(Self(e6))
        } else {
            Err(ValidationError::InvalidShare {
                reason: format!("{e6} millionths is more than the whole"),
            })
        }
    }

    /// The millionths, for exact comparison.
    pub const fn e6(self) -> u32 {
        self.0
    }

    /// The fraction a person reads, `0.25` for a quarter. Exact: a
    /// millionth round-trips through `f64`.
    pub fn fraction(self) -> f64 {
        f64::from(self.0) / F64_1E6
    }

    /// Whether it is none of the whole.
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    /// Whether it is the whole.
    pub const fn is_whole(self) -> bool {
        self.0 == ONE_E6
    }

    /// The rest of the whole.
    pub const fn complement(self) -> Self {
        Self(ONE_E6 - self.0)
    }

    /// Weights as shares that sum to exactly [`Self::ONE`].
    ///
    /// The weights need not sum to anything in particular: each becomes
    /// its fraction of their total, floored to a millionth, and the
    /// millionths the flooring left over go to the largest remainders, so
    /// `[1.0, 1.0, 1.0]` is `[333_334, 333_333, 333_333]` and not a
    /// `999_999` that leaves an atom unassigned on every split.
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidShare`] when a weight is negative or not
    /// finite, or when they sum to zero, which is no partition at all.
    pub fn partition(weights: &[f64]) -> Result<Vec<Self>, ValidationError> {
        let invalid = |reason: String| ValidationError::InvalidShare { reason };
        if let Some(bad) = weights.iter().find(|w| !w.is_finite() || **w < 0.0) {
            return Err(invalid(format!(
                "weight {bad} is not a finite non-negative number"
            )));
        }
        let total: f64 = weights.iter().sum();
        if total <= 0.0 {
            return Err(invalid(
                "weights sum to zero, so there is nothing to partition".into(),
            ));
        }
        let scaled: Vec<f64> = weights.iter().map(|w| w / total * F64_1E6).collect();
        let mut shares: Vec<u32> = scaled.iter().map(|s| s.floor() as u32).collect();
        let assigned: u32 = shares.iter().sum();
        let mut order: Vec<usize> = (0..shares.len()).collect();
        order.sort_by(|&a, &b| {
            (scaled[b] - scaled[b].floor())
                .partial_cmp(&(scaled[a] - scaled[a].floor()))
                .expect("finite remainders compare")
                .then(a.cmp(&b))
        });
        for &i in order.iter().take(ONE_E6.saturating_sub(assigned) as usize) {
            shares[i] += 1;
        }
        Ok(shares.into_iter().map(Self).collect())
    }

    /// `x` scaled by this share, truncated toward zero as the chain's
    /// division truncates. In 256 bits, so no `u128` leaves the width.
    pub(crate) fn scale_u128(self, x: u128) -> u128 {
        let scaled = U256::from(x) * U256::from(self.0) / BIGINT_1E6;
        u128::try_from(scaled).expect("a share of a u128 fits a u128")
    }

    /// `x` scaled by this share, its magnitude truncated toward zero so the
    /// result never crosses zero.
    pub(crate) fn scale_i128(self, x: i128) -> i128 {
        let magnitude = self.scale_u128(x.unsigned_abs()) as i128;
        if x < 0 { -magnitude } else { magnitude }
    }

    /// `part` as a share of `whole`, to the nearest millionth; `None` when
    /// `whole` is zero or `part` exceeds it.
    pub(crate) fn of_u128(part: u128, whole: u128) -> Option<Self> {
        if whole == 0 || part > whole {
            return None;
        }
        let whole = U256::from(whole);
        let e6 = (U256::from(part) * BIGINT_1E6 + whole / U256::from(2)) / whole;
        Some(Self(
            u32::try_from(e6).expect("a part of its whole is at most one"),
        ))
    }

    /// `x` in `parts` equal pieces, the remainder spread one atom each
    /// over the first pieces, so the pieces sum to `x`.
    pub(crate) fn split_u128(x: u128, parts: NonZeroUsize) -> Vec<u128> {
        let n = parts.get() as u128;
        let (each, extra) = (x / n, x % n);
        (0..n).map(|i| each + u128::from(i < extra)).collect()
    }

    /// `x` by `weights`, which must sum to [`Self::ONE`]: each piece is
    /// floored and the atoms the flooring left over go to the largest
    /// remainders, so the pieces sum to `x`.
    pub(crate) fn split_weighted_u128(
        x: u128,
        weights: &[Self],
    ) -> Result<Vec<u128>, ValidationError> {
        let total: u64 = weights.iter().map(|w| u64::from(w.0)).sum();
        if total != u64::from(ONE_E6) {
            return Err(ValidationError::InvalidShare {
                reason: format!("weights sum to {total} millionths, not the whole"),
            });
        }
        let x = U256::from(x);
        let (mut pieces, remainders): (Vec<u128>, Vec<U256>) = weights
            .iter()
            .map(|w| {
                let scaled = x * U256::from(w.0);
                let piece = u128::try_from(scaled / BIGINT_1E6).expect("a share of x fits");
                (piece, scaled % BIGINT_1E6)
            })
            .unzip();
        let assigned: u128 = pieces.iter().sum();
        let left = u128::try_from(x).expect("x came from a u128") - assigned;
        let mut order: Vec<usize> = (0..pieces.len()).collect();
        order.sort_by(|&a, &b| remainders[b].cmp(&remainders[a]).then(a.cmp(&b)));
        for &i in order.iter().take(left as usize) {
            pieces[i] += 1;
        }
        Ok(pieces)
    }
}

impl TryFrom<f64> for Share {
    type Error = ValidationError;

    /// A fraction a person wrote, rounded to the nearest millionth.
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidShare`] when it is not finite or lies
    /// outside `[0, 1]`.
    fn try_from(fraction: f64) -> Result<Self, ValidationError> {
        if !fraction.is_finite() || !(0.0..=1.0).contains(&fraction) {
            return Err(ValidationError::InvalidShare {
                reason: format!("a share is a fraction in [0, 1], got {fraction}"),
            });
        }
        Ok(Self((fraction * F64_1E6).round() as u32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A share is bounded by the whole at both doors, and the fraction
    /// round-trips exactly.
    #[test]
    fn a_share_is_at_most_the_whole() {
        let quarter = Share::try_from(0.25).unwrap();
        assert_eq!(quarter.e6(), 250_000);
        assert_eq!(quarter.fraction(), 0.25);
        assert_eq!(quarter.complement().e6(), 750_000);
        assert_eq!(Share::from_e6(1_000_000).unwrap(), Share::ONE);
        assert!(Share::from_e6(1_000_001).is_err());
        for bad in [-0.1, 1.0000001, f64::NAN, f64::INFINITY] {
            assert!(Share::try_from(bad).is_err(), "{bad}");
        }
        // Nearest millionth, not floored: a config's 0.3 is 300_000.
        assert_eq!(Share::try_from(0.3).unwrap().e6(), 300_000);
        assert_eq!(Share::try_from(0.0000004).unwrap(), Share::ZERO);
    }

    /// The wire form is the fraction, so a config writes `0.25` and reads
    /// it back under the same bound.
    #[test]
    fn the_wire_form_is_the_fraction() {
        let quarter = Share::try_from(0.25).unwrap();
        assert_eq!(serde_json::to_string(&quarter).unwrap(), "0.25");
        assert_eq!(serde_json::from_str::<Share>("0.25").unwrap(), quarter);
        assert!(serde_json::from_str::<Share>("1.5").is_err());
    }

    /// A partition sums to the whole however the weights were written.
    #[test]
    fn a_partition_sums_to_exactly_one() {
        let thirds = Share::partition(&[1.0, 1.0, 1.0]).unwrap();
        assert_eq!(
            thirds.iter().map(|s| s.e6()).collect::<Vec<_>>(),
            [333_334, 333_333, 333_333]
        );
        let lopsided = Share::partition(&[0.7, 0.2, 0.1]).unwrap();
        assert_eq!(lopsided.iter().map(|s| s.e6()).sum::<u32>(), 1_000_000);
        assert_eq!(
            Share::partition(&[5.0]).unwrap(),
            vec![Share::ONE],
            "one weight is the whole, whatever its size"
        );
        assert!(Share::partition(&[0.0, 0.0]).is_err());
        assert!(Share::partition(&[1.0, -1.0]).is_err());
        assert!(Share::partition(&[]).is_err());
    }

    /// Scaling truncates toward zero on both signs, so a scaled delta never
    /// crosses zero and a scaled count never rounds up past its share.
    #[test]
    fn scaling_truncates_toward_zero() {
        let third = Share::from_e6(333_333).unwrap();
        assert_eq!(third.scale_u128(10), 3);
        assert_eq!(third.scale_i128(-10), -3);
        assert_eq!(Share::ONE.scale_u128(u128::MAX), u128::MAX);
        assert_eq!(Share::ZERO.scale_u128(u128::MAX), 0);
    }

    /// A part's share of its whole is the nearest millionth, and a part
    /// larger than its whole is not a share.
    #[test]
    fn a_part_is_a_share_of_its_whole() {
        assert_eq!(Share::of_u128(1, 3), Some(Share::from_e6(333_333).unwrap()));
        assert_eq!(Share::of_u128(2, 3), Some(Share::from_e6(666_667).unwrap()));
        assert_eq!(Share::of_u128(7, 7), Some(Share::ONE));
        assert_eq!(Share::of_u128(0, 7), Some(Share::ZERO));
        assert_eq!(Share::of_u128(8, 7), None);
        assert_eq!(Share::of_u128(0, 0), None);
    }

    /// Both splits conserve the amount: what goes out in pieces is exactly
    /// what went in, with the remainder's atoms placed by rule.
    #[test]
    fn a_split_conserves_the_amount() {
        let n = NonZeroUsize::new(3).unwrap();
        assert_eq!(Share::split_u128(10, n), [4, 3, 3]);
        assert_eq!(Share::split_u128(2, n), [1, 1, 0]);

        let weights = Share::partition(&[1.0, 1.0, 1.0]).unwrap();
        let pieces = Share::split_weighted_u128(100, &weights).unwrap();
        assert_eq!(pieces.iter().sum::<u128>(), 100);
        // 33.3334 / 33.3333 / 33.3333: one floors to 33, the leftover atom
        // goes to the largest remainder, which is the first.
        assert_eq!(pieces, [34, 33, 33]);

        let lopsided = Share::partition(&[0.7, 0.2, 0.1]).unwrap();
        let pieces = Share::split_weighted_u128(1_000_001, &lopsided).unwrap();
        assert_eq!(pieces.iter().sum::<u128>(), 1_000_001);

        let short = [Share::from_e6(500_000).unwrap(); 1];
        assert!(
            Share::split_weighted_u128(100, &short).is_err(),
            "weights that do not reach the whole leave atoms nowhere"
        );
    }
}
