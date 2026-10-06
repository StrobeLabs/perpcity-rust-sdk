//! The taker-health batch: block-pinned liquidation tests for a set of
//! position ids.
//!
//! The pure test lives in [`crate::math::taker`]; this module is its
//! composition over one [`StateAt`]: the market-wide multicall and the
//! mark once, then per chunk of ids one row multicall carrying each
//! position, its taker checkpoints and its maker row, the last to tell a
//! taker from a band. No storage read: every checkpoint a taker settles
//! against is in its rows.

use alloy::primitives::U256;
use alloy::sol_types::SolCall;

use crate::contracts::{IMulticall3, Perp};
use crate::convert::unpack_balance_delta;
use crate::errors::Result;
use crate::math::taker::{TakerHealth, TakerMarketSnapshot, TakerState};
use crate::units::{Earnings, Funding, PerSide, PerpDelta, Ratio, UsdcAtoms, UsdcDelta};

use super::market::MarketReader;
use super::state::{PerpViews, RowOutcome, StateAt, decode_row, ema_window_secs};
use super::u24_to_u32;

impl MarketReader {
    /// [`StateAt::taker_healths`] at the lagged snapshot block. An empty
    /// input resolves no block.
    pub async fn get_taker_healths(
        &self,
        pos_ids: &[U256],
    ) -> Result<Vec<RowOutcome<TakerHealth>>> {
        if pos_ids.is_empty() {
            return Ok(Vec::new());
        }
        self.state().await?.taker_healths(pos_ids).await
    }
}

impl StateAt {
    /// The liquidation test of each taker position in `pos_ids`, every
    /// read at this block.
    ///
    /// Exactly one [`RowOutcome`] per input id, in input order: `Ok(Some)`
    /// is an open taker's [`TakerHealth`]; `Ok(None)` an id that is not an
    /// open taker at this block, a band, a closed or burned position, a
    /// never-minted id; `Err` that one id's read, decode or arithmetic
    /// failing, the rest of the batch unaffected, worth retrying exactly
    /// when [`PerpCityError::is_transient`](crate::errors::PerpCityError::is_transient)
    /// says so. A chunk's row multicall failing fails the ids it served
    /// with the shared cause; only the market-wide read fails the whole
    /// call.
    ///
    /// Reads are batched: one multicall for the views and the cumulatives,
    /// then the beacon's index and the stored EMAs, once; then per chunk of
    /// at most [`MAX_ROW_BATCH`](super::MAX_ROW_BATCH) ids one row multicall
    /// of three rows per id. The mark is the contract's own, the fair price
    /// [`Self::mark`] reads, so [`TakerHealth::is_liquidatable`] is the
    /// verdict `liquidateTaker` would reach at this block.
    pub async fn taker_healths(&self, pos_ids: &[U256]) -> Result<Vec<RowOutcome<TakerHealth>>> {
        if pos_ids.is_empty() {
            return Ok(Vec::new());
        }
        let market = self.taker_market().await?;
        let outcomes = self
            .rows(
                pos_ids,
                |pos_id| {
                    [
                        Perp::positionsCall { posId: pos_id }.abi_encode(),
                        Perp::takerDetailsCall { posId: pos_id }.abi_encode(),
                        Perp::makerDetailsCall { posId: pos_id }.abi_encode(),
                    ]
                },
                |pos_id, rows| match taker_state(pos_id, rows)? {
                    Some(taker) => Ok(Some(market.taker_health(&taker)?)),
                    None => Ok(None),
                },
            )
            .await;

        // The batch degrades per position instead of failing, so this is
        // the one place the shape of what came back is visible.
        let (mut computed, mut not_a_taker, mut failed) = (0usize, 0usize, 0usize);
        for outcome in &outcomes {
            match &outcome.row {
                Ok(Some(_)) => computed += 1,
                Ok(None) => not_a_taker += 1,
                Err(_) => failed += 1,
            }
        }
        let block = self.block().number;
        if failed > 0 {
            tracing::warn!(
                count = pos_ids.len(),
                computed,
                not_a_taker,
                failed,
                block,
                "taker healths read, some positions failed"
            );
        } else {
            tracing::debug!(
                count = pos_ids.len(),
                computed,
                not_a_taker,
                failed,
                block,
                "taker healths read"
            );
        }
        Ok(outcomes)
    }

    /// The market-wide inputs of the health test at this block: the
    /// cumulatives and the views behind the mark in one multicall, then
    /// the mark itself, every read pinned here.
    async fn taker_market(&self) -> Result<TakerMarketSnapshot> {
        let chain = self.market().chain();
        let perp = Perp::new(self.market().perp(), chain.provider());
        let (modules, pool_state, rates, ema_window, cumls) = chain
            .multicall_at(self.id())
            .add(perp.modules())
            .add(perp.poolState())
            .add(perp.rates())
            .add(perp.EMA_WINDOW())
            .add(perp.cumulatives())
            .aggregate()
            .await
            .map_err(|e| self.multicall_read_error(e))?;
        let views = PerpViews {
            modules,
            pool_state,
            last_touch: rates.lastTouch.to::<u64>(),
            ema_window: ema_window_secs(ema_window)?,
        };
        let mark = self.mark_from(&views).await?;
        Ok(TakerMarketSnapshot {
            block: self.block(),
            funding: Funding::from_x96(cumls.fundingX96),
            util_payments: PerSide::new(
                Earnings::from_x96(cumls.longUtilPaymentsX96),
                Earnings::from_x96(cumls.shortUtilPaymentsX96),
            ),
            mark: mark.fair_price(),
        })
    }
}

/// One position's health inputs from its three rows, or `None` when the id
/// holds no open taker position at the block: a band is a maker whatever
/// its exposure, and a position with no exposure is closed, burned or
/// never minted. This is the role test `liquidateTaker` applies before
/// its health check, so an id the batch calls a taker is one the contract
/// would too.
fn taker_state(pos_id: U256, rows: &[IMulticall3::Result; 3]) -> Result<Option<TakerState>> {
    let [position_row, taker_row, maker_row] = rows;
    let position = decode_row::<Perp::positionsCall>(pos_id, position_row, "position")?;
    let details = decode_row::<Perp::takerDetailsCall>(pos_id, taker_row, "takerDetails")?;
    let maker = decode_row::<Perp::makerDetailsCall>(pos_id, maker_row, "makerDetails")?;
    let (perp, usd) = unpack_balance_delta(position.delta);
    if maker.liquidity != 0 || perp == 0 {
        return Ok(None);
    }
    Ok(Some(TakerState {
        delta_perp: PerpDelta::new(perp),
        delta_usd: UsdcDelta::new(usd),
        margin: UsdcAtoms::new(position.margin),
        liquidation_margin_ratio: Ratio::from_e6(u24_to_u32(position.liqMarginRatio))?,
        last_cuml_funding: Funding::from_x96(position.lastCumlFundingX96),
        last_util_payments: PerSide::new(
            Earnings::from_x96(details.lastLongUtilPaymentsX96),
            Earnings::from_x96(details.lastShortUtilPaymentsX96),
        ),
    }))
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{I256, Uint};

    use super::*;
    use crate::client::mock::{self, Rpc, returns, x96};
    use crate::contracts::{Cumulatives, IBeacon, Position, Rates, Taker};
    use crate::convert::pack_balance_delta;
    use crate::errors::{ContractError, PerpCityError};

    const TIMESTAMP: u64 = 1_700_000_000;

    async fn state() -> (StateAt, Rpc) {
        let (client, rpc) = mock::client();
        rpc.block(92, TIMESTAMP);
        let state = client.market().state_at(92).await.unwrap();
        (state, rpc)
    }

    /// The market-wide answers: the five views in one multicall at block
    /// 92 with every price 1.0, the market last touched at the block's
    /// timestamp and every cumulative at zero, then the beacon's index and
    /// the stored EMAs' word.
    fn market_answers(rpc: &Rpc) {
        let one = x96(1, 0);
        rpc.aggregate(
            92,
            [
                returns::<Perp::modulesCall>(&mock::modules()),
                returns::<Perp::poolStateCall>(&Perp::poolStateReturn {
                    sqrtPrice: Uint::from(1u8) << 96,
                    ..mock::pool_state(one)
                }),
                returns::<Perp::ratesCall>(&Rates {
                    lastTouch: Uint::from(TIMESTAMP),
                    ..mock::rates(0)
                }),
                returns::<Perp::EMA_WINDOWCall>(&U256::from(3_600u32)),
                returns::<Perp::cumulativesCall>(&Cumulatives {
                    fundingX96: I256::ZERO,
                    fundingDivSqrtPX96: I256::ZERO,
                    longUtilPaymentsX96: U256::ZERO,
                    shortUtilPaymentsX96: U256::ZERO,
                    longUtilEarningsX96: U256::ZERO,
                    shortUtilEarningsX96: U256::ZERO,
                }),
            ],
        );
        rpc.call::<IBeacon::indexCall>(&one);
        rpc.storage(mock::emas_word(one.to::<u128>(), one.to::<u128>()));
    }

    /// A position holding `perp` against `usd` with this margin and a 5%
    /// liquidation ratio, its checkpoints at zero.
    fn position(perp: i128, usd: i128, margin: u128) -> Position {
        Position {
            delta: pack_balance_delta(perp, usd),
            liqMarginRatio: mock::e6(50_000),
            ..mock::position(margin)
        }
    }

    fn taker_row() -> Vec<u8> {
        returns::<Perp::takerDetailsCall>(&Taker {
            lastLongUtilPaymentsX96: U256::ZERO,
            lastShortUtilPaymentsX96: U256::ZERO,
        })
    }

    /// The three rows of one id: its position, its taker checkpoints and
    /// a maker row with this liquidity.
    fn rows(position: &Position, liquidity: u128) -> [IMulticall3::Result; 3] {
        [
            mock::ok_row(returns::<Perp::positionsCall>(position)),
            mock::ok_row(taker_row()),
            mock::ok_row(returns::<Perp::makerDetailsCall>(&mock::maker(
                -60, 60, liquidity,
            ))),
        ]
    }

    /// The batch read must stay usable from spawned tasks: its future is
    /// Send. Compile-time regression test; no RPC is made.
    #[test]
    fn get_taker_healths_future_is_send() {
        fn require_send<T: Send>(_: &T) {}

        let (client, _rpc) = mock::client();
        let ids = [U256::ONE];
        let fut = client.market().get_taker_healths(&ids);
        require_send(&fut);
        drop(fut);
    }

    /// The role test is the contract's: a band is a maker whatever its
    /// exposure, no exposure is no position, and the rest are takers with
    /// their rows read into the state the health test takes.
    #[test]
    fn a_taker_is_an_exposure_with_no_liquidity() {
        let id = U256::from(7u8);
        let taker = taker_state(id, &rows(&position(1_000_000, -1_000_000, 100_000), 0))
            .unwrap()
            .expect("a long with no band is a taker");
        assert_eq!(taker.delta_perp, PerpDelta::new(1_000_000));
        assert_eq!(taker.delta_usd, UsdcDelta::new(-1_000_000));
        assert_eq!(taker.margin, UsdcAtoms::new(100_000));
        assert_eq!(taker.liquidation_margin_ratio.e6(), 50_000);
        assert!(
            taker_state(id, &rows(&position(1_000_000, -1_000_000, 100_000), 1_000))
                .unwrap()
                .is_none(),
            "a band is a maker"
        );
        assert!(
            taker_state(id, &rows(&position(0, 0, 0), 0))
                .unwrap()
                .is_none(),
            "no exposure is no position"
        );
    }

    /// Every id gets its outcome from one handle's block: the taker is
    /// computed from its rows at the block's mark, the reverted row fails
    /// alone and deterministically, the band and the empty id are not
    /// takers. The request sequence is the whole cost of the batch.
    #[tokio::test]
    async fn taker_healths_read_everything_at_the_handles_block() {
        let (state, rpc) = state().await;
        let pos_ids = [
            U256::from(11u8),
            U256::from(22u8),
            U256::from(33u8),
            U256::from(44u8),
        ];
        market_answers(&rpc);
        let [p11, t11, m11] = rows(&position(1_000_000, -1_000_000, 100_000), 0);
        let [p22, _, m22] = rows(&position(1_000_000, -1_000_000, 100_000), 0);
        let [p33, t33, m33] = rows(&position(0, 0, 1_000_000), 1_000);
        let [p44, t44, m44] = rows(&position(0, 0, 0), 0);
        rpc.aggregate3(vec![
            p11,
            t11,
            m11,
            p22,
            mock::failed_row(),
            m22,
            p33,
            t33,
            m33,
            p44,
            t44,
            m44,
        ]);

        let outcomes = state.taker_healths(&pos_ids).await.unwrap();
        assert_eq!(
            outcomes.iter().map(|o| o.pos_id).collect::<Vec<_>>(),
            pos_ids
        );
        let health = outcomes[0]
            .row
            .as_ref()
            .unwrap()
            .as_ref()
            .expect("pos 11 is an open taker");
        assert_eq!(health.block.number, 92);
        assert_eq!(health.position_value(), UsdcAtoms::new(1_000_000));
        assert_eq!(health.unrealized_pnl(), UsdcDelta::ZERO);
        assert_eq!(health.equity(), UsdcDelta::new(100_000));
        assert!(!health.is_liquidatable(), "10% margin against a 5% floor");
        let Err(err) = &outcomes[1].row else {
            panic!("pos 22's taker row reverted: {:?}", outcomes[1].row);
        };
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::MulticallFailed { .. })
            ),
            "{err}"
        );
        assert!(!err.is_transient(), "a reverted row reverts again");
        assert!(outcomes[2].row.as_ref().unwrap().is_none(), "a band");
        assert!(outcomes[3].row.as_ref().unwrap().is_none(), "never minted");
        assert!(
            rpc.is_drained(),
            "one multicall, the index, the EMAs' word, the rows"
        );
    }

    /// The market-wide read is the one read every outcome depends on, so
    /// its failure is the call's, typed for the block.
    #[tokio::test]
    async fn a_failed_market_read_fails_the_whole_call() {
        let (state, rpc) = state().await;
        rpc.fails("header not found");
        let err = state
            .taker_healths(&[U256::from(11u8)])
            .await
            .expect_err("the market-wide multicall failed");
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::BlockUnavailable { number: 92 })
            ),
            "{err}"
        );
        assert!(err.is_transient());
        assert!(rpc.is_drained());
    }
}
