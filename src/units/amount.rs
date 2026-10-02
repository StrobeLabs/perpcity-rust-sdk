//! The two assets a market settles in, each a count of atoms.
//!
//! Both are six-decimal on chain, so both are `u128` counts of the same
//! width, and before these types only a field's name said which asset a
//! number was. The pair that matters is [`PerpAtoms`] and [`UsdcAtoms`]:
//! they are indistinguishable as primitives, and the one operation that
//! crosses between them is a valuation at a [`Price`].

use alloy::primitives::{I256, U256};

use super::fixed_point::{Rounding, mul_div, s_full_mul_div, to_i256};
use crate::constants::Q96;
use crate::errors::ValidationError;

use super::price::Price;
use super::{F64_1E6, MAX_SAFE_F64_INT, count, delta};

count! {
    /// USDC the chain holds: a count of atoms, each a millionth of a
    /// dollar and the smallest amount a market can settle.
    ///
    /// The `f64` dollars a person reads come from [`Self::usdc`] and are
    /// the derived view, exact below 2^53 atoms (about nine billion
    /// dollars) and approximate above it.
    UsdcAtoms(u128) as atoms
}

count! {
    /// The market's own token: a count of atoms, also six decimals, and
    /// never interchangeable with [`UsdcAtoms`] however alike the two look
    /// as integers.
    ///
    /// Capacity, open interest and a band's holdings are all counted in
    /// these. [`Self::value_at`] is the only way to a USDC figure.
    PerpAtoms(u128) as atoms
}

delta! {
    /// USDC owed or owing: the signed count a settle computes.
    ///
    /// Every component of a settle preview is one of these, positive
    /// toward the position and negative away from it, and they add because
    /// the protocol's supply bound keeps any sum far inside `i128`.
    UsdcDelta(i128) as atoms, magnitude UsdcAtoms
}

delta! {
    /// A signed exposure in the market's token: positive long, negative
    /// short, as a position's `delta` stores it.
    PerpDelta(i128) as atoms, magnitude PerpAtoms
}

/// A human amount as a signed count of atoms, with the fractional atom
/// floored.
///
/// Floored, not truncated: a negative amount rounds away from zero, so
/// `-1.1234567` is `-1_123_457` atoms rather than `-1_123_456`. That is a
/// choice about a caller's own input, not a transcription of the contract,
/// which truncates toward zero in its signed division — nothing here feeds
/// a port, and the behaviour is pinned by a test.
///
/// # Errors
///
/// [`ValidationError::Overflow`] when `amount` is not finite, or is past
/// the range an `f64` represents exactly.
fn atoms_from_f64(amount: f64) -> Result<i128, ValidationError> {
    if !amount.is_finite() {
        return Err(ValidationError::Overflow {
            context: format!("amount {amount} is not finite"),
        });
    }
    if amount.abs() > MAX_SAFE_F64_INT as f64 {
        return Err(ValidationError::Overflow {
            context: format!("amount {amount} exceeds safe f64 integer range (2^53)"),
        });
    }
    Ok((amount * F64_1E6).floor() as i128)
}

/// A count of atoms the chain returned in a wider word, narrowed to the
/// `u128` the contracts store it in.
fn atoms_from_u256(atoms: U256, asset: &str) -> Result<u128, ValidationError> {
    u128::try_from(atoms).map_err(|_| ValidationError::Overflow {
        context: format!("{asset} {atoms} exceeds uint128"),
    })
}

/// A count of atoms a 256-bit computation produced, narrowed to `i128`.
fn atoms_from_i256(atoms: I256, asset: &str) -> Result<i128, ValidationError> {
    i128::try_from(atoms).map_err(|_| ValidationError::Overflow {
        context: format!("{asset} {atoms} exceeds int128"),
    })
}

impl UsdcAtoms {
    /// The dollars, for a person: lossy, and exact only below 2^53 atoms.
    pub fn usdc(self) -> f64 {
        self.atoms() as f64 / F64_1E6
    }
}

impl UsdcDelta {
    /// The dollars, for a person: lossy, and exact only below 2^53 atoms.
    pub fn usdc(self) -> f64 {
        self.atoms() as f64 / F64_1E6
    }
}

impl PerpAtoms {
    /// The tokens, for a person: lossy, and exact only below 2^53 atoms.
    pub fn perp(self) -> f64 {
        self.atoms() as f64 / F64_1E6
    }

    /// What this many tokens are worth at `price`, in USDC atoms.
    ///
    /// The one crossing between the two assets. Truncated toward zero, as
    /// Solidity's division truncates; a port that needs the contract's
    /// other rounding at a particular step does its own arithmetic.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] when the product does not fit the
    /// width the contracts hold a balance in.
    pub fn value_at(self, price: Price) -> Result<UsdcAtoms, ValidationError> {
        let atoms = mul_div(
            U256::from(self.atoms()),
            price.x96(),
            Q96,
            Rounding::TowardZero,
        )?;
        Ok(UsdcAtoms::new(atoms_from_u256(atoms, "position value")?))
    }
}

impl PerpDelta {
    /// The tokens, for a person: lossy, and exact only below 2^53 atoms.
    pub fn perp(self) -> f64 {
        self.atoms() as f64 / F64_1E6
    }

    /// What this exposure is worth at `price`, keeping its sign.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] when the product does not fit the
    /// width the contracts hold a signed balance in.
    pub fn value_at(self, price: Price) -> Result<UsdcDelta, ValidationError> {
        let atoms = s_full_mul_div(
            I256::try_from(self.atoms()).expect("i128 fits I256"),
            to_i256(price.x96(), "price exceeds the signed range")?,
            Q96,
            Rounding::TowardZero,
        )?;
        Ok(UsdcDelta::new(atoms_from_i256(atoms, "position value")?))
    }
}

// ── Conversions ───────────────────────────────────────────────────────
//
// Every one that can fail does, and says what was out of range: a value
// past these widths is a broken read rather than a quantity.

impl TryFrom<f64> for UsdcAtoms {
    type Error = ValidationError;

    /// Dollars as atoms. Refuses a negative amount: a count cannot be
    /// below zero.
    fn try_from(usdc: f64) -> Result<Self, Self::Error> {
        let atoms = atoms_from_f64(usdc)?;
        u128::try_from(atoms)
            .map(Self::new)
            .map_err(|_| ValidationError::Overflow {
                context: format!("USDC amount {usdc} is negative"),
            })
    }
}

impl TryFrom<f64> for UsdcDelta {
    type Error = ValidationError;

    /// Dollars as a signed count of atoms.
    fn try_from(usdc: f64) -> Result<Self, Self::Error> {
        atoms_from_f64(usdc).map(Self::new)
    }
}

impl TryFrom<f64> for PerpAtoms {
    type Error = ValidationError;

    /// Tokens as atoms. Refuses a negative amount.
    fn try_from(perp: f64) -> Result<Self, Self::Error> {
        let atoms = atoms_from_f64(perp)?;
        u128::try_from(atoms)
            .map(Self::new)
            .map_err(|_| ValidationError::Overflow {
                context: format!("perp amount {perp} is negative"),
            })
    }
}

impl TryFrom<f64> for PerpDelta {
    type Error = ValidationError;

    /// Tokens as a signed exposure.
    fn try_from(perp: f64) -> Result<Self, Self::Error> {
        atoms_from_f64(perp).map(Self::new)
    }
}

impl TryFrom<U256> for UsdcAtoms {
    type Error = ValidationError;

    /// A balance the chain returned as a 256-bit word, narrowed to the
    /// width the contracts store it in.
    fn try_from(atoms: U256) -> Result<Self, Self::Error> {
        atoms_from_u256(atoms, "USDC balance").map(Self::new)
    }
}

impl TryFrom<UsdcAtoms> for UsdcDelta {
    type Error = ValidationError;

    /// A count as a signed count.
    fn try_from(atoms: UsdcAtoms) -> Result<Self, Self::Error> {
        i128::try_from(atoms.atoms())
            .map(Self::new)
            .map_err(|_| ValidationError::Overflow {
                context: format!("USDC {} exceeds int128", atoms.atoms()),
            })
    }
}

impl TryFrom<UsdcDelta> for UsdcAtoms {
    type Error = ValidationError;

    /// A signed count as a count, refusing a negative one.
    fn try_from(delta: UsdcDelta) -> Result<Self, Self::Error> {
        u128::try_from(delta.atoms())
            .map(Self::new)
            .map_err(|_| ValidationError::Overflow {
                context: format!("USDC {} is negative", delta.atoms()),
            })
    }
}

impl TryFrom<PerpAtoms> for PerpDelta {
    type Error = ValidationError;

    /// A count as a signed exposure.
    fn try_from(atoms: PerpAtoms) -> Result<Self, Self::Error> {
        i128::try_from(atoms.atoms())
            .map(Self::new)
            .map_err(|_| ValidationError::Overflow {
                context: format!("perp {} exceeds int128", atoms.atoms()),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scale is the contract's: a dollar is a million atoms, and the
    /// conversion back is the same number.
    #[test]
    fn dollars_and_atoms_are_one_scale_apart() {
        assert_eq!(UsdcAtoms::try_from(1.5).unwrap(), UsdcAtoms::new(1_500_000));
        assert_eq!(UsdcAtoms::new(1_500_000).usdc(), 1.5);
        assert_eq!(
            UsdcDelta::try_from(-2.5).unwrap(),
            UsdcDelta::new(-2_500_000)
        );
        assert_eq!(UsdcDelta::new(-2_000_000).usdc(), -2.0);
    }

    /// A fraction of an atom truncates toward zero, as the chain's own
    /// division does, rather than rounding to the nearer atom.
    #[test]
    fn a_fraction_of_an_atom_truncates() {
        assert_eq!(UsdcAtoms::try_from(1.0000019).unwrap().atoms(), 1_000_001);
    }

    /// A count cannot be negative, and the refusal names the amount.
    #[test]
    fn a_negative_amount_is_not_a_count() {
        let err = UsdcAtoms::try_from(-1.0).unwrap_err();
        assert!(err.to_string().contains("negative"), "{err}");
        assert!(PerpAtoms::try_from(-1.0).is_err());
        // The signed twins take it.
        assert!(UsdcDelta::try_from(-1.0).is_ok());
        assert!(PerpDelta::try_from(-1.0).is_ok());
    }

    /// An amount that cannot survive the trip through `f64` is refused
    /// rather than rounded.
    #[test]
    fn an_amount_past_the_f64_range_is_refused() {
        for amount in [f64::NAN, f64::INFINITY, 1e30] {
            assert!(UsdcDelta::try_from(amount).is_err(), "{amount}");
        }
    }

    /// The valuation is the one crossing between the assets, and it is the
    /// product of the count and the price.
    #[test]
    fn tokens_are_valued_at_a_price() {
        let price = Price::try_from(2.0).unwrap();
        // Two tokens at two dollars is four dollars.
        assert_eq!(
            PerpAtoms::new(2_000_000).value_at(price).unwrap(),
            UsdcAtoms::new(4_000_000)
        );
        // A short's value keeps its sign.
        assert_eq!(
            PerpDelta::new(-2_000_000).value_at(price).unwrap(),
            UsdcDelta::new(-4_000_000)
        );
        assert_eq!(PerpDelta::ZERO.value_at(price).unwrap(), UsdcDelta::ZERO);
    }

    /// The signed twin adds, negates and sums; the count does neither
    /// silently, and says so by returning an option.
    #[test]
    fn deltas_add_and_counts_are_checked() {
        let (a, b) = (UsdcDelta::new(7), UsdcDelta::new(-10));
        assert_eq!(a + b, UsdcDelta::new(-3));
        assert_eq!(-b, UsdcDelta::new(10));
        assert_eq!([a, b].into_iter().sum::<UsdcDelta>(), UsdcDelta::new(-3));
        assert_eq!(b.magnitude(), UsdcAtoms::new(10));

        let (big, small) = (UsdcAtoms::new(10), UsdcAtoms::new(7));
        assert_eq!(big.checked_sub(small), Some(UsdcAtoms::new(3)));
        assert_eq!(small.checked_sub(big), None);
        assert_eq!(small.saturating_sub(big), UsdcAtoms::ZERO);
        assert_eq!(UsdcAtoms::new(u128::MAX).checked_add(small), None);
    }

    /// The wire form is the bare number, so a value that was persisted or
    /// logged as a primitive still reads back.
    #[test]
    fn the_wire_form_is_the_number_alone() {
        assert_eq!(
            serde_json::to_string(&UsdcAtoms::new(1_500_000)).unwrap(),
            "1500000"
        );
        assert_eq!(
            serde_json::from_str::<PerpDelta>("-42").unwrap(),
            PerpDelta::new(-42)
        );
    }
}
