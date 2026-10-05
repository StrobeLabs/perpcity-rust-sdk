//! The market's totals: what the reads would return at the caller's
//! block, what moved in silence, and the combine law from genesis.

use super::*;

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
        let whole = genesis(&with_withdrawal);
        assert_eq!(
            whole.solvency().unwrap().total_margin,
            UsdcAtoms::new(500_000_000),
            "adjust with delta {delta}"
        );
        // A cut between the transfer and the swap leaves the transfer in
        // one segment and the swap in the other, and the fees it would
        // remove on its own are the ones the statement already had out.
        for cut in 0..=with_withdrawal.len() {
            let mut left = genesis(&with_withdrawal[..cut]);
            left.combine(Replay::fold(&with_withdrawal[cut..]));
            assert_eq!(left, whole, "delta {delta}, cut at {cut}");
        }
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
