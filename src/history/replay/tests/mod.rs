//! Tests for the replay over fixture tapes, one file per concern; the
//! tapes and the row builders they share are here.

mod activity;
mod market;
mod positions;
mod seeds;

use alloy::primitives::{B256, I256, U256};

use super::seed::{Seed, SeedPosition};
use super::*;
use crate::client::{MarketRates, PositionRole};
use crate::contracts::Modules as ContractModules;
use crate::events::MarketEvent;
use crate::history::fold::{Change, Window};
use crate::history::tape::Wallets;
use crate::history::test_support::tape::{
    assert_combine_law, modify, per_side, pool_initialized, pool_swapped, price, row, settle, swap,
};
use crate::math::pricing::calculate_emas;
use crate::math::range::{MakerBand, TickRange};
use crate::units::{
    Earnings, Funding, FundingPerSqrtPrice, LDelta, PerpAtoms, PerpDelta, SqrtPrice, UsdcAtoms,
    UsdcDelta,
};

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
                pos_id: U256::from(9),
                swap: swap(1_000_000, price(43), 10_000),
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
                swap: swap(1_000_000, price(43), 0),
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
                swap: swap(500_000, price(44), 0),
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
                swap: swap(-300_000, price(44), 0),
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
                swap: swap(-700, price(45), 0),
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
                swap: swap(-1_200_000, price(45), 0),
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

/// The lifecycle with the pool's own events: created at tick 0, then each
/// taker swap's pool swap, which leaves the maker's three liquidity changes
/// at ticks 5, 10 and 8.
fn priced_lifecycle() -> Vec<TapeEvent> {
    let mut tape = lifecycle();
    tape.extend([
        row(19, 0, pool_initialized(0)),
        row(20, 1, pool_swapped(5)),
        row(22, 2, pool_swapped(10)),
        row(24, 2, pool_swapped(8)),
        row(26, 3, pool_swapped(2)),
    ]);
    tape.sort_by_key(TapeEvent::point);
    tape
}

/// The base fixture, an accrual, then the two positions' lives with the
/// pool's events.
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
    tape.extend(priced_lifecycle());
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
        // A tape without the pool's own events stands at the root of the
        // price its swaps printed.
        sqrt_price: market.market().positions.pool().map_or_else(
            || SqrtPrice::try_from(market.pool_price().value().unwrap()).unwrap(),
            |pool| pool.sqrt_price,
        ),
        positions: market
            .positions()
            .open()
            .map(|(pos_id, state)| {
                let margin = UsdcAtoms::new(1_000_000 + pos_id.to::<u128>());
                let position = match (
                    state.taker_size(),
                    state.taker_usd(),
                    state.maker_band(),
                    state.kind(),
                ) {
                    (Some(size), Some(usd), _, _) => SeedPosition::Taker { size, usd, margin },
                    (_, _, Some(band), PositionKind::Maker { moved, usd, .. }) => {
                        SeedPosition::Maker {
                            range: band.range,
                            liquidity: LDelta::new(band.liquidity.units() as i128),
                            moved,
                            usd,
                            margin,
                        }
                    }
                    _ => SeedPosition::Unknown { margin },
                };
                (pos_id, position)
            })
            .collect(),
    }
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
                swap: swap(1_000_000, price(43), 0),
            },
        ),
        row(
            32,
            0,
            MarketEvent::TakerClosed {
                pos_id: U256::from(1),
                swap: swap(-1_000_000, price(40), 0),
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
