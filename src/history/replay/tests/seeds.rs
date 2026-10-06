//! The seeded start: a seed is a checkpoint, a seeded margin is known
//! until touched, and an event at or before the fold's point is refused.

use super::*;

/// A fold seeded from the reads at a block and continued over the tape
/// after it answers every read-shaped question as the fold from genesis
/// does: a seed is a checkpoint.
#[test]
fn a_seed_is_a_checkpoint() {
    let tape = whole_market();
    // After block 22: a taker sized, a maker banded, the tick known.
    let cut = tape.iter().position(|row| row.block_number > 22).unwrap();
    let prefix = genesis(&tape[..cut]);
    let at = block_of(&tape[cut - 1]);
    let seed = seed_of(&prefix, at);
    assert_eq!(seed.positions.len(), 2);

    let mut seeded = Replay::from_seed(seed).unwrap();
    assert_eq!(seeded.block(), Some(at));
    assert_eq!(
        seeded.gaps().unknowns.margin_unknown,
        0,
        "a seed knows every margin"
    );
    assert_eq!(
        seeded.gaps().unknowns.partial_positions,
        0,
        "a seed knows every level"
    );
    for row in &tape[cut..] {
        seeded.apply(row);
    }
    let whole = genesis(&tape);
    let last = block_of(tape.last().unwrap());

    assert_eq!(seeded.applied(), whole.applied() - cut as u64);
    assert_eq!(seeded.block(), whole.block());
    assert_eq!(seeded.capacity_at(last), whole.capacity_at(last));
    assert_eq!(
        seeded.mark_at(last, 600).unwrap(),
        whole.mark_at(last, 600).unwrap()
    );
    assert_eq!(seeded.funding_per_day(), whole.funding_per_day());
    assert_eq!(seeded.cumulatives(), whole.cumulatives());
    assert_eq!(seeded.solvency(), whole.solvency());
    assert_eq!(
        seeded.module(ModuleKind::Pricing),
        whole.module(ModuleKind::Pricing)
    );
    assert_eq!(seeded.pool_ticks(), whole.pool_ticks());
    assert_eq!(seeded.pool_tick(), whole.pool_tick());
    assert_eq!(seeded.pool_liquidity(), whole.pool_liquidity());
    for (pos_id, expected) in whole.positions().iter() {
        let actual = seeded.position(pos_id).unwrap();
        assert_eq!(actual.is_open(), expected.is_open(), "{pos_id} open");
        assert_eq!(actual.taker_size(), expected.taker_size(), "{pos_id} size");
        assert_eq!(actual.taker_usd(), expected.taker_usd(), "{pos_id} USD leg");
        assert_eq!(actual.maker_band(), expected.maker_band(), "{pos_id} band");
        assert_eq!(actual.closed(), expected.closed(), "{pos_id} close");
    }
    assert_eq!(seeded.gaps(), whole.gaps());
}

/// A seeded position's margin is the read's until an event touches the
/// position; then it is unknown, since no event carries it.
#[test]
fn a_seeded_margin_is_known_until_an_event_touches_the_position() {
    let tape = whole_market();
    let cut = tape.iter().position(|row| row.block_number > 22).unwrap();
    let prefix = genesis(&tape[..cut]);
    let mut seeded = Replay::from_seed(seed_of(&prefix, block_of(&tape[cut - 1]))).unwrap();

    let taker = seeded.position(U256::from(1)).unwrap();
    assert_eq!(taker.margin(), Some(UsdcAtoms::new(1_000_001)));
    assert_eq!(taker.last(), None, "no event has touched it since the read");
    assert_eq!(seeded.gaps().unknowns.margin_unknown, 0);

    // The maker trims its band: its margin moved, the taker's did not.
    seeded.apply(&tape[cut]);
    seeded.apply(&tape[cut + 1]);
    assert_eq!(
        seeded.position(U256::from(2)).unwrap().margin(),
        None,
        "touched"
    );
    assert_eq!(
        seeded.position(U256::from(1)).unwrap().margin(),
        Some(UsdcAtoms::new(1_000_001))
    );
    assert_eq!(seeded.gaps().unknowns.margin_unknown, 1);
}

/// An event at or before the fold's point is refused and counted, never
/// applied: a duplicate, a driver out of order, or a seed's own block
/// delivered again.
#[test]
fn an_event_at_or_before_the_folds_point_is_refused_and_counted() {
    let tape = lifecycle();
    let mut market = genesis(&tape[..3]);
    let before = market.clone();

    market.apply(&tape[2]);
    market.apply(&tape[0]);
    assert_eq!(market.gaps().faults.refused, 2);
    assert_eq!(market.applied(), before.applied());
    assert_eq!(market.point(), before.point());
    assert_eq!(market.positions(), before.positions());
    assert_eq!(market.pool_ticks(), before.pool_ticks());

    market.apply(&tape[3]);
    assert_eq!(market.applied(), before.applied() + 1);

    // A seed stands at the end of its block.
    let mut seeded =
        Replay::from_seed(seed_of(&genesis(&whole_market()[..16]), block_of(&tape[2]))).unwrap();
    seeded.apply(&tape[2]);
    assert_eq!(
        seeded.gaps().faults.refused,
        1,
        "the seed's block, delivered again"
    );
    seeded.apply(&tape[3]);
    assert_eq!(seeded.gaps().faults.refused, 1);
    assert_eq!(seeded.applied(), 1);
}
