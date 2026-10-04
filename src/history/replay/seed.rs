//! A market's state read at one block, in the shape the fold starts from:
//! what `StateAt` answers, gathered once, so a live cache boots without a
//! scan and a tape tail continues from the read's block.

use std::collections::BTreeMap;

use alloy::primitives::{Address, U256};

use crate::client::{MarketRates, SolvencyState, StateAt};
use crate::contracts::Modules;
use crate::convert::unpack_balance_delta;
use crate::errors::{Result, ValidationError};
use crate::events::CumulativesInfo;
use crate::math::BlockContext;
use crate::math::capacity::MarketCapacity;
use crate::math::pricing::Emas;
use crate::math::range::TickRange;
use crate::math::swap::TickLiquidity;
use crate::units::{LDelta, PerpDelta, Price, UsdcAtoms};

/// Every figure the fold holds, as the reads returned it at one block.
#[derive(Clone)]
pub(crate) struct Seed {
    pub(crate) perp: Address,
    pub(crate) block: BlockContext,
    pub(crate) pool_price: Price,
    pub(crate) index: Price,
    pub(crate) emas: Emas,
    pub(crate) rates: MarketRates,
    pub(crate) cumulatives: CumulativesInfo,
    pub(crate) capacity: MarketCapacity,
    pub(crate) solvency: SolvencyState,
    pub(crate) modules: Modules,
    pub(crate) ticks: BTreeMap<i32, TickLiquidity>,
    pub(crate) tick: i32,
    pub(crate) positions: Vec<(U256, SeedPosition)>,
}

/// One position as its rows describe it: the kind from which row holds
/// it, the level from the row, and the margin, which only a read carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SeedPosition {
    Taker {
        size: PerpDelta,
        margin: UsdcAtoms,
    },
    Maker {
        range: TickRange,
        liquidity: LDelta,
        margin: UsdcAtoms,
    },
    /// A row with neither size nor band, which the contract does not
    /// leave standing; kept as unknown rather than guessed.
    Unknown {
        margin: UsdcAtoms,
    },
}

impl Seed {
    /// Read everything at the state's block. A position row that fails to
    /// read fails the seed: a seed with a hole is not a seed.
    ///
    /// # Errors
    ///
    /// Any read's error, or [`ValidationError::Overflow`] if a band's
    /// liquidity does not fit the signed sum the fold keeps.
    pub(crate) async fn read(state: &StateAt) -> Result<Self> {
        let (mark, emas, rates, cumulatives, capacity, solvency, modules, pool, minted) = tokio::try_join!(
            state.mark(),
            state.emas(),
            state.rates(),
            state.cumulatives(),
            state.capacity(),
            state.solvency(),
            state.modules(),
            state.pool(),
            state.next_pos_id(),
        )?;

        let ids: Vec<U256> = (1..minted).map(U256::from).collect();
        let (rows, bands) = tokio::join!(state.positions(&ids), state.maker_bands(&ids));
        let mut positions = Vec::new();
        for (row, band) in rows.into_iter().zip(bands) {
            let Some(row) = row.row? else {
                continue;
            };
            let margin = UsdcAtoms::new(row.margin);
            let position = match band.row? {
                Some(band) => SeedPosition::Maker {
                    range: band.range,
                    liquidity: LDelta::new(i128::try_from(band.liquidity.units()).map_err(
                        |_| ValidationError::Overflow {
                            context: "a band's liquidity as a signed sum".into(),
                        },
                    )?),
                    margin,
                },
                None => {
                    let (perp, _) = unpack_balance_delta(row.delta);
                    if perp == 0 {
                        SeedPosition::Unknown { margin }
                    } else {
                        SeedPosition::Taker {
                            size: PerpDelta::new(perp),
                            margin,
                        }
                    }
                }
            };
            positions.push((band.pos_id, position));
        }

        Ok(Self {
            perp: state.market().perp(),
            block: state.block(),
            pool_price: mark.pool_price,
            index: mark.index,
            emas,
            rates,
            cumulatives,
            capacity,
            solvency,
            modules,
            ticks: pool.ticks,
            tick: pool.tick,
            positions,
        })
    }
}
