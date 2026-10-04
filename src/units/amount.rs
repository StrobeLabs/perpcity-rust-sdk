//! The two assets a market settles in, each a count of atoms.
//!
//! Both are six-decimal on chain, so both are `u128` counts of the same
//! width, and before these types only a field's name said which asset a
//! number was. The pair that matters is [`PerpAtoms`] and [`UsdcAtoms`]:
//! they are indistinguishable as primitives, and the one operation that
//! crosses between them is a valuation at a [`Price`].

use std::fmt;

use alloy::primitives::{I256, U256};

use super::fixed_point::{Rounding, mul_div, s_full_mul_div, to_i256};
use crate::constants::Q96;
use crate::errors::ValidationError;

use super::price::Price;
use super::{F64_1E6, MAX_SAFE_F64_INT, bounded_count, count, delta};

/// The atoms in one unit of either asset: both are six-decimal.
const ATOMS_PER_UNIT: u128 = 1_000_000;
/// The decimal places an atom is the last of.
const PLACES: usize = 6;

bounded_count! {
    /// USDC the chain holds: a count of atoms, each a millionth of a
    /// dollar and the smallest amount a market can settle.
    ///
    /// The `f64` dollars a person reads come from [`Self::usdc`] and are
    /// the derived view, exact below 2^53 atoms (about nine billion
    /// dollars) and approximate above it.
    UsdcAtoms(u128) as atoms
}

bounded_count! {
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
    /// Which way positive points is the *field's*, named by the field and
    /// stated on it — a settle's `funding_owed` is positive when the
    /// position pays and is subtracted, while its earnings are positive
    /// when the position receives and are added. The type promises only
    /// that any sum of these stays far inside `i128`, which the protocol's
    /// supply bound guarantees; it does not promise they all point the
    /// same way, so read the field before adding it.
    UsdcDelta(i128) as atoms, magnitude UsdcAtoms
}

delta! {
    /// A signed exposure in the market's token: positive long, negative
    /// short, as a position's `delta` stores it.
    PerpDelta(i128) as atoms, magnitude PerpAtoms
}

/// A human amount as a signed count of atoms, rounded to the nearest atom.
///
/// Rounded, not floored: the amount is a number a person wrote or a
/// strategy computed, and most decimals have no binary representation, so
/// `0.29` arrives as `0.28999999999999998` and a floor would make it an
/// atom short. Rounding reads every decimal of six places or fewer to the
/// atom it names, up to 2^32 units (about four billion dollars), past
/// which a unit in the `f64`'s last place is more than half an atom and
/// the sixth decimal is not in the number to read; a seventh place rounds,
/// since there is nothing smaller to hold it. This is a choice about a
/// caller's own input, not a transcription of the contract, which
/// truncates in its own division — nothing here feeds a port.
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
    Ok((amount * F64_1E6).round() as i128)
}

/// A count of atoms as the decimal a person reads, exact: the sign, the
/// whole units, and the fraction with its trailing zeros trimmed, so
/// `5_800_000` is `5.8` and `100_000_000` is `100`. A precision asked of
/// the formatter truncates the fraction to that many places.
fn fmt_atoms(f: &mut fmt::Formatter<'_>, negative: bool, magnitude: u128) -> fmt::Result {
    let whole = magnitude / ATOMS_PER_UNIT;
    let fraction = format!("{:0PLACES$}", magnitude % ATOMS_PER_UNIT);
    let fraction = match f.precision() {
        Some(places) => &fraction[..places.min(PLACES)],
        None => fraction.trim_end_matches('0'),
    };
    let text = if fraction.is_empty() {
        whole.to_string()
    } else {
        format!("{whole}.{fraction}")
    };
    // Padded as an integer is, so the width and fill apply and the
    // precision, already spent on the fraction, does not shorten the text.
    f.pad_integral(!negative, "", &text)
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

    /// How many tokens this much USDC buys at `price`: the inverse of
    /// [`PerpAtoms::value_at`], truncated toward zero.
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidPrice`] when `price` is zero, and
    /// [`ValidationError::Overflow`] when the quotient does not fit the
    /// width the contracts hold a balance in.
    pub fn perp_at(self, price: Price) -> Result<PerpAtoms, ValidationError> {
        if price.is_zero() {
            return Err(ValidationError::InvalidPrice {
                reason: "cannot size tokens at a zero price".into(),
            });
        }
        let atoms = mul_div(
            U256::from(self.atoms()),
            Q96,
            price.x96(),
            Rounding::TowardZero,
        )?;
        Ok(PerpAtoms::new(atoms_from_u256(atoms, "tokens at price")?))
    }

    /// The price this much USDC implies for `size` tokens: USDC per token,
    /// what a fill's two legs say it traded at, exact in Q96 and floored.
    /// The third corner of the crossing, after [`PerpAtoms::value_at`] and
    /// [`Self::perp_at`].
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidPrice`] when `size` is zero: nothing was
    /// traded, so no price was.
    pub fn per(self, size: PerpAtoms) -> Result<Price, ValidationError> {
        if size.is_zero() {
            return Err(ValidationError::InvalidPrice {
                reason: "no tokens to price USDC against".into(),
            });
        }
        let x96 = mul_div(
            U256::from(self.atoms()),
            Q96,
            U256::from(size.atoms()),
            Rounding::TowardZero,
        )?;
        Ok(Price::from_x96(x96))
    }
}

impl UsdcDelta {
    /// The dollars, for a person: lossy, and exact only below 2^53 atoms.
    pub fn usdc(self) -> f64 {
        self.atoms() as f64 / F64_1E6
    }

    /// The exposure this much USDC buys at `price`, keeping its sign: the
    /// inverse of [`PerpDelta::value_at`].
    ///
    /// # Errors
    ///
    /// As [`UsdcAtoms::perp_at`].
    pub fn perp_at(self, price: Price) -> Result<PerpDelta, ValidationError> {
        let magnitude = PerpDelta::from(self.magnitude().perp_at(price)?);
        Ok(if self.is_negative() {
            -magnitude
        } else {
            magnitude
        })
    }

    /// The price this USDC leg implies for the `size` leg it was traded
    /// against, by magnitudes: a position's entry price from its cost basis
    /// and its exposure, whichever way each points.
    ///
    /// # Errors
    ///
    /// As [`UsdcAtoms::per`].
    pub fn per(self, size: PerpDelta) -> Result<Price, ValidationError> {
        self.magnitude().per(size.magnitude())
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

// ── The human view ────────────────────────────────────────────────────
//
// What a person reads in a log or a report: the exact decimal, `5.8`,
// never the atom count and never a float's trailing noise.

impl fmt::Display for UsdcAtoms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_atoms(f, false, self.atoms())
    }
}

impl fmt::Display for UsdcDelta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_atoms(f, self.is_negative(), self.atoms().unsigned_abs())
    }
}

impl fmt::Display for PerpAtoms {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_atoms(f, false, self.atoms())
    }
}

impl fmt::Display for PerpDelta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt_atoms(f, self.is_negative(), self.atoms().unsigned_abs())
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

/// A count as a signed count. Infallible for the reason the delta's
/// operators are: the supply bound keeps any count the chain produces far
/// inside `i128`, so a count past it is outside the type's domain already.
impl From<UsdcAtoms> for UsdcDelta {
    fn from(atoms: UsdcAtoms) -> Self {
        debug_assert!(
            i128::try_from(atoms.atoms()).is_ok(),
            "USDC count past int128"
        );
        Self::new(atoms.atoms() as i128)
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

/// A count as a long exposure, for the reason the USDC widening is
/// infallible.
impl From<PerpAtoms> for PerpDelta {
    fn from(atoms: PerpAtoms) -> Self {
        debug_assert!(
            i128::try_from(atoms.atoms()).is_ok(),
            "perp count past int128"
        );
        Self::new(atoms.atoms() as i128)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::Share;

    /// The float door reads every decimal of six places or fewer to the
    /// atom it names: most hundredths have no binary representation, and a
    /// floor would have made some of them an atom short — the order a
    /// strategy did not mean to send.
    #[test]
    fn a_human_amount_reads_to_the_atom_it_names() {
        for cents in 1..100i128 {
            let exact = PerpDelta::new(cents * 10_000);
            let amount = cents as f64 / 100.0;
            assert_eq!(PerpDelta::try_from(amount).unwrap(), exact, "{amount}");
            assert_eq!(PerpDelta::try_from(-amount).unwrap(), -exact, "-{amount}");
        }
        assert_eq!(PerpDelta::try_from(5.8).unwrap(), PerpDelta::new(5_800_000));
        assert_eq!(UsdcAtoms::try_from(0.000001).unwrap(), UsdcAtoms::new(1));
        assert_eq!(
            UsdcDelta::try_from(-1.000001).unwrap(),
            UsdcDelta::new(-1_000_001)
        );
        // A seventh place has nothing to hold it and rounds to the nearer
        // atom, away from zero on the half.
        assert_eq!(UsdcAtoms::try_from(1.0000019).unwrap().atoms(), 1_000_002);
        assert_eq!(UsdcAtoms::try_from(1.0000011).unwrap().atoms(), 1_000_001);
        assert_eq!(UsdcDelta::try_from(-1.1234567).unwrap().atoms(), -1_123_457);
    }

    /// An amount displays as the exact decimal, trailing zeros trimmed,
    /// and never as a float's noise; a precision truncates; width pads.
    #[test]
    fn an_amount_displays_as_the_decimal_a_person_reads() {
        assert_eq!(PerpDelta::new(5_800_000).to_string(), "5.8");
        assert_eq!(PerpDelta::new(100_000_000).to_string(), "100");
        assert_eq!(PerpDelta::new(290_000).to_string(), "0.29");
        assert_eq!(PerpDelta::new(-1_000_001).to_string(), "-1.000001");
        assert_eq!(PerpDelta::ZERO.to_string(), "0");
        assert_eq!(UsdcAtoms::new(1).to_string(), "0.000001");
        assert_eq!(format!("{:.2}", UsdcAtoms::new(5_800_000)), "5.80");
        assert_eq!(format!("{:.0}", UsdcDelta::new(-5_800_000)), "-5");
        assert_eq!(format!("{:>8}", PerpAtoms::new(5_800_000)), "     5.8");
    }

    /// The third corner of the crossing: a fill's USDC leg over its token
    /// leg is the price it traded at, exact where the legs allow and
    /// floored where they do not.
    #[test]
    fn a_fill_implies_its_price() {
        let usd = UsdcAtoms::try_from(580.0).unwrap();
        let size = PerpAtoms::try_from(5.8).unwrap();
        assert_eq!(usd.per(size).unwrap(), Price::try_from(100.0).unwrap());
        // The inverse of the valuation on the same legs.
        assert_eq!(size.value_at(usd.per(size).unwrap()).unwrap(), usd);
        // By magnitudes on the signed legs: a short's cost basis is as
        // negative as its exposure, and the price is the same.
        let paid = UsdcDelta::try_from(-580.0).unwrap();
        let held = PerpDelta::try_from(-5.8).unwrap();
        assert_eq!(paid.per(held).unwrap(), Price::try_from(100.0).unwrap());
        // One third floors.
        let third = UsdcAtoms::new(1).per(PerpAtoms::new(3)).unwrap();
        assert_eq!(third.x96(), Q96 / U256::from(3));
        assert!(matches!(
            usd.per(PerpAtoms::ZERO),
            Err(ValidationError::InvalidPrice { .. })
        ));
    }

    /// The two assets' counts add with an operator and sum from an
    /// iterator, because the protocol bounds what a sum of balances can
    /// reach. Subtracting stays checked: a negative count is not a count,
    /// and that asymmetry is the whole reason `+` is safe here.
    #[test]
    fn a_bounded_count_adds_but_does_not_subtract() {
        let (a, b) = (UsdcAtoms::new(1_500_000), UsdcAtoms::new(250_000));
        assert_eq!(a + b, UsdcAtoms::new(1_750_000));

        let mut running = UsdcAtoms::ZERO;
        running += a;
        running += b;
        assert_eq!(running, UsdcAtoms::new(1_750_000));

        // The four fee legs of a swap are the motivating fold.
        let legs = [a, b, UsdcAtoms::new(1), UsdcAtoms::ZERO];
        assert_eq!(
            legs.into_iter().sum::<UsdcAtoms>(),
            UsdcAtoms::new(1_750_001)
        );

        assert_eq!(a.checked_sub(b), Some(UsdcAtoms::new(1_250_000)));
        assert_eq!(b.checked_sub(a), None, "a count cannot go below zero");
        assert_eq!(b.saturating_sub(a), UsdcAtoms::ZERO);
        // `+` is infallible because the supply bound says so, not because
        // overflow is impossible in the width; the checked door stays for a
        // caller holding a figure the chain did not produce.
        assert_eq!(UsdcAtoms::new(u128::MAX).checked_add(b), None);

        // The other asset is the same shape and still does not mix with it.
        assert_eq!(PerpAtoms::new(7) + PerpAtoms::new(3), PerpAtoms::new(10));
    }

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

    /// A factor a count cannot take is a bug and says so.
    #[test]
    #[should_panic(expected = "negative factor")]
    fn a_count_refuses_a_negative_factor() {
        let _ = UsdcAtoms::new(1) * -0.5;
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
    /// product of the count and the price; sizing is its inverse.
    #[test]
    fn tokens_are_valued_at_a_price() {
        let price = Price::try_from(2.0).unwrap();
        // Two tokens at two dollars is four dollars, and four dollars at
        // two dollars is two tokens.
        assert_eq!(
            PerpAtoms::new(2_000_000).value_at(price).unwrap(),
            UsdcAtoms::new(4_000_000)
        );
        assert_eq!(
            UsdcAtoms::new(4_000_000).perp_at(price).unwrap(),
            PerpAtoms::new(2_000_000)
        );
        assert_eq!(
            UsdcDelta::new(-4_000_000).perp_at(price).unwrap(),
            PerpDelta::new(-2_000_000)
        );
        assert_eq!(
            UsdcAtoms::new(3).perp_at(price).unwrap(),
            PerpAtoms::new(1),
            "truncated"
        );
        assert!(
            UsdcAtoms::new(1)
                .perp_at(Price::from_x96(U256::ZERO))
                .is_err()
        );
        // A short's value keeps its sign.
        assert_eq!(
            PerpDelta::new(-2_000_000).value_at(price).unwrap(),
            UsdcDelta::new(-4_000_000)
        );
        assert_eq!(PerpDelta::ZERO.value_at(price).unwrap(), UsdcDelta::ZERO);
    }

    /// The signed twin adds, subtracts, negates and sums, and drops its
    /// sign through `magnitude`.
    #[test]
    fn a_delta_adds_negates_and_sums() {
        let (a, b) = (UsdcDelta::new(7), UsdcDelta::new(-10));
        assert_eq!(a + b, UsdcDelta::new(-3));
        assert_eq!(-b, UsdcDelta::new(10));
        assert_eq!([a, b].into_iter().sum::<UsdcDelta>(), UsdcDelta::new(-3));
        assert_eq!(b.magnitude(), UsdcAtoms::new(10));
    }

    /// A count widens to its delta without a door to fail at, and narrows
    /// back only when it is not negative.
    #[test]
    fn a_count_widens_freely_and_narrows_checked() {
        assert_eq!(UsdcDelta::from(UsdcAtoms::new(5)), UsdcDelta::new(5));
        assert_eq!(PerpDelta::from(PerpAtoms::new(5)), PerpDelta::new(5));
        assert_eq!(
            UsdcAtoms::try_from(UsdcDelta::new(5)).unwrap(),
            UsdcAtoms::new(5)
        );
        assert!(UsdcAtoms::try_from(UsdcDelta::new(-5)).is_err());
    }

    /// An amount multiplies by a share or by a plain factor, once and in
    /// one direction: truncated toward zero on either sign. A split returns
    /// every atom.
    #[test]
    fn an_amount_scales_and_splits_by_share() {
        let third = Share::from_e6(333_333).unwrap();
        assert_eq!(UsdcAtoms::new(10) * third, UsdcAtoms::new(3));
        assert_eq!(third * UsdcAtoms::new(10), UsdcAtoms::new(3));
        assert_eq!(UsdcDelta::new(-10) * third, UsdcDelta::new(-3));
        // The float path: a strategy's literal, converted once and exact
        // from there. A leverage is a factor past one; a negative factor on
        // a delta flips its side.
        assert_eq!(
            UsdcAtoms::new(100_000_000) * 5.0,
            UsdcAtoms::new(500_000_000)
        );
        assert_eq!(5.0 * PerpDelta::new(-3), PerpDelta::new(-15));
        assert_eq!(0.5 * UsdcAtoms::new(7), UsdcAtoms::new(3));
        assert_eq!(PerpDelta::new(7) * -0.5, PerpDelta::new(-3));
        assert_eq!(
            UsdcAtoms::new(3) * 0.1,
            UsdcAtoms::ZERO,
            "truncated, not rounded"
        );
        assert_eq!(
            UsdcAtoms::new(1).share_of(UsdcAtoms::new(4)),
            Some(Share::try_from(0.25).unwrap())
        );

        let three = std::num::NonZeroUsize::new(3).unwrap();
        assert_eq!(
            PerpDelta::new(-10).split(three),
            [PerpDelta::new(-4), PerpDelta::new(-3), PerpDelta::new(-3)]
        );
        let weights = Share::partition(&[3.0, 1.0]).unwrap();
        let pieces = UsdcAtoms::new(1_000_001).split_weighted(&weights).unwrap();
        assert_eq!(
            pieces.iter().copied().sum::<UsdcAtoms>(),
            UsdcAtoms::new(1_000_001)
        );
        assert_eq!(pieces, [UsdcAtoms::new(750_001), UsdcAtoms::new(250_000)]);
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
