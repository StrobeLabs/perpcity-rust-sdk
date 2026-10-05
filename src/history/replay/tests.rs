//! Tests for the replay over fixture tapes: equality with the reads'
//! shapes, the combine law at every cut, and what the fold does not know.

use alloy::primitives::{B256, I256, U256};

use super::super::fold::{Change, Window};
use super::seed::{Seed, SeedPosition};
use super::*;
use crate::client::{MarketRates, PositionRole};
use crate::constants::Q96;
use crate::contracts::Modules as ContractModules;
use crate::events::{MakerSettle, MarketEvent, SwapInfo};
use crate::math::pricing::calculate_emas;
use crate::math::range::{MakerBand, TickRange};
use crate::units::{
    Earnings, Funding, FundingPerSqrtPrice, LDelta, PerpAtoms, PerpDelta, UsdcAtoms, UsdcDelta,
};

/// A row in `block`; the fixtures put one transaction in each block, so
/// the transaction is the block's.
fn row(block: u64, log_index: u64, event: MarketEvent) -> TapeEvent {
    TapeEvent {
        block_number: block,
        block_hash: B256::with_last_byte(block as u8),
        log_index,
        timestamp: 1_700_000_000 + block * 10,
        tx_hash: B256::repeat_byte(block as u8),
        event,
    }
}

/// The fixture tape folded as a market from its genesis.
fn genesis(tape: &[TapeEvent]) -> Replay {
    let mut market = Replay::from_genesis(Address::ZERO);
    for row in tape {
        market.apply(row);
    }
    market
}

fn block_of(row: &TapeEvent) -> BlockContext {
    BlockContext {
        number: row.block_number,
        hash: row.block_hash,
        timestamp: row.timestamp,
    }
}

fn price(units: u64) -> Price {
    Price::from_x96(Q96 * U256::from(units))
}

fn swap(pool_price: Price, insurance_fee: u128) -> SwapInfo {
    SwapInfo {
        perp_delta: PerpDelta::new(1_000_000),
        usd_delta: UsdcDelta::new(-40_000_000),
        pool_price,
        total_fee: UsdcDelta::new(100_000),
        lp_fee: UsdcAtoms::new(70_000),
        protocol_fee: UsdcAtoms::new(10_000),
        creator_fee: UsdcAtoms::new(10_000),
        insurance_fee: UsdcAtoms::new(insurance_fee),
    }
}

fn per_side(long: u128, short: u128) -> PerSide<PerpAtoms> {
    PerSide::new(PerpAtoms::new(long), PerpAtoms::new(short))
}

/// A market's first stretch: a touch, a print, capacity, a maker's
/// mint, a swap, the books.
fn tape() -> Vec<TapeEvent> {
    vec![
        row(
            10,
            0,
            MarketEvent::RatesAndEmasRefreshed {
                funding_per_day: FundingRate::from_wad(1_000_000_000_000_000),
                util_fee_per_day: PerSide::new(
                    UtilizationRate::from_wad(100),
                    UtilizationRate::from_wad(200),
                ),
                last_touch: 1_700_000_100,
                pool_price_ema: price(40),
                index_ema: price(41),
            },
        ),
        row(10, 1, MarketEvent::IndexUpdated { index: price(42) }),
        row(
            11,
            0,
            MarketEvent::CapacityUpdated {
                capacity: per_side(5_000_000, 6_000_000),
            },
        ),
        row(
            11,
            1,
            MarketEvent::PositionTransferred {
                from: Address::ZERO,
                to: Address::repeat_byte(0x0A),
                pos_id: U256::from(1),
            },
        ),
        row(
            12,
            0,
            MarketEvent::TakerOpened {
                pos_id: U256::from(2),
                swap: swap(price(43), 10_000),
            },
        ),
        row(
            12,
            1,
            MarketEvent::OpenInterestUpdated {
                open_interest: per_side(1_000_000, 0),
            },
        ),
        row(
            12,
            2,
            MarketEvent::MarginTransferred {
                margin_delta: UsdcDelta::new(100_000_000),
                total_margin: UsdcAtoms::new(100_000_000),
            },
        ),
        row(
            13,
            0,
            MarketEvent::BadDebtAccounted {
                bad_debt: UsdcAtoms::new(2_000_000),
                insurance_after: UsdcAtoms::ZERO,
                bad_debt_after: UsdcAtoms::new(2_000_000),
            },
        ),
        row(
            13,
            1,
            MarketEvent::MarginTransferred {
                margin_delta: UsdcDelta::new(-10_000_000),
                total_margin: UsdcAtoms::new(90_000_000),
            },
        ),
        row(
            14,
            0,
            MarketEvent::ModuleSet {
                module: ModuleKind::Pricing,
                address: Address::repeat_byte(0x4D),
            },
        ),
    ]
}

#[test]
fn the_totals_are_the_last_stated_and_the_snapshots_carry_the_callers_block() {
    let tape = tape();
    let market = genesis(&tape);
    let last = block_of(tape.last().unwrap());

    assert_eq!(market.applied(), 10);
    assert_eq!(market.block(), Some(last));
    assert_eq!(market.pool_price().value(), Some(price(43)));
    assert_eq!(market.index().value(), Some(price(42)));
    assert_eq!(
        market.capacity_at(last),
        Some(MarketCapacity {
            block: last,
            capacity: per_side(5_000_000, 6_000_000),
            open_interest: per_side(1_000_000, 0),
        })
    );
    assert_eq!(
        market.solvency(),
        Some(SolvencyState {
            bad_debt: UsdcAtoms::new(2_000_000),
            total_margin: UsdcAtoms::new(90_000_000),
        })
    );
    assert_eq!(
        market.module(ModuleKind::Pricing),
        Some(Address::repeat_byte(0x4D))
    );
    assert_eq!(market.module(ModuleKind::Beacon), None);
    assert_eq!(
        market.custody().latest_owner(U256::from(1)),
        Some(Address::repeat_byte(0x0A))
    );
    assert_eq!(
        market.gaps(),
        Gaps {
            unknowns: Unknowns {
                margin_unknown: 1,
                ..Unknowns::default()
            },
            ..Gaps::default()
        },
        "every total was restated; one taker stands, with the margin no event carries"
    );
}

#[test]
fn the_mark_is_the_emas_advanced_to_the_callers_block() {
    let tape = tape();
    let market = genesis(&tape);
    let at = BlockContext {
        number: 20,
        hash: B256::with_last_byte(20),
        timestamp: 1_700_003_700,
    };
    let ema_window = 3_600;
    let mark = market.mark_at(at, ema_window).unwrap().unwrap();
    assert_eq!(mark.block, at);
    assert_eq!(mark.pool_price, price(43));
    assert_eq!(mark.index, price(42));
    let stored = market.emas().unwrap();
    let expected = calculate_emas(
        stored.pair().unwrap(),
        crate::math::pricing::PricePair::try_from_x96(price(43).x96(), price(42).x96()).unwrap(),
        stored.last_touch,
        at.timestamp,
        ema_window,
    )
    .unwrap();
    assert_eq!(mark.emas, expected);

    assert_eq!(
        Replay::fold(&tape[..1]).mark_at(at, ema_window).unwrap(),
        None,
        "no mark before a swap and a print"
    );
}

/// A donation or a booking moves the margin total silently; a swap's
/// insurance fee while debt stands repays debt silently and removes
/// less than its gross fees. Each counts until the next statement, and
/// a statement clears it.
#[test]
fn silences_are_counted_until_the_next_statement() {
    let mut tape = tape();
    tape.push(row(
        15,
        0,
        MarketEvent::Donated {
            donor: Address::repeat_byte(0xD0),
            amount: UsdcAtoms::new(1_000_000),
            bad_debt: UsdcAtoms::new(1_000_000),
            insurance: UsdcAtoms::ZERO,
        },
    ));
    tape.push(row(
        16,
        0,
        MarketEvent::TakerOpened {
            pos_id: U256::from(3),
            swap: swap(price(44), 10_000),
        },
    ));
    let market = genesis(&tape);
    assert_eq!(
        market.gaps(),
        Gaps {
            silences: Silences {
                total_margin_unemitted: 2,
                bad_debt_unemitted: 1,
            },
            unknowns: Unknowns {
                margin_unknown: 2,
                ..Unknowns::default()
            },
            faults: Faults::default(),
        },
        "the donation, and the swap's fees net of what repaid the debt; two takers stand"
    );
    assert_eq!(
        market.solvency().unwrap().bad_debt,
        UsdcAtoms::new(1_000_000)
    );

    tape.push(row(
        17,
        0,
        MarketEvent::LossSocialized {
            original_amount: UsdcAtoms::new(500_000),
            fee_charged: UsdcAtoms::new(5_000),
            bad_debt_after: UsdcAtoms::new(995_000),
        },
    ));
    tape.push(row(
        17,
        1,
        MarketEvent::MarginTransferred {
            margin_delta: UsdcDelta::new(-495_000),
            total_margin: UsdcAtoms::new(90_505_000),
        },
    ));
    let market = genesis(&tape);
    assert_eq!(
        market.gaps(),
        Gaps {
            unknowns: Unknowns {
                margin_unknown: 2,
                ..Unknowns::default()
            },
            ..Gaps::default()
        }
    );

    // A swap while no debt stands repays nothing, so it is no silence.
    let mut clear = tape.clone();
    clear.push(row(
        18,
        0,
        MarketEvent::LossSocialized {
            original_amount: UsdcAtoms::new(995_000),
            fee_charged: UsdcAtoms::new(995_000),
            bad_debt_after: UsdcAtoms::ZERO,
        },
    ));
    clear.push(row(
        19,
        0,
        MarketEvent::TakerOpened {
            pos_id: U256::from(4),
            swap: swap(price(45), 10_000),
        },
    ));
    assert_eq!(genesis(&clear).gaps().silences.bad_debt_unemitted, 0);
}

/// The fold of the whole tape equals the combination of the folds of
/// its two halves, at every cut, gaps included.
#[test]
fn the_fold_of_a_concatenation_is_the_combination_of_the_folds() {
    let mut tape = tape();
    tape.push(row(
        15,
        0,
        MarketEvent::Donated {
            donor: Address::repeat_byte(0xD0),
            amount: UsdcAtoms::new(1_000_000),
            bad_debt: UsdcAtoms::new(1_000_000),
            insurance: UsdcAtoms::ZERO,
        },
    ));
    tape.push(row(
        16,
        0,
        MarketEvent::TakerOpened {
            pos_id: U256::from(3),
            swap: swap(price(44), 10_000),
        },
    ));
    tape.push(row(
        16,
        1,
        MarketEvent::PositionTransferred {
            from: Address::repeat_byte(0x0A),
            to: Address::repeat_byte(0x0B),
            pos_id: U256::from(1),
        },
    ));
    let whole = Replay::fold(&tape);
    for cut in 0..=tape.len() {
        let mut left = Replay::fold(&tape[..cut]);
        left.combine(Replay::fold(&tape[cut..]));
        assert_eq!(left, whole, "cut at {cut}");
    }
}

/// From genesis the books, capacity and open interest are zero and
/// stated, so a market that never booked debt reads as debt-free rather
/// than unknown; what the creation log alone carries stays unknown. A
/// genesis fold continued by a segment fold is the genesis fold of the
/// whole.
#[test]
fn genesis_states_the_zeros_and_leaves_the_creation_log_unknown() {
    let perp = Address::repeat_byte(0xF0);
    let empty = Replay::from_genesis(perp);
    assert_eq!(empty.perp(), perp);
    assert_eq!(
        empty.solvency(),
        Some(SolvencyState {
            bad_debt: UsdcAtoms::ZERO,
            total_margin: UsdcAtoms::ZERO,
        })
    );
    let genesis_block = BlockContext::default();
    assert_eq!(
        empty.capacity_at(genesis_block),
        Some(MarketCapacity {
            block: genesis_block,
            capacity: PerSide::default(),
            open_interest: PerSide::default(),
        })
    );
    assert_eq!(empty.pool_price().value(), None);
    assert_eq!(empty.emas(), None);
    assert_eq!(empty.module(ModuleKind::Pricing), None);
    assert_eq!(empty.gaps(), Gaps::default());

    // A tape that never books debt: the books are still known.
    let tape = tape();
    let debt_free: Vec<TapeEvent> = tape
        .iter()
        .filter(|row| !matches!(row.event, MarketEvent::BadDebtAccounted { .. }))
        .copied()
        .collect();
    let mut from_genesis = Replay::from_genesis(perp);
    for row in &debt_free {
        from_genesis.apply(row);
    }
    assert_eq!(
        from_genesis.solvency(),
        Some(SolvencyState {
            bad_debt: UsdcAtoms::ZERO,
            total_margin: UsdcAtoms::new(90_000_000),
        })
    );
    assert_eq!(
        Replay::fold(&debt_free).solvency(),
        None,
        "a segment fold does not know the debt was never booked"
    );

    // Genesis then a segment is genesis over the whole.
    for cut in 0..=tape.len() {
        let mut left = Replay::from_genesis(perp);
        for row in &tape[..cut] {
            left.apply(row);
        }
        left.combine(Replay::fold(&tape[cut..]));
        let mut whole = Replay::from_genesis(perp);
        for row in &tape {
            whole.apply(row);
        }
        assert_eq!(left, whole, "cut at {cut}");
    }
}

fn with_tx(mut row: TapeEvent, tx: u8) -> TapeEvent {
    row.tx_hash = B256::with_last_byte(tx);
    row
}

fn margin_transferred(delta: i128, total: u128) -> MarketEvent {
    MarketEvent::MarginTransferred {
        margin_delta: UsdcDelta::new(delta),
        total_margin: UsdcAtoms::new(total),
    }
}

/// A swap's protocol, creator and insurance fees (30,000 here) leave
/// the margin total with no event, and when they leave relative to the
/// transaction's `MarginTransferred` is the path's.
#[test]
fn swap_fees_leave_the_margin_total_as_the_build_orders_them() {
    let open = |tx| {
        vec![
            with_tx(
                row(10, 0, margin_transferred(1_000_000_000, 1_000_000_000)),
                tx,
            ),
            with_tx(
                row(
                    10,
                    1,
                    MarketEvent::TakerOpened {
                        pos_id: U256::from(1),
                        swap: swap(price(40), 10_000),
                    },
                ),
                tx,
            ),
        ]
    };
    // An open transfers the deposit first, then removes the fees.
    let opened = genesis(&open(1));
    assert_eq!(
        opened.solvency().unwrap().total_margin,
        UsdcAtoms::new(999_970_000)
    );
    let one_taker = Gaps {
        unknowns: Unknowns {
            margin_unknown: 1,
            ..Unknowns::default()
        },
        ..Gaps::default()
    };
    assert_eq!(opened.gaps(), one_taker);

    let adjust_with = |delta: i128, tx| {
        vec![
            with_tx(row(20, 0, margin_transferred(delta, 500_000_000)), tx),
            with_tx(
                row(
                    20,
                    1,
                    MarketEvent::TakerAdjusted {
                        pos_id: U256::from(1),
                        swap: swap(price(41), 10_000),
                        funding: UsdcDelta::new(0),
                        util_fees: UsdcAtoms::ZERO,
                    },
                ),
                tx,
            ),
        ]
    };
    // An adjust transfers a deposit before the removal, a withdrawal
    // after it.
    let with_deposit: Vec<TapeEvent> = open(1)
        .into_iter()
        .chain(adjust_with(50_000_000, 2))
        .collect();
    assert_eq!(
        genesis(&with_deposit).solvency().unwrap().total_margin,
        UsdcAtoms::new(499_970_000)
    );
    for delta in [-50_000_000, 0] {
        let with_withdrawal: Vec<TapeEvent> =
            open(1).into_iter().chain(adjust_with(delta, 2)).collect();
        assert_eq!(
            genesis(&with_withdrawal).solvency().unwrap().total_margin,
            UsdcAtoms::new(500_000_000),
            "adjust with delta {delta}"
        );
    }

    // A liquidation closes before it transfers the fee, so the fees
    // leave after no statement and the transfer that follows restates
    // the total.
    let liquidation = vec![
        with_tx(
            row(
                30,
                0,
                MarketEvent::TakerClosed {
                    pos_id: U256::from(1),
                    swap: swap(price(42), 10_000),
                    funding: UsdcDelta::new(0),
                    util_fees: UsdcAtoms::ZERO,
                    liquidation_fee: UsdcAtoms::ZERO,
                    is_liquidation: false,
                },
            ),
            3,
        ),
        with_tx(
            row(
                30,
                1,
                MarketEvent::TakerLiquidated {
                    pos_id: U256::from(1),
                    perp_amount: PerpAtoms::new(1_000_000),
                    liquidation_fee: UsdcAtoms::new(2_000_000),
                },
            ),
            3,
        ),
    ];
    let mut liquidated = genesis(&open(1));
    for r in &liquidation {
        liquidated.apply(r);
    }
    assert_eq!(
        liquidated.solvency().unwrap().total_margin,
        UsdcAtoms::new(999_940_000),
        "the close's fees left after the open's statement"
    );
    liquidated.apply(&with_tx(
        row(30, 2, margin_transferred(-2_000_000, 997_940_000)),
        3,
    ));
    assert_eq!(
        liquidated.solvency().unwrap().total_margin,
        UsdcAtoms::new(997_940_000)
    );

    // A segment fold removes the same fees from the total it was told,
    // and invents no debt it was never told.
    let segment = Replay::fold(&open(1));
    assert_eq!(segment.gaps(), one_taker);
    assert_eq!(
        segment.solvency(),
        None,
        "a segment never told the debt does not invent it"
    );
}

fn settle() -> MakerSettle {
    MakerSettle {
        funding: UsdcDelta::new(1_000),
        util_fees: PerSide::new(UsdcAtoms::new(10), UsdcAtoms::new(20)),
        lp_fees: UsdcAtoms::new(30),
    }
}

fn sized_swap(perp_delta: i128, pool_price: Price) -> SwapInfo {
    SwapInfo {
        perp_delta: PerpDelta::new(perp_delta),
        ..swap(pool_price, 0)
    }
}

fn modify(pos: u64, lower: i32, upper: i32, delta: i128) -> MarketEvent {
    MarketEvent::ModifyLiquidity {
        pool_id: B256::with_last_byte(7),
        sender: Address::ZERO,
        tick_lower: lower,
        tick_upper: upper,
        liquidity_delta: LDelta::new(delta),
        salt: B256::from(U256::from(pos)),
    }
}

fn tick(gross: u128, net: i128) -> TickLiquidity {
    TickLiquidity {
        gross: LUnits::new(gross),
        net: LDelta::new(net),
    }
}

/// Two positions' lives: taker 1 opens, adds, is liquidated in part and
/// then whole; maker 2 deposits a band, trims it, and is converted when
/// the rest is pulled, then closes as the taker it became.
fn lifecycle() -> Vec<TapeEvent> {
    vec![
        row(
            20,
            0,
            MarketEvent::TakerOpened {
                pos_id: U256::from(1),
                swap: sized_swap(1_000_000, price(43)),
            },
        ),
        row(21, 0, modify(2, -600, 600, 1_000)),
        row(
            21,
            1,
            MarketEvent::MakerOpened {
                pos_id: U256::from(2),
            },
        ),
        row(
            22,
            0,
            MarketEvent::TicksCrossed {
                starting_tick: 0,
                ending_tick: 10,
                zero_for_one: false,
            },
        ),
        row(
            22,
            1,
            MarketEvent::TakerAdjusted {
                pos_id: U256::from(1),
                swap: sized_swap(500_000, price(44)),
                funding: UsdcDelta::ZERO,
                util_fees: UsdcAtoms::ZERO,
            },
        ),
        row(23, 0, modify(2, -600, 600, -400)),
        row(
            23,
            1,
            MarketEvent::MakerAdjusted {
                pos_id: U256::from(2),
                settle: settle(),
            },
        ),
        row(
            24,
            0,
            MarketEvent::TakerAdjusted {
                pos_id: U256::from(1),
                swap: sized_swap(-300_000, price(44)),
                funding: UsdcDelta::ZERO,
                util_fees: UsdcAtoms::ZERO,
            },
        ),
        row(
            24,
            1,
            MarketEvent::TakerLiquidated {
                pos_id: U256::from(1),
                perp_amount: PerpAtoms::new(300_000),
                liquidation_fee: UsdcAtoms::new(1_000),
            },
        ),
        row(25, 0, modify(2, -600, 600, -600)),
        row(
            25,
            1,
            MarketEvent::MakerConverted {
                pos_id: U256::from(2),
                settle: settle(),
                liquidation_fee: UsdcAtoms::ZERO,
                is_liquidation: false,
            },
        ),
        row(
            26,
            0,
            MarketEvent::TakerClosed {
                pos_id: U256::from(2),
                swap: sized_swap(-700, price(45)),
                funding: UsdcDelta::ZERO,
                util_fees: UsdcAtoms::ZERO,
                liquidation_fee: UsdcAtoms::ZERO,
                is_liquidation: false,
            },
        ),
        row(
            26,
            1,
            MarketEvent::TakerClosed {
                pos_id: U256::from(1),
                swap: sized_swap(-1_200_000, price(45)),
                funding: UsdcDelta::ZERO,
                util_fees: UsdcAtoms::ZERO,
                liquidation_fee: UsdcAtoms::ZERO,
                is_liquidation: false,
            },
        ),
        row(
            26,
            2,
            MarketEvent::TakerLiquidated {
                pos_id: U256::from(1),
                perp_amount: PerpAtoms::new(1_200_000),
                liquidation_fee: UsdcAtoms::new(4_000),
            },
        ),
    ]
}

/// A maker's band is the range its first liquidity change named and
/// the sum of the changes since; the pool's tick map is the same changes
/// summed per tick, and its liquidity the net at or below the tick.
#[test]
fn a_makers_band_and_the_pools_book_are_the_sum_of_liquidity_changes() {
    let tape = lifecycle();
    let band = |lower, upper, liquidity| {
        MakerBand::new(
            TickRange::new(lower, upper).unwrap(),
            LUnits::new(liquidity),
        )
    };

    // Deposited: the band stands, at the price of the swap before it.
    let deposited = genesis(&tape[..3]);
    let maker = deposited.position(U256::from(2)).unwrap();
    assert_eq!(maker.maker_band(), Some(band(-600, 600, 1_000)));
    assert_eq!(maker.deposit_pool_price(), Some(price(43)));
    assert_eq!(maker.opened(), Some(tape[2].point()));
    assert_eq!(
        deposited.pool_ticks(),
        Some(BTreeMap::from([
            (-600, tick(1_000, 1_000)),
            (600, tick(1_000, -1_000))
        ]))
    );
    assert_eq!(
        deposited.pool_tick(),
        None,
        "no swap has moved the tick, and the first tick is the factory's"
    );
    assert_eq!(deposited.pool_liquidity(), None);

    // A swap moves the tick into the band; the band's liquidity is active.
    let crossed = genesis(&tape[..5]);
    assert_eq!(crossed.pool_tick(), Some(10));
    assert_eq!(crossed.pool_liquidity(), Some(LUnits::new(1_000)));

    // Trimmed.
    let trimmed = genesis(&tape[..7]);
    assert_eq!(
        trimmed.position(U256::from(2)).unwrap().maker_band(),
        Some(band(-600, 600, 600))
    );
    assert_eq!(trimmed.pool_liquidity(), Some(LUnits::new(600)));

    // Pulled and converted: the band is gone, the tick map is empty, and
    // the position is a taker of a size no event carries.
    let converted = genesis(&tape[..11]);
    let taker = converted.position(U256::from(2)).unwrap();
    assert_eq!(taker.maker_band(), None);
    assert_eq!(taker.taker_size(), None);
    assert!(matches!(
        taker.kind(),
        PositionKind::Taker { sized: false, .. }
    ));
    assert!(taker.is_open());
    assert_eq!(converted.pool_ticks(), Some(BTreeMap::new()));
    assert_eq!(converted.pool_liquidity(), Some(LUnits::ZERO));
    assert_eq!(converted.gaps().unknowns.taker_size_unknown, 1);

    // Closed as a taker.
    let closed = genesis(&tape[..12]);
    let taker = closed.position(U256::from(2)).unwrap();
    assert_eq!(taker.closed(), Some(tape[11].point()));
    assert_eq!(closed.gaps().unknowns.taker_size_unknown, 0);
}

/// A taker's size is the sum of its swaps' perp deltas; each dedicated
/// liquidation event, and each tailed close, counts one liquidation.
#[test]
fn a_taker_is_sized_by_its_swaps_and_its_liquidations_are_counted() {
    let tape = lifecycle();
    let one = U256::from(1);

    let opened = genesis(&tape[..1]);
    let taker = opened.position(one).unwrap();
    assert_eq!(taker.taker_size(), Some(PerpDelta::new(1_000_000)));
    assert_eq!(taker.opened(), Some(tape[0].point()));
    assert_eq!(taker.liquidations(), 0);

    let added = genesis(&tape[..5]);
    assert_eq!(
        added.position(one).unwrap().taker_size(),
        Some(PerpDelta::new(1_500_000))
    );

    let partly = genesis(&tape[..9]);
    let taker = partly.position(one).unwrap();
    assert_eq!(taker.taker_size(), Some(PerpDelta::new(1_200_000)));
    assert_eq!(taker.liquidations(), 1);
    assert!(taker.is_open());

    let whole = genesis(&tape);
    let taker = whole.position(one).unwrap();
    assert_eq!(taker.taker_size(), Some(PerpDelta::ZERO));
    assert_eq!(taker.liquidations(), 2);
    assert_eq!(taker.closed(), Some(tape[12].point()));
    assert_eq!(whole.positions().open().count(), 0);
    assert_eq!(whole.gaps().unknowns.margin_unknown, 0);

    // The retired build's shape says it in the close's tail instead.
    let tailed = genesis(&[
        tape[0],
        row(
            21,
            0,
            MarketEvent::TakerClosed {
                pos_id: one,
                swap: sized_swap(-1_000_000, price(44)),
                funding: UsdcDelta::ZERO,
                util_fees: UsdcAtoms::ZERO,
                liquidation_fee: UsdcAtoms::new(500),
                is_liquidation: true,
            },
        ),
    ]);
    assert_eq!(tailed.position(one).unwrap().liquidations(), 1);
}

/// A segment that starts mid-life knows what moved and not where a
/// position stands, and says so: no size, no band, no tick map, and the
/// positions counted as partial.
#[test]
fn a_segment_knows_what_moved_and_not_where_positions_stand() {
    let tape = lifecycle();
    let segment = Replay::fold(&tape[3..9]);

    let taker = segment.position(U256::from(1)).unwrap();
    assert_eq!(taker.opened(), None);
    assert_eq!(taker.taker_size(), None);
    assert_eq!(
        taker.kind(),
        PositionKind::Taker {
            moved: PerpDelta::new(200_000),
            sized: false,
        }
    );
    assert_eq!(taker.liquidations(), 1);

    let maker = segment.position(U256::from(2)).unwrap();
    assert_eq!(maker.opened(), None);
    assert_eq!(maker.maker_band(), None, "the level is unknown");
    assert_eq!(maker.deposit_pool_price(), None);
    assert_eq!(
        maker.kind(),
        PositionKind::Maker {
            range: Some(TickRange::new(-600, 600).unwrap()),
            liquidity: LDelta::new(-400),
            sized: false,
            deposit_pool_price: None,
        }
    );

    assert_eq!(
        segment.pool_ticks(),
        None,
        "a segment's tick map is not whole"
    );
    assert_eq!(segment.pool_liquidity(), None);
    assert_eq!(segment.pool_tick(), Some(10));
    assert_eq!(
        segment.gaps(),
        Gaps {
            unknowns: Unknowns {
                partial_positions: 2,
                taker_size_unknown: 1,
                margin_unknown: 2,
            },
            ..Gaps::default()
        }
    );
}

/// The combine law over positions and the pool's liquidity, at every cut, from
/// genesis and as segments.
#[test]
fn positions_and_the_pool_combine_at_every_cut() {
    let tape = lifecycle();
    let whole = genesis(&tape);
    let segments = Replay::fold(&tape);
    for cut in 0..=tape.len() {
        let mut from_genesis = genesis(&tape[..cut]);
        from_genesis.combine(Replay::fold(&tape[cut..]));
        assert_eq!(from_genesis, whole, "genesis cut at {cut}");

        let mut left = Replay::fold(&tape[..cut]);
        left.combine(Replay::fold(&tape[cut..]));
        assert_eq!(left, segments, "segment cut at {cut}");
    }
}

/// The base fixture, an accrual, then the two positions' lives.
fn whole_market() -> Vec<TapeEvent> {
    let mut tape = tape();
    tape.push(row(
        15,
        0,
        MarketEvent::CumulativesAccrued {
            cumulatives: CumulativesInfo {
                funding: Funding::from_x96(I256::try_from(1_000i64).unwrap()),
                funding_div_sqrt_p: FundingPerSqrtPrice::from_x96(
                    I256::try_from(2_000i64).unwrap(),
                ),
                util_payments: PerSide::new(
                    Earnings::from_x96(U256::from(3)),
                    Earnings::from_x96(U256::from(4)),
                ),
                util_earnings: PerSide::new(
                    Earnings::from_x96(U256::from(5)),
                    Earnings::from_x96(U256::from(6)),
                ),
            },
        },
    ));
    tape.extend(lifecycle());
    tape
}

/// What a read would have returned at the fold's block: every figure
/// the fold holds, taken from its accessors, with a margin invented for
/// each position since the tape never carries one.
fn seed_of(market: &Replay, at: BlockContext) -> Seed {
    let module = |kind| market.module(kind).unwrap_or(Address::ZERO);
    let emas = market.emas().unwrap();
    Seed {
        perp: market.perp(),
        block: at,
        pool_price: market.pool_price().value().unwrap(),
        index: market.index().value().unwrap(),
        emas,
        rates: MarketRates {
            funding_per_day: market.funding_per_day().unwrap(),
            util_fee_per_day: market.util_fee_per_day().unwrap(),
            last_touch: emas.last_touch,
        },
        cumulatives: market.cumulatives().unwrap(),
        capacity: market.capacity_at(at).unwrap(),
        solvency: market.solvency().unwrap(),
        modules: ContractModules {
            beacon: module(ModuleKind::Beacon),
            fees: module(ModuleKind::Fees),
            funding: module(ModuleKind::Funding),
            marginRatios: module(ModuleKind::MarginRatios),
            priceImpact: module(ModuleKind::PriceImpact),
            pricing: module(ModuleKind::Pricing),
        },
        ticks: market.pool_ticks().unwrap(),
        tick: market.pool_tick().unwrap(),
        positions: market
            .positions()
            .open()
            .map(|(pos_id, state)| {
                let margin = UsdcAtoms::new(1_000_000 + pos_id.to::<u128>());
                let position = match (state.taker_size(), state.maker_band()) {
                    (Some(size), _) => SeedPosition::Taker { size, margin },
                    (_, Some(band)) => SeedPosition::Maker {
                        range: band.range,
                        liquidity: LDelta::new(band.liquidity.units() as i128),
                        margin,
                    },
                    _ => SeedPosition::Unknown { margin },
                };
                (pos_id, position)
            })
            .collect(),
    }
}

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
    let prefix = genesis(&tape[..3]);
    let mut seeded =
        Replay::from_seed(seed_of(&genesis(&whole_market()[..16]), block_of(&tape[2]))).unwrap();
    let _ = prefix;
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

/// A print, an open, then a liquidation the build says twice in one
/// transaction: the tailed close and the dedicated event after it.
fn liquidation_tape() -> Vec<TapeEvent> {
    vec![
        row(30, 0, MarketEvent::IndexUpdated { index: price(42) }),
        row(
            31,
            0,
            MarketEvent::TakerOpened {
                pos_id: U256::from(1),
                swap: sized_swap(1_000_000, price(43)),
            },
        ),
        row(
            32,
            0,
            MarketEvent::TakerClosed {
                pos_id: U256::from(1),
                swap: sized_swap(-1_000_000, price(40)),
                funding: UsdcDelta::ZERO,
                util_fees: UsdcAtoms::ZERO,
                liquidation_fee: UsdcAtoms::new(5_000),
                is_liquidation: true,
            },
        ),
        row(
            32,
            1,
            MarketEvent::TakerLiquidated {
                pos_id: U256::from(1),
                perp_amount: PerpAtoms::new(1_000_000),
                liquidation_fee: UsdcAtoms::new(5_000),
            },
        ),
    ]
}

/// One arrival however many events say it, with the prices around it;
/// and the same arrival whether the cut falls inside the transaction or
/// before the prices it is recorded against.
#[test]
fn a_liquidation_is_one_arrival_with_the_prices_around_it() {
    let tape = liquidation_tape();
    let market = genesis(&tape);

    let liquidations = market.liquidations();
    assert_eq!(liquidations.len(), 1);
    let arrival = liquidations.last().unwrap();
    assert_eq!(
        arrival.point,
        tape[2].point(),
        "the close, not the event after it"
    );
    assert_eq!(
        arrival.mark,
        Liquidation {
            pos_id: U256::from(1),
            role: PositionRole::Taker,
            fee: UsdcAtoms::new(5_000),
            pool_price_before: Some(price(43)),
            pool_price_after: Some(price(40)),
            index_then: Some(price(42)),
        }
    );
    assert_eq!(market.swaps().len(), 2);
    assert_eq!(market.settlements().len(), 1);
    assert_eq!(market.prints().len(), 1);

    for cut in 0..=tape.len() {
        let mut left = genesis(&tape[..cut]);
        left.combine(Replay::fold(&tape[cut..]));
        assert_eq!(left, market, "cut at {cut}");
    }
}

/// The other build's shape: the close says nothing of a liquidation
/// and the dedicated event after it carries no price. The record is
/// the same one, assembled from the transaction's progress, whether
/// the cut falls before the close, between it and the event, or after.
#[test]
fn a_liquidation_said_only_by_the_dedicated_event_is_the_same_record() {
    let mut tape = liquidation_tape();
    let MarketEvent::TakerClosed { is_liquidation, .. } = &mut tape[2].event else {
        unreachable!("the fixture's third row is the close");
    };
    *is_liquidation = false;
    let market = genesis(&tape);

    let liquidations = market.liquidations();
    assert_eq!(liquidations.len(), 1);
    let arrival = liquidations.last().unwrap();
    assert_eq!(arrival.point, tape[3].point(), "the dedicated event");
    assert_eq!(
        arrival.mark,
        Liquidation {
            pos_id: U256::from(1),
            role: PositionRole::Taker,
            fee: UsdcAtoms::new(5_000),
            pool_price_before: Some(price(43)),
            pool_price_after: Some(price(40)),
            index_then: Some(price(42)),
        },
        "the prices around the transaction, not around the event"
    );

    for cut in 0..=tape.len() {
        let mut left = genesis(&tape[..cut]);
        left.combine(Replay::fold(&tape[cut..]));
        assert_eq!(left, market, "cut at {cut}");
    }
}

/// The series keep what the contract stated, and a window of it.
#[test]
fn the_replay_keeps_series_and_trims_them_to_a_retention() {
    let tape = liquidation_tape();
    let market = genesis(&tape);
    assert_eq!(market.index().value(), Some(price(42)));
    assert_eq!(
        market.pool_price().samples().len(),
        2,
        "the open's and the close's prices"
    );
    assert_eq!(
        market.pool_price().at(tape[1].point()).map(|r| r.value),
        Some(price(43))
    );
    assert_eq!(
        market
            .pool_price()
            .change_over(Window::seconds(10))
            .map(|r| r.value),
        Some(Change {
            from: price(43),
            to: price(40)
        })
    );

    // The rows are ten seconds apart; a five-second window keeps the
    // last sample and the one before it.
    let mut short =
        Replay::from_genesis(Address::ZERO).retaining(Retention::Last(Window::seconds(5)));
    for row in &tape {
        short.apply(row);
    }
    assert_eq!(short.pool_price().samples().len(), 2);
    assert_eq!(short.pool_price().value(), Some(price(40)));
    assert_eq!(
        short.open_interest().samples().len(),
        1,
        "genesis's zero is the one sample before the window"
    );
    assert_eq!(short.deposited(), UsdcAtoms::ZERO);
}
