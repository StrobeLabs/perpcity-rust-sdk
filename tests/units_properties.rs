//! The properties the units' exactness claim rests on, checked across
//! their domains rather than at hand-picked points: a split returns every
//! atom it was given, the two asset crossings invert each other to within
//! the truncation they document, a price and its root round-trip to within
//! the floor the root documents, and a human amount survives the trip
//! through `f64` to the atom.

use std::num::NonZeroUsize;

use alloy::primitives::U256;
use perpcity_sdk::constants::Q96;
use perpcity_sdk::{PerpAtoms, PerpDelta, Price, Share, SqrtPrice, UsdcAtoms, UsdcDelta};
use proptest::prelude::*;

/// A human amount with at most six decimal places, in the range an `f64`
/// can hold six decimals at all: positive, under 2^32 dollars. Above that
/// a unit in the last place of the `f64` is itself more than half an atom,
/// so no door could read the sixth decimal back — the first run of this
/// property found the bound at four billion.
fn human_amount() -> impl Strategy<Value = f64> {
    (1u64..4_000_000_000_000_000u64).prop_map(|atoms| atoms as f64 / 1e6)
}

/// A price a market could trade at, from a tenth of a cent to a million.
fn market_price() -> impl Strategy<Value = Price> {
    (1_000u64..1_000_000_000_000u64).prop_map(|micro| Price::try_from(micro as f64 / 1e6).unwrap())
}

proptest! {
    /// Every piece of a split is accounted for: the pieces sum to the
    /// amount, whatever the amount and however many pieces.
    #[test]
    fn a_split_returns_every_atom(x: u128, parts in 1usize..64) {
        let parts = NonZeroUsize::new(parts).unwrap();
        let pieces = UsdcAtoms::new(x).split(parts);
        prop_assert_eq!(pieces.len(), parts.get());
        prop_assert_eq!(pieces.iter().map(|p| p.atoms()).sum::<u128>(), x);
    }

    /// A signed split keeps the sign on every piece and still sums back.
    #[test]
    fn a_signed_split_returns_every_atom(x: i128, parts in 1usize..64) {
        let parts = NonZeroUsize::new(parts).unwrap();
        let pieces = PerpDelta::new(x).split(parts);
        prop_assert!(pieces.iter().all(|p| p.is_zero() || p.is_negative() == (x < 0)));
        prop_assert_eq!(pieces.iter().map(|p| p.atoms()).sum::<i128>(), x);
    }

    /// Whatever the weights, a partition is exactly the whole, and a split
    /// by it is exactly the amount.
    #[test]
    fn a_partition_is_the_whole_and_splits_exactly(
        weights in prop::collection::vec(0.001f64..1_000.0, 1..16),
        x: u128,
    ) {
        let shares = Share::partition(&weights).unwrap();
        prop_assert_eq!(shares.iter().map(|s| s.e6()).sum::<u32>(), 1_000_000);
        let pieces = UsdcAtoms::new(x).split_weighted(&shares).unwrap();
        prop_assert_eq!(pieces.iter().map(|p| p.atoms()).sum::<u128>(), x);
    }

    /// Valuing tokens and sizing them back never gains an atom: both
    /// crossings truncate, so the round trip is at most the original.
    #[test]
    fn value_at_and_perp_at_invert_to_within_truncation(
        perp in 0u128..1_000_000_000_000_000u128,
        price in market_price(),
    ) {
        let perp = PerpAtoms::new(perp);
        let usd = perp.value_at(price).unwrap();
        let back = usd.perp_at(price).unwrap();
        prop_assert!(back <= perp, "{back:?} > {perp:?}");
        let usd_back = back.value_at(price).unwrap();
        prop_assert!(usd_back <= usd, "{usd_back:?} > {usd:?}");
        // And the trip USDC → tokens → USDC also never gains.
        let sized = usd.perp_at(price).unwrap().value_at(price).unwrap();
        prop_assert!(sized <= usd);
    }

    /// A fill's implied price, applied back to its token leg, returns at
    /// most the USDC leg: `per` floors.
    #[test]
    fn an_implied_price_values_the_leg_it_came_from(
        usd in 1u128..1_000_000_000_000_000u128,
        perp in 1u128..1_000_000_000_000_000u128,
    ) {
        let (usd, perp) = (UsdcAtoms::new(usd), PerpAtoms::new(perp));
        let implied = usd.per(perp).unwrap();
        prop_assert!(perp.value_at(implied).unwrap() <= usd);
    }

    /// A price's root squares back to at most the price, and the loss is
    /// the floor the root documents: under `(2r + 1) / 2^96` plus one unit.
    #[test]
    fn a_price_and_its_root_round_trip_within_the_floor(price in market_price()) {
        let root = SqrtPrice::try_from(price).unwrap();
        let back = root.squared().unwrap();
        prop_assert!(back <= price);
        let lost = price.x96() - back.x96();
        let bound = (U256::from(2u8) * root.x96() + U256::ONE) / Q96 + U256::ONE;
        prop_assert!(lost <= bound, "lost {lost} > bound {bound}");
    }

    /// A human price reads back to within a millionth, and never above
    /// what was written: `try_from` keeps 48 fractional bits and `to_f64`
    /// floors to six decimals.
    #[test]
    fn a_human_price_reads_back_to_the_micro(human in 0.001f64..1_000_000.0) {
        let back = Price::try_from(human).unwrap().to_f64().unwrap();
        prop_assert!(back <= human + 1e-9, "{back} > {human}");
        prop_assert!(human - back <= 1e-6 + 1e-9, "{human} came back as {back}");
    }

    /// A decimal of six places or fewer becomes the atom count it names,
    /// and reads back as itself: the human door is exact on its domain.
    #[test]
    fn a_human_amount_is_the_atom_count_it_names(human in human_amount()) {
        let atoms = UsdcAtoms::try_from(human).unwrap();
        prop_assert_eq!(atoms.atoms() as f64, (human * 1e6).round());
        prop_assert_eq!(atoms.usdc(), human);
        let signed = UsdcDelta::try_from(-human).unwrap();
        prop_assert_eq!(signed.atoms(), -(atoms.atoms() as i128));
    }

    /// Scaling by a share never exceeds the amount, and the complement
    /// takes exactly what the share left.
    #[test]
    fn a_share_and_its_complement_partition_an_amount(
        x in 0u128..u128::MAX / 2,
        e6 in 0u32..=1_000_000,
    ) {
        let share = Share::from_e6(e6).unwrap();
        let (taken, left) = (UsdcAtoms::new(x) * share, UsdcAtoms::new(x) * share.complement());
        prop_assert!(taken.atoms() <= x);
        // Two truncations can lose at most one atom between them.
        prop_assert!(x - (taken.atoms() + left.atoms()) <= 1);
    }
}
