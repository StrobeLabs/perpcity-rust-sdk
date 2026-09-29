//! A market's storage at one block, [`StateAt`].

use alloy::eips::BlockId;
use alloy::primitives::U256;

use crate::contracts::{IERC20, Perp, Position};
use crate::convert::usdc_from_atoms;
use crate::errors::{Result, ValidationError};
use crate::math::BlockContext;
use crate::math::range::MakerRange;
use crate::math::tick::get_sqrt_ratio_at_tick;
use crate::types::SolvencyState;

use super::i24_to_i32;
use super::market::MarketReader;

/// One market's storage, every read pinned to one block.
///
/// The handle is the block: it resolves the header once and pins each
/// read to that hash, so values read through one handle cannot come from
/// different blocks. [`MarketReader::state`] pins the lagged snapshot
/// block the other pinned reads use; [`MarketReader::state_at`] pins a
/// block the caller names, which past a recent window needs an archive
/// endpoint.
///
/// Owned and cheap to clone (the reader is Arc-backed), so a fan-out over
/// thousands of positions can move it into tasks. It is the state half of
/// what [`History`](crate::history::History) is for logs.
#[derive(Clone, Debug)]
pub struct StateAt {
    market: MarketReader,
    block: BlockContext,
}

impl MarketReader {
    /// State reads pinned to the lagged, reorg-safe snapshot block (see
    /// [`SNAPSHOT_BLOCK_LAG`](crate::constants::SNAPSHOT_BLOCK_LAG)).
    ///
    /// # Errors
    ///
    /// [`ContractError::BlockUnavailable`](crate::ContractError::BlockUnavailable)
    /// when the serving replica is missing the pinned header.
    pub async fn state(&self) -> Result<StateAt> {
        let (block, _) = self.chain.lagged_snapshot_block().await?;
        Ok(StateAt {
            market: self.clone(),
            block,
        })
    }

    /// State reads pinned to block `number`.
    ///
    /// The header is resolved here; the state behind it is only checked
    /// by the reads, so a full (non-archive) endpoint hands out the handle
    /// for an old block and then fails each read with the node's
    /// "historical state is not available" transport error.
    ///
    /// # Errors
    ///
    /// [`ContractError::BlockUnavailable`](crate::ContractError::BlockUnavailable)
    /// when the endpoint does not serve that header.
    pub async fn state_at(&self, number: u64) -> Result<StateAt> {
        let (block, _) = self.chain.block_at(number).await?;
        Ok(StateAt {
            market: self.clone(),
            block,
        })
    }
}

impl StateAt {
    /// The block every read on this handle is pinned to.
    pub fn block(&self) -> BlockContext {
        self.block
    }

    /// The market being read.
    pub fn market(&self) -> &MarketReader {
        &self.market
    }

    fn id(&self) -> BlockId {
        BlockId::hash(self.block.hash)
    }

    /// The market's solvency books.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] if a figure does not fit `i128` — a
    /// broken read, not a balance.
    pub async fn solvency(&self) -> Result<SolvencyState> {
        let perp = Perp::new(self.market.perp, self.market.chain.provider());
        let state = perp.solvencyState().block(self.id()).call().await?;
        Ok(SolvencyState {
            bad_debt: usdc_from_atoms(state.badDebt, "badDebt")?,
            total_margin: usdc_from_atoms(state.totalMargin, "totalMargin")?,
        })
    }

    /// Positions ever minted: ids `1..next_pos_id()` are the range a scan
    /// of the market covers.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] if the count does not fit `u64` — a
    /// broken read, since no market has minted that many.
    pub async fn next_pos_id(&self) -> Result<u64> {
        let perp = Perp::new(self.market.perp, self.market.chain.provider());
        let next = perp.nextPosId().block(self.id()).call().await?;
        u64::try_from(next).map_err(|_| {
            ValidationError::Overflow {
                context: format!("nextPosId {next} exceeds u64"),
            }
            .into()
        })
    }

    /// One position's raw contract state, or `None` for an id that was
    /// never minted or has been closed (the contract reports both as an
    /// empty struct).
    ///
    /// `None` rather than an error because this is the read for
    /// enumerating `1..next_pos_id()`, where most ids are closed;
    /// [`MarketReader::get_position`] asks about a position expected to
    /// exist and fails with `PositionNotFound` instead.
    pub async fn position(&self, pos_id: U256) -> Result<Option<Position>> {
        let perp = Perp::new(self.market.perp, self.market.chain.provider());
        let position = perp.positions(pos_id).block(self.id()).call().await?;
        Ok((position.margin != 0 || !position.delta.is_zero()).then_some(position))
    }

    /// One position's liquidity range, or `None` if it holds none there:
    /// a taker, or a maker whose liquidity is gone.
    ///
    /// The ticks are validated against the V4 domain so tick math on the
    /// result cannot fail on chain-supplied values.
    pub async fn maker_range(&self, pos_id: U256) -> Result<Option<MakerRange>> {
        let perp = Perp::new(self.market.perp, self.market.chain.provider());
        let maker = perp.makerDetails(pos_id).block(self.id()).call().await?;
        if maker.liquidity == 0 {
            return Ok(None);
        }
        let range = MakerRange::new(
            i24_to_i32(maker.tickLower),
            i24_to_i32(maker.tickUpper),
            maker.liquidity,
        );
        get_sqrt_ratio_at_tick(range.tick_lower)?;
        get_sqrt_ratio_at_tick(range.tick_upper)?;
        Ok(Some(range))
    }

    /// The pool's current tick.
    pub async fn pool_tick(&self) -> Result<i32> {
        let perp = Perp::new(self.market.perp, self.market.chain.provider());
        let state = perp.poolState().block(self.id()).call().await?;
        Ok(i24_to_i32(state.tick))
    }

    /// The USDC the perp holds, which backs its positions' margin and also
    /// its insurance fund and uncollected fees.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] if the balance does not fit `i128`.
    pub async fn collateral(&self) -> Result<f64> {
        let chain = &self.market.chain;
        let raw: U256 = IERC20::new(chain.deployments().usdc, chain.provider())
            .balanceOf(self.market.perp)
            .block(self.id())
            .call()
            .await?;
        Ok(usdc_from_atoms(raw, "collateral")?)
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::I256;

    use super::*;
    use crate::client::mock::{self, PERP, Rpc};
    use crate::constants::SNAPSHOT_BLOCK_LAG;
    use crate::errors::{ContractError, PerpCityError};

    const TIMESTAMP: u64 = 1_700_000_000;

    /// A handle pinned to block 92 by name, over a mocked reader.
    async fn state() -> (StateAt, Rpc) {
        let (client, rpc) = mock::client();
        rpc.block(92, TIMESTAMP);
        let state = client.market().state_at(92).await.unwrap();
        (state, rpc)
    }

    #[tokio::test]
    async fn state_pins_the_lagged_snapshot_block() {
        let (client, rpc) = mock::client();
        rpc.quantity(100);
        let hash = rpc.block(100 - SNAPSHOT_BLOCK_LAG, TIMESTAMP);

        let state = client.market().state().await.unwrap();
        assert_eq!(
            state.block(),
            BlockContext {
                number: 100 - SNAPSHOT_BLOCK_LAG,
                hash,
                timestamp: TIMESTAMP,
            }
        );
        assert_eq!(state.market().perp(), PERP);
        assert!(rpc.is_drained(), "blockNumber and the header, nothing else");
    }

    #[tokio::test]
    async fn state_at_pins_the_named_block_without_asking_for_the_head() {
        let (state, rpc) = state().await;
        assert_eq!(state.block().number, 92);
        assert!(rpc.is_drained(), "the header alone");
    }

    #[tokio::test]
    async fn a_missing_header_is_a_failed_read_not_the_head() {
        let (client, rpc) = mock::client();
        rpc.no_block();
        let Err(PerpCityError::Contract(ContractError::BlockUnavailable { number })) =
            client.market().state_at(7).await
        else {
            panic!("an absent header must fail the handle");
        };
        assert_eq!(number, 7);
    }

    #[tokio::test]
    async fn solvency_is_scaled_to_usdc() {
        let (state, rpc) = state().await;
        rpc.call::<Perp::solvencyStateCall>(&mock::solvency(1_500_000, 250_000_000));
        assert_eq!(
            state.solvency().await.unwrap(),
            SolvencyState {
                bad_debt: 1.5,
                total_margin: 250.0,
            }
        );
    }

    #[tokio::test]
    async fn next_pos_id_is_a_count_and_refuses_a_broken_one() {
        let (state, rpc) = state().await;
        rpc.call::<Perp::nextPosIdCall>(&U256::from(1_717));
        assert_eq!(state.next_pos_id().await.unwrap(), 1_717);

        rpc.call::<Perp::nextPosIdCall>(&U256::from(u64::MAX).saturating_add(U256::from(1)));
        let Err(PerpCityError::Validation(ValidationError::Overflow { .. })) =
            state.next_pos_id().await
        else {
            panic!("a count past u64 is a broken read, not a saturated one");
        };
    }

    /// `None` for the empty struct the contract returns for closed and
    /// never-minted ids, where `get_position` fails — the two callers
    /// want different things from the same bytes.
    #[tokio::test]
    async fn an_empty_position_is_none_here_and_not_found_there() {
        let (state, rpc) = state().await;
        rpc.call::<Perp::positionsCall>(&mock::position(0));
        assert!(state.position(U256::from(3)).await.unwrap().is_none());

        rpc.call::<Perp::positionsCall>(&mock::position(5_000_000));
        let position = state.position(U256::from(4)).await.unwrap().unwrap();
        assert_eq!(position.margin, 5_000_000);

        rpc.call::<Perp::positionsCall>(&mock::position(0));
        let Err(PerpCityError::Contract(ContractError::PositionNotFound { pos_id })) =
            state.market().get_position(U256::from(3)).await
        else {
            panic!("get_position keeps its error");
        };
        assert_eq!(pos_id, U256::from(3));
    }

    /// A position with exposure but no margin is still a position: the
    /// contract's empty struct has both zero.
    #[tokio::test]
    async fn a_position_with_exposure_and_no_margin_is_present() {
        let (state, rpc) = state().await;
        let mut exposed = mock::position(0);
        exposed.delta = I256::try_from(-7).unwrap();
        rpc.call::<Perp::positionsCall>(&exposed);
        assert!(state.position(U256::from(9)).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn maker_range_is_none_without_liquidity() {
        let (state, rpc) = state().await;
        rpc.call::<Perp::makerDetailsCall>(&mock::maker(-60, 60, 0));
        assert_eq!(state.maker_range(U256::from(1)).await.unwrap(), None);

        rpc.call::<Perp::makerDetailsCall>(&mock::maker(38_340, 38_430, 97_506_535));
        assert_eq!(
            state.maker_range(U256::from(1_691)).await.unwrap(),
            Some(MakerRange::new(38_340, 38_430, 97_506_535))
        );
    }

    #[tokio::test]
    async fn pool_tick_is_the_slot_tick() {
        let (state, rpc) = state().await;
        rpc.call::<Perp::poolStateCall>(&mock::pool_state_at_tick(38_340));
        assert_eq!(state.pool_tick().await.unwrap(), 38_340);
    }

    #[tokio::test]
    async fn collateral_is_the_perps_usdc_balance() {
        let (state, rpc) = state().await;
        rpc.call::<IERC20::balanceOfCall>(&U256::from(3_265_080_000u64));
        assert_eq!(state.collateral().await.unwrap(), 3_265.08);
        assert!(rpc.is_drained());
    }
}
