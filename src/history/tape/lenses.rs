//! Typed views over a run of a tape, as `str` has `lines`: the swaps, the
//! prints, and the rows about one position, each a filtering iterator that
//! yields a record with its point. The marks a lens yields are the ones
//! the replay's activity fold keeps as arrivals, so a question asked of a
//! tape and of a replay is asked in one vocabulary.

use alloy::primitives::U256;

use crate::events::{MarketEvent, SwapInfo};
use crate::history::fold::Arrival;
use crate::units::Price;

use super::{TapeEvent, TapeSlice};

/// What a taker's swap was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapAction {
    /// The position's opening trade.
    Opened,
    /// A change of size on an open position.
    Adjusted,
    /// The closing trade, a liquidation's included.
    Closed,
}

/// A taker's swap, as the event settled it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Swap {
    /// The position that swapped.
    pub pos_id: U256,
    /// What the swap was for.
    pub action: SwapAction,
    /// What moved, at what price, for what fees.
    pub info: SwapInfo,
}

impl Swap {
    /// The swap an event carries, if it is a taker's trade.
    pub fn of(event: &MarketEvent) -> Option<Self> {
        let (pos_id, action, info) = match *event {
            MarketEvent::TakerOpened { pos_id, swap } => (pos_id, SwapAction::Opened, swap),
            MarketEvent::TakerAdjusted { pos_id, swap, .. } => (pos_id, SwapAction::Adjusted, swap),
            MarketEvent::TakerClosed { pos_id, swap, .. } => (pos_id, SwapAction::Closed, swap),
            _ => return None,
        };
        Some(Self {
            pos_id,
            action,
            info,
        })
    }
}

/// The position an event is about, for the kinds that are about one.
fn position_of(event: &MarketEvent) -> Option<U256> {
    match *event {
        MarketEvent::MakerOpened { pos_id }
        | MarketEvent::MakerAdjusted { pos_id, .. }
        | MarketEvent::MakerConverted { pos_id, .. }
        | MarketEvent::MakerClosed { pos_id, .. }
        | MarketEvent::MakerLiquidated { pos_id, .. }
        | MarketEvent::MakerBackstopped { pos_id, .. }
        | MarketEvent::TakerOpened { pos_id, .. }
        | MarketEvent::TakerAdjusted { pos_id, .. }
        | MarketEvent::TakerClosed { pos_id, .. }
        | MarketEvent::TakerLiquidated { pos_id, .. }
        | MarketEvent::TakerBackstopped { pos_id, .. }
        | MarketEvent::PositionTransferred { pos_id, .. } => Some(pos_id),
        _ => None,
    }
}

impl TapeSlice {
    /// Every taker swap, with the position it moved and what it was for.
    pub fn swaps(&self) -> impl Iterator<Item = Arrival<Swap>> + '_ {
        self.iter()
            .filter_map(|row| Some(row.arrival(Swap::of(&row.event)?)))
    }

    /// Every print of the beacon's index.
    pub fn prints(&self) -> impl Iterator<Item = Arrival<Price>> + '_ {
        self.iter().filter_map(|row| match row.event {
            MarketEvent::IndexUpdated { index } => Some(row.arrival(index)),
            _ => None,
        })
    }

    /// Every row about position `pos_id`: its trades, settlements,
    /// liquidations and transfers, in chain order.
    pub fn of_position(&self, pos_id: U256) -> impl Iterator<Item = &TapeEvent> + '_ {
        self.iter()
            .filter(move |row| position_of(&row.event) == Some(pos_id))
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::Address;

    use super::*;
    use crate::history::tape::Tape;
    use crate::history::test_support::tape::{price, row, swap};
    use crate::units::{UsdcAtoms, UsdcDelta};

    fn tape() -> Tape {
        Tape::new(vec![
            row(1, 0, MarketEvent::IndexUpdated { index: price(40) }),
            row(
                2,
                0,
                MarketEvent::TakerOpened {
                    pos_id: U256::from(1),
                    swap: swap(1_000, price(41), 0),
                },
            ),
            row(
                2,
                1,
                MarketEvent::PositionTransferred {
                    from: Address::ZERO,
                    to: Address::repeat_byte(0x0A),
                    pos_id: U256::from(1),
                },
            ),
            row(
                3,
                0,
                MarketEvent::TakerAdjusted {
                    pos_id: U256::from(1),
                    swap: swap(500, price(42), 0),
                    funding: UsdcDelta::ZERO,
                    util_fees: UsdcAtoms::ZERO,
                },
            ),
            row(
                3,
                1,
                MarketEvent::MakerOpened {
                    pos_id: U256::from(2),
                },
            ),
            row(4, 0, MarketEvent::IndexUpdated { index: price(43) }),
            row(
                5,
                0,
                MarketEvent::TakerClosed {
                    pos_id: U256::from(1),
                    swap: swap(-1_500, price(44), 0),
                    funding: UsdcDelta::ZERO,
                    util_fees: UsdcAtoms::ZERO,
                    liquidation_fee: UsdcAtoms::ZERO,
                    is_liquidation: false,
                },
            ),
        ])
        .unwrap()
    }

    #[test]
    fn the_swaps_lens_yields_each_taker_trade_with_what_it_was_for() {
        let tape = tape();
        let swaps: Vec<Arrival<Swap>> = tape.swaps().collect();
        assert_eq!(
            swaps
                .iter()
                .map(|a| (a.point.block, a.mark.action, a.mark.info.pool_price))
                .collect::<Vec<_>>(),
            vec![
                (2, SwapAction::Opened, price(41)),
                (3, SwapAction::Adjusted, price(42)),
                (5, SwapAction::Closed, price(44)),
            ]
        );
        assert_eq!(
            swaps[0].tx, tape[1].tx_hash,
            "the arrival carries the row's transaction"
        );
        assert_eq!(tape[..2].len(), 2);
        assert_eq!(tape.in_blocks(3..).swaps().count(), 2, "a lens over a run");
    }

    #[test]
    fn the_prints_lens_yields_the_index_at_each_print() {
        let prints: Vec<Price> = tape().prints().map(|a| a.mark).collect();
        assert_eq!(prints, vec![price(40), price(43)]);
    }

    #[test]
    fn a_positions_rows_are_its_trades_and_transfers_and_no_one_elses() {
        let tape = tape();
        let blocks: Vec<(u64, u64)> = tape
            .of_position(U256::from(1))
            .map(|row| (row.block_number, row.log_index))
            .collect();
        assert_eq!(blocks, vec![(2, 0), (2, 1), (3, 0), (5, 0)]);
        assert_eq!(tape.of_position(U256::from(2)).count(), 1);
        assert_eq!(tape.of_position(U256::from(9)).count(), 0);
    }
}
