//! Reads of the market at one block, [`StateAt`].
//!
//! The counterpart of [`queries`](super::queries): there, every read is
//! independently current; here, a handle resolves one block and every read
//! on it is pinned to that block's hash. Two values that must agree — open
//! interest against capacity, a position against the mark — are read
//! through one handle, and agree by construction.

use std::collections::BTreeMap;

use alloy::eips::BlockId;
use alloy::primitives::{B256, U256};
use alloy::providers::{MulticallError, Provider};
use alloy::transports::RpcError;

use crate::constants::{MAX_SWAP_SQRT_PRICE_X96, MAX_TICK, MIN_SWAP_SQRT_PRICE_X96, MIN_TICK};
use crate::contracts::{
    IBeacon, IERC20, IMarginRatios, IPoolManagerState, IPriceImpact, Perp, Position,
};
use crate::convert::usdc_from_atoms;
use crate::errors::{ContractError, PerpCityError, Result, ValidationError};
use crate::math::BlockContext;
use crate::math::capacity::MarketCapacity;
use crate::math::ema::{PricePair, calculate_emas};
use crate::math::range::{MakerBand, TickRange};
use crate::math::swap::{TakerMarketSnapshot, TickLiquidity};
use crate::storage::{perp_emas_slot, v4_tick_bitmap_slot, v4_tick_slot};
use crate::types::{MarginRatioTriple, MarginRatios, SolvencyState};

use super::market::MarketReader;
use super::queries::{BookImmutables, multicall_error, registered_module};
use super::{i24_to_i32, u24_to_u32};

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
    /// [`ContractError::BlockUnavailable`] when the serving replica is
    /// missing the pinned header.
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
    /// by the reads. A full (non-archive) endpoint keeps every header but
    /// prunes old state, so it hands out the handle and then fails each
    /// read with [`ContractError::StateUnavailable`], which is not
    /// transient: the fix is an archive endpoint, not a retry.
    ///
    /// # Errors
    ///
    /// [`ContractError::BlockUnavailable`] when the endpoint does not
    /// serve that header.
    pub async fn state_at(&self, number: u64) -> Result<StateAt> {
        let (block, _) = self.chain.block_at(number).await?;
        Ok(StateAt {
            market: self.clone(),
            block,
        })
    }

    // ── One pinned read, on a fresh lagged handle ─────────────────────
    //
    // Each of these is `state().await?.<read>()`: the single-read
    // convenience. Anything that reads twice and needs the values to
    // agree takes `state()` once and reads on it.

    /// [`StateAt::capacity`] at the lagged snapshot block.
    pub async fn get_capacity(&self) -> Result<MarketCapacity> {
        self.state().await?.capacity().await
    }

    /// [`StateAt::margin_ratios`] at the lagged snapshot block.
    pub async fn get_margin_ratios(&self) -> Result<MarginRatios> {
        self.state().await?.margin_ratios().await
    }

    /// [`StateAt::taker_book`] at the lagged snapshot block.
    ///
    /// The deployment immutables are read first: they are not the block's
    /// to give, and a market whose immutables are rejected never resolves
    /// a block at all.
    pub async fn load_taker_market_snapshot(&self) -> Result<TakerMarketSnapshot> {
        self.book_immutables().await?;
        self.state().await?.taker_book().await
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

    /// A read's failure, with the node's "state pruned" answer typed as
    /// [`ContractError::StateUnavailable`] for this handle's block.
    fn read_error(&self, error: alloy::contract::Error) -> PerpCityError {
        match &error {
            alloy::contract::Error::TransportError(RpcError::ErrorResp(payload))
                if state_pruned(&payload.message) =>
            {
                ContractError::StateUnavailable {
                    number: self.block.number,
                }
                .into()
            }
            _ => error.into(),
        }
    }

    /// The market's solvency books.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] if a figure does not fit `i128` — a
    /// broken read, not a balance.
    pub async fn solvency(&self) -> Result<SolvencyState> {
        let perp = Perp::new(self.market.perp, self.market.chain.provider());
        let state = perp
            .solvencyState()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
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
        let next = perp
            .nextPosId()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
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
        let position = perp
            .positions(pos_id)
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        Ok((position.margin != 0 || !position.delta.is_zero()).then_some(position))
    }

    /// One position's band, or `None` if it holds no liquidity there: a
    /// taker, or a maker whose liquidity is gone.
    ///
    /// The chain's ticks pass through [`TickRange::new`], so this is where
    /// a malformed range would fail — never the math downstream.
    pub async fn maker_band(&self, pos_id: U256) -> Result<Option<MakerBand>> {
        let perp = Perp::new(self.market.perp, self.market.chain.provider());
        let maker = perp
            .makerDetails(pos_id)
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        if maker.liquidity == 0 {
            return Ok(None);
        }
        let range = TickRange::new(i24_to_i32(maker.tickLower), i24_to_i32(maker.tickUpper))?;
        Ok(Some(MakerBand::new(range, maker.liquidity)))
    }

    /// The pool's current tick.
    pub async fn pool_tick(&self) -> Result<i32> {
        let perp = Perp::new(self.market.perp, self.market.chain.provider());
        let state = perp
            .poolState()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
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
            .await
            .map_err(|e| self.read_error(e))?;
        Ok(usdc_from_atoms(raw, "collateral")?)
    }

    /// The market's taker capacity and the open interest drawing on it, in
    /// one multicall.
    ///
    /// [`MarketCapacity`] derives each side's headroom and utilization as
    /// the contract computes them.
    pub async fn capacity(&self) -> Result<MarketCapacity> {
        let perp = Perp::new(self.market.perp, self.market.chain.provider());
        let (capacity, oi) = self
            .market
            .chain
            .multicall_at(self.id())
            .add(perp.capacity())
            .add(perp.openInterest())
            .aggregate()
            .await
            .map_err(|e| self.multicall_read_error(e))?;
        Ok(MarketCapacity {
            block: self.block,
            capacity: capacity.into(),
            long_open_interest_atoms: oi.long,
            short_open_interest_atoms: oi.short,
        })
    }

    /// The `IMarginRatios` module's maker and taker init / liquidation /
    /// backstop thresholds, as fractions.
    ///
    /// `modules()` and the two getters are read at this block, so a
    /// governance module swap or ratio update cannot straddle the result.
    /// Verified live 2026-09-07 on HORMUZ-TRAFFIC's module
    /// `0x8afca53c52b1f02d76aefb811c6b08f4bd3e4cf9` (Arbitrum One):
    /// `makerMarginRatios()` = (1000000, 900000, 800000), i.e.
    /// 1.0 / 0.9 / 0.8; `takerMarginRatios()` = (100000, 50000, 20000),
    /// i.e. 0.1 / 0.05 / 0.02.
    ///
    /// # Errors
    ///
    /// [`ContractError::ModuleNotRegistered`] when `modules().marginRatios`
    /// is the zero address.
    pub async fn margin_ratios(&self) -> Result<MarginRatios> {
        let provider = self.market.chain.provider();
        let perp = Perp::new(self.market.perp, provider);
        let modules = perp
            .modules()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let ratios = IMarginRatios::new(
            registered_module(modules.marginRatios, "IMarginRatios")?,
            provider,
        );
        let maker_call = ratios.makerMarginRatios().block(self.id());
        let taker_call = ratios.takerMarginRatios().block(self.id());
        let (maker, taker) = tokio::try_join!(maker_call.call(), taker_call.call())
            .map_err(|e| self.read_error(e))?;
        Ok(MarginRatios {
            maker: MarginRatioTriple::from_e6(
                u24_to_u32(maker.init),
                u24_to_u32(maker.liq),
                u24_to_u32(maker.backstop),
            ),
            taker: MarginRatioTriple::from_e6(
                u24_to_u32(taker.init),
                u24_to_u32(taker.liq),
                u24_to_u32(taker.backstop),
            ),
        })
    }

    /// The taker book: an exact concentrated-liquidity snapshot of the
    /// deployed Perp (`perpcity-contracts@4bbe554f`) at this block.
    ///
    /// Reads the stored EMA pair from its storage slot, advances it with
    /// the contract's exact arithmetic, evaluates the configured
    /// price-impact module, and rebuilds the tick map from the
    /// PoolManager's bitmap. Every contract and PoolManager read is pinned
    /// to this block's hash.
    ///
    /// # Errors
    ///
    /// [`ContractError::ModuleNotRegistered`] when the perp has no beacon
    /// or no price-impact module.
    pub async fn taker_book(&self) -> Result<TakerMarketSnapshot> {
        let immutables = self.market.book_immutables().await?;
        let provider = self.market.chain.provider();
        let perp = Perp::new(self.market.perp, provider);
        let stored_emas = provider
            .get_storage_at(self.market.perp, perp_emas_slot())
            .block_id(self.id())
            .await
            .map_err(|e| self.read_error(e.into()))?;
        let pool_state_call = perp.poolState().block(self.id());
        let modules_call = perp.modules().block(self.id());
        let rates_call = perp.rates().block(self.id());
        let (state, modules, rates) = tokio::try_join!(
            pool_state_call.call(),
            modules_call.call(),
            rates_call.call(),
        )
        .map_err(|e| self.read_error(e))?;
        let beacon = registered_module(modules.beacon, "IBeacon")?;
        let index = IBeacon::new(beacon, provider)
            .index()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let stored = PricePair {
            amm: (stored_emas & U256::from(u128::MAX)).to::<u128>(),
            index: (stored_emas >> 128usize).to::<u128>(),
        };
        let spot = PricePair::try_from_x96(state.ammPrice, index)?;
        let emas = calculate_emas(
            stored,
            spot,
            rates.lastTouch.to::<u64>(),
            self.block.timestamp,
            immutables.ema_window,
        )?;
        let price_impact = registered_module(modules.priceImpact, "IPriceImpact")?;
        let bounds = IPriceImpact::new(price_impact, provider)
            .sqrtPriceBounds(
                state.ammPrice,
                index,
                U256::from(emas.amm),
                U256::from(emas.index),
            )
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        let header = TakerMarketSnapshot {
            block: self.block,
            sqrt_price_x96: state.sqrtPrice.to::<U256>(),
            tick: i24_to_i32(state.tick),
            liquidity: state.liquidity,
            ticks: BTreeMap::new(),
            protocol_sqrt_min_x96: MIN_SWAP_SQRT_PRICE_X96,
            protocol_sqrt_max_x96: MAX_SWAP_SQRT_PRICE_X96,
            impact_sqrt_min_x96: bounds.sqrtMin,
            impact_sqrt_max_x96: bounds.sqrtMax,
        };
        self.fill_book(header, immutables).await
    }

    /// Populate `snapshot.ticks` from the PoolManager's tick bitmap at this
    /// block, then verify the reconstruction against the pool's reported
    /// active liquidity before returning it.
    async fn fill_book(
        &self,
        mut snapshot: TakerMarketSnapshot,
        immutables: BookImmutables,
    ) -> Result<TakerMarketSnapshot> {
        let BookImmutables {
            pool_id,
            tick_spacing: spacing,
            ..
        } = immutables;
        let chain = &self.market.chain;

        let min_word = MIN_TICK.div_euclid(spacing).div_euclid(256);
        let max_word = MAX_TICK.div_euclid(spacing).div_euclid(256);
        let bitmap_slots: Vec<B256> = (min_word..=max_word)
            .map(|word| B256::from(v4_tick_bitmap_slot(pool_id, word)))
            .collect();
        let manager = IPoolManagerState::new(chain.deployments().pool_manager, chain.provider());
        let bitmaps = manager
            .extsload_1(bitmap_slots)
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;

        let mut initialized = Vec::new();
        for (offset, bitmap) in bitmaps.into_iter().enumerate() {
            let word = min_word + offset as i32;
            let bits = U256::from_be_bytes(bitmap.0);
            for bit in 0..256i32 {
                if bits.bit(bit as usize) {
                    let compressed = word * 256 + bit;
                    let initialized_tick = compressed * spacing;
                    if (MIN_TICK..=MAX_TICK).contains(&initialized_tick) {
                        initialized.push(initialized_tick);
                    }
                }
            }
        }

        let tick_slots: Vec<B256> = initialized
            .iter()
            .map(|&initialized_tick| B256::from(v4_tick_slot(pool_id, initialized_tick)))
            .collect();
        let tick_words = if tick_slots.is_empty() {
            Vec::new()
        } else {
            manager
                .extsload_1(tick_slots)
                .block(self.id())
                .call()
                .await
                .map_err(|e| self.read_error(e))?
        };
        let mut ticks = BTreeMap::new();
        for (initialized_tick, word) in initialized.into_iter().zip(tick_words) {
            let raw = U256::from_be_bytes(word.0);
            let gross = (raw & U256::from(u128::MAX)).to::<u128>();
            let net_raw = (raw >> 128usize).to::<u128>();
            let net = net_raw as i128;
            ticks.insert(initialized_tick, TickLiquidity { gross, net });
        }

        let reconstructed = ticks
            .range(..=snapshot.tick)
            .try_fold(0u128, |active, (_, info)| {
                if info.net >= 0 {
                    active.checked_add(info.net as u128)
                } else {
                    active.checked_sub(info.net.unsigned_abs())
                }
            })
            .ok_or_else(|| ValidationError::Overflow {
                context: "reconstructing active liquidity".into(),
            })?;
        if reconstructed != snapshot.liquidity {
            return Err(ContractError::MulticallFailed {
                reason: format!(
                    "tick snapshot liquidity mismatch: reconstructed {reconstructed}, pool {}",
                    snapshot.liquidity
                ),
            }
            .into());
        }

        snapshot.ticks = ticks;
        Ok(snapshot)
    }

    /// A multicall's failure, with the node's "state pruned" answer typed
    /// as [`ContractError::StateUnavailable`] for this handle's block; every
    /// other failure classified as [`multicall_error`] does.
    fn multicall_read_error(&self, error: MulticallError) -> PerpCityError {
        match error {
            MulticallError::TransportError(RpcError::ErrorResp(payload))
                if state_pruned(&payload.message) =>
            {
                ContractError::StateUnavailable {
                    number: self.block.number,
                }
                .into()
            }
            other => multicall_error(other),
        }
    }
}

/// Whether a node's error message is its refusal to serve pruned state:
/// Nitro's "historical state … is not available", geth's "missing trie
/// node". The message is the only place a node says so.
fn state_pruned(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("historical state") || message.contains("missing trie node")
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, I256, Uint};

    use super::*;
    use crate::client::mock::{self, PERP, Rpc, e6, returns, x96};
    use crate::constants::SNAPSHOT_BLOCK_LAG;
    use crate::contracts::{Modules, Rates};
    use crate::math::capacity::Capacity;

    const TIMESTAMP: u64 = 1_700_000_000;
    /// The market's tick spacing in these tests.
    const SPACING: i32 = 30;

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

    /// A full node keeps the header and prunes the state: the handle is
    /// handed out and the read names the condition, which no retry fixes.
    #[tokio::test]
    async fn pruned_state_is_a_typed_permanent_failure() {
        for message in [
            "historical state 1682db2b34068813264a4d89e2d8d02b0703b6f0e6acd8437d8080d98a5db56b is not available",
            "missing trie node 0x1682db2b (path ) state 0x1682db2b is not available",
        ] {
            let (state, rpc) = state().await;
            rpc.fails(message);
            let error = state.solvency().await.unwrap_err();
            let PerpCityError::Contract(ContractError::StateUnavailable { number }) = error else {
                panic!("{message:?} must type as StateUnavailable, got {error}");
            };
            assert_eq!(number, 92);
            assert!(
                !error.is_transient(),
                "an archive endpoint is the fix, not a retry"
            );
        }
    }

    /// Any other node failure keeps its own shape.
    #[tokio::test]
    async fn other_read_failures_pass_through() {
        let (state, rpc) = state().await;
        rpc.fails("execution aborted (timeout = 5s)");
        assert!(matches!(
            state.pool_tick().await.unwrap_err(),
            PerpCityError::Abi(_)
        ));
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
    async fn maker_band_is_none_without_liquidity() {
        let (state, rpc) = state().await;
        rpc.call::<Perp::makerDetailsCall>(&mock::maker(-60, 60, 0));
        assert_eq!(state.maker_band(U256::from(1)).await.unwrap(), None);

        rpc.call::<Perp::makerDetailsCall>(&mock::maker(38_340, 38_430, 97_506_535));
        assert_eq!(
            state.maker_band(U256::from(1_691)).await.unwrap(),
            Some(MakerBand::new(
                TickRange::new(38_340, 38_430).unwrap(),
                97_506_535
            ))
        );
    }

    /// A range the contract could never store still cannot reach the
    /// math: it fails here, as the chain boundary.
    #[tokio::test]
    async fn a_malformed_range_fails_at_the_read() {
        let (state, rpc) = state().await;
        rpc.call::<Perp::makerDetailsCall>(&mock::maker(60, -60, 5));
        assert!(matches!(
            state.maker_band(U256::from(2)).await,
            Err(PerpCityError::Validation(
                ValidationError::InvalidTickRange { .. }
            ))
        ));
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

    // ── One handle, one block ─────────────────────────────────────────

    /// The `IMarginRatios` module's two triples, as the mock serves them.
    fn margin_ratio_answers(rpc: &Rpc) {
        rpc.call::<Perp::modulesCall>(&mock::modules());
        rpc.call::<IMarginRatios::makerMarginRatiosCall>(&IMarginRatios::makerMarginRatiosReturn {
            init: e6(1_000_000),
            liq: e6(900_000),
            backstop: e6(800_000),
        });
        rpc.call::<IMarginRatios::takerMarginRatiosCall>(&IMarginRatios::takerMarginRatiosReturn {
            init: e6(100_000),
            liq: e6(50_000),
            backstop: e6(20_000),
        });
    }

    /// The point of the handle: several reads resolve the header once
    /// and every result is from that block.
    #[tokio::test]
    async fn one_handle_resolves_the_header_once_for_every_read() {
        let (state, rpc) = state().await;
        rpc.aggregate(
            92,
            [
                returns::<Perp::capacityCall>(&mock::capacity(10, 20)),
                returns::<Perp::openInterestCall>(&mock::open_interest(3, 4)),
            ],
        );
        margin_ratio_answers(&rpc);

        let capacity = state.capacity().await.unwrap();
        let ratios = state.margin_ratios().await.unwrap();
        assert_eq!(capacity.block, state.block());
        assert_eq!(capacity.block.number, 92);
        assert_eq!(ratios.maker.init, 1.0);
        assert!(
            rpc.is_drained(),
            "the header once at construction, then one multicall and three calls"
        );
    }

    /// A pruned block fails a multicall read the same way it fails a
    /// single call: typed, and not transient.
    #[tokio::test]
    async fn pruned_state_fails_a_multicall_read_the_same_way() {
        let (state, rpc) = state().await;
        rpc.fails("historical state 1682db2b is not available");

        let error = state.capacity().await.unwrap_err();
        let PerpCityError::Contract(ContractError::StateUnavailable { number }) = error else {
            panic!("a multicall over pruned state must type as StateUnavailable, got {error}");
        };
        assert_eq!(number, 92);
        assert!(!error.is_transient());
    }

    // ── The single-read conveniences: a fresh lagged handle each ──────

    /// Both triples come back as fractions. The two reads share one ABI
    /// shape, so distinct values are what pin their order.
    #[tokio::test]
    async fn margin_ratios_decode_both_triples_as_fractions() {
        let (client, rpc) = mock::client();
        rpc.quantity(100);
        rpc.block(100 - SNAPSHOT_BLOCK_LAG, TIMESTAMP);
        margin_ratio_answers(&rpc);

        let ratios = client.market().get_margin_ratios().await.unwrap();
        assert_eq!(
            (
                ratios.maker.init,
                ratios.maker.liquidation,
                ratios.maker.backstop
            ),
            (1.0, 0.9, 0.8)
        );
        assert_eq!(
            (
                ratios.taker.init,
                ratios.taker.liquidation,
                ratios.taker.backstop
            ),
            (0.1, 0.05, 0.02)
        );
        assert!(
            rpc.is_drained(),
            "blockNumber, block, modules, maker, taker"
        );
    }

    /// The pinned block is the head less the lag, and a replica that has
    /// no header for it is a failed read that names the block — and is
    /// transient, since the replica will catch up.
    #[tokio::test]
    async fn a_missing_lagged_header_is_block_unavailable_at_head_minus_lag() {
        let (client, rpc) = mock::client();
        rpc.quantity(100);
        rpc.no_block();

        let err = client.market().get_margin_ratios().await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::BlockUnavailable { number })
                    if number == 100 - SNAPSHOT_BLOCK_LAG
            ),
            "{err}"
        );
        assert!(err.is_transient());
        assert!(
            rpc.is_drained(),
            "nothing is read at a block that is missing"
        );
    }

    /// A market with no ratios module fails after the modules read, not
    /// with an opaque decode error from the zero address.
    #[tokio::test]
    async fn margin_ratios_without_a_module_fail_after_the_modules_read() {
        let (client, rpc) = mock::client();
        rpc.quantity(100);
        rpc.block(100 - SNAPSHOT_BLOCK_LAG, TIMESTAMP);
        rpc.call::<Perp::modulesCall>(&Modules {
            marginRatios: Address::ZERO,
            ..mock::modules()
        });

        let err = client.market().get_margin_ratios().await.unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::ModuleNotRegistered { ref module })
                    if module == "IMarginRatios"
            ),
            "{err}"
        );
        assert!(rpc.is_drained());
    }

    /// Capacity and open interest are read in one batch at the lagged
    /// block, and the result carries that block.
    #[tokio::test]
    async fn capacity_carries_the_lagged_block_it_was_read_at() {
        let (client, rpc) = mock::client();
        rpc.quantity(100);
        let hash = rpc.block(100 - SNAPSHOT_BLOCK_LAG, TIMESTAMP);
        rpc.aggregate(
            100 - SNAPSHOT_BLOCK_LAG,
            [
                returns::<Perp::capacityCall>(&mock::capacity(10, 20)),
                returns::<Perp::openInterestCall>(&mock::open_interest(3, 4)),
            ],
        );

        assert_eq!(
            client.market().get_capacity().await.unwrap(),
            MarketCapacity {
                block: BlockContext {
                    number: 100 - SNAPSHOT_BLOCK_LAG,
                    hash,
                    timestamp: TIMESTAMP,
                },
                capacity: Capacity {
                    long_atoms: 10,
                    short_atoms: 20,
                },
                long_open_interest_atoms: 3,
                short_open_interest_atoms: 4,
            }
        );
        assert!(rpc.is_drained(), "blockNumber, block, one multicall");
    }

    // ── The taker book: immutables, pinned views, and the bitmap ──────

    /// The pool's id, as `POOL_ID()` reports it.
    const POOL_ID: B256 = B256::repeat_byte(0x99);
    /// When the market was last touched, and the block time in these
    /// tests, so the stored EMAs need no advancing.
    const TOUCHED_AT: u64 = TIMESTAMP;
    /// Bitmap words the book walk covers at this spacing:
    /// `MIN_TICK.div_euclid(30) = -4606`, `.div_euclid(256) = -18`, up to
    /// `4606.div_euclid(256) = 17` — 36 words, word `w` at offset `w + 18`.
    const BITMAP_WORDS: usize = 36;

    /// The three immutables the first snapshot reads, in `try_join!` order.
    fn immutables_answers(rpc: &Rpc) {
        rpc.call::<Perp::POOL_IDCall>(&POOL_ID);
        rpc.call::<Perp::poolKeyCall>(&mock::pool_key(SPACING));
        rpc.call::<Perp::EMA_WINDOWCall>(&U256::from(3_600u32));
    }

    /// Everything after the immutables: the lagged block, the stored
    /// EMAs, the three pinned `Perp` views, the beacon, the impact bounds,
    /// then the bitmap and — if any tick is set — the tick words. Pool,
    /// index and EMAs are all 1.0; returns the pinned block's hash.
    fn snapshot_answers(
        rpc: &Rpc,
        liquidity: u128,
        bitmap: Vec<B256>,
        tick_words: Option<Vec<B256>>,
    ) -> B256 {
        let one = x96(1, 0);
        rpc.quantity(100);
        let hash = rpc.block(100 - SNAPSHOT_BLOCK_LAG, TOUCHED_AT);
        rpc.storage((one << 128) | one);
        rpc.call::<Perp::poolStateCall>(&Perp::poolStateReturn {
            sqrtPrice: Uint::from(1u8) << 96,
            liquidity,
            ..mock::pool_state(one)
        });
        rpc.call::<Perp::modulesCall>(&mock::modules());
        rpc.call::<Perp::ratesCall>(&Rates {
            lastTouch: Uint::from(TOUCHED_AT),
            ..mock::rates(0)
        });
        rpc.call::<IBeacon::indexCall>(&one);
        rpc.call::<IPriceImpact::sqrtPriceBoundsCall>(&IPriceImpact::sqrtPriceBoundsReturn {
            sqrtMin: one >> 1,
            sqrtMax: one << 1,
        });
        rpc.call::<IPoolManagerState::extsload_1Call>(&bitmap);
        if let Some(words) = tick_words {
            rpc.call::<IPoolManagerState::extsload_1Call>(&words);
        }
        hash
    }

    /// A bitmap with these `(offset, bit)`s set.
    fn bitmap(set: &[(usize, usize)]) -> Vec<B256> {
        let mut words = vec![U256::ZERO; BITMAP_WORDS];
        for &(offset, bit) in set {
            words[offset] |= U256::from(1u8) << bit;
        }
        words.into_iter().map(B256::from).collect()
    }

    /// A tick's storage word: `liquidityNet` in the high half,
    /// `liquidityGross` in the low.
    fn tick_word(gross: u128, net: i128) -> B256 {
        B256::from((U256::from(net as u128) << 128) | U256::from(gross))
    }

    fn expected_snapshot(hash: B256, liquidity: u128) -> TakerMarketSnapshot {
        let one = x96(1, 0);
        TakerMarketSnapshot {
            block: BlockContext {
                number: 100 - SNAPSHOT_BLOCK_LAG,
                hash,
                timestamp: TOUCHED_AT,
            },
            sqrt_price_x96: one,
            tick: 0,
            liquidity,
            ticks: BTreeMap::new(),
            protocol_sqrt_min_x96: MIN_SWAP_SQRT_PRICE_X96,
            protocol_sqrt_max_x96: MAX_SWAP_SQRT_PRICE_X96,
            impact_sqrt_min_x96: one >> 1,
            impact_sqrt_max_x96: one << 1,
        }
    }

    /// An empty bitmap means no tick words are asked for, and the empty
    /// book reconciles with a pool holding no liquidity.
    #[tokio::test]
    async fn taker_snapshot_of_an_empty_book_skips_the_tick_words() {
        let (client, rpc) = mock::client();
        immutables_answers(&rpc);
        let hash = snapshot_answers(&rpc, 0, bitmap(&[]), None);

        assert_eq!(
            client.market().load_taker_market_snapshot().await.unwrap(),
            expected_snapshot(hash, 0)
        );
        assert!(rpc.is_drained(), "twelve answers, none left over");
    }

    /// Set bits become ticks at `compressed * spacing`, their words are
    /// read in bitmap order, and the net liquidity of every tick at or
    /// below the pool's must add up to what the pool reports.
    #[tokio::test]
    async fn taker_snapshot_rebuilds_the_book_from_the_bitmap() {
        const L: u128 = 1_000;
        let (client, rpc) = mock::client();
        immutables_answers(&rpc);
        // Tick -60 is compressed -2: word -1 (offset 17), bit 254.
        // Tick 60 is compressed 2: word 0 (offset 18), bit 2.
        let hash = snapshot_answers(
            &rpc,
            L,
            bitmap(&[(17, 254), (18, 2)]),
            Some(vec![tick_word(L, L as i128), tick_word(L, -(L as i128))]),
        );

        let snapshot = client.market().load_taker_market_snapshot().await.unwrap();
        assert_eq!(
            snapshot,
            TakerMarketSnapshot {
                ticks: BTreeMap::from([
                    (
                        -60,
                        TickLiquidity {
                            gross: L,
                            net: L as i128
                        }
                    ),
                    (
                        60,
                        TickLiquidity {
                            gross: L,
                            net: -(L as i128)
                        }
                    ),
                ]),
                ..expected_snapshot(hash, L)
            }
        );
        assert!(rpc.is_drained(), "thirteen answers");
    }

    /// A book whose ticks do not add up to the pool's active liquidity is
    /// a wrong read, not a snapshot.
    #[tokio::test]
    async fn a_book_that_does_not_reconcile_with_the_pool_is_rejected() {
        const L: u128 = 1_000;
        let (client, rpc) = mock::client();
        immutables_answers(&rpc);
        snapshot_answers(
            &rpc,
            L + 1,
            bitmap(&[(17, 254), (18, 2)]),
            Some(vec![tick_word(L, L as i128), tick_word(L, -(L as i128))]),
        );

        let err = client
            .market()
            .load_taker_market_snapshot()
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                PerpCityError::Contract(ContractError::MulticallFailed { ref reason })
                    if reason.contains("liquidity mismatch")
            ),
            "{err}"
        );
    }

    /// The immutables are read once per market: a second snapshot starts
    /// at the lagged block.
    #[tokio::test]
    async fn book_immutables_are_read_once_per_market() {
        let (client, rpc) = mock::client();
        immutables_answers(&rpc);
        snapshot_answers(&rpc, 0, bitmap(&[]), None);
        client.market().load_taker_market_snapshot().await.unwrap();

        let hash = snapshot_answers(&rpc, 0, bitmap(&[]), None);
        assert_eq!(
            client.market().load_taker_market_snapshot().await.unwrap(),
            expected_snapshot(hash, 0)
        );
        assert!(rpc.is_drained(), "nine answers: no immutables");
    }

    /// The immutables belong to the market, not the reader: a second
    /// reader of the same market over one chain reader inherits them, and
    /// a reader of another market reads its own.
    #[tokio::test]
    async fn book_immutables_are_shared_per_market_across_readers() {
        let (chain, rpc) = mock::chain();
        let (first, second) = (chain.market(PERP), chain.market(PERP));
        immutables_answers(&rpc);
        snapshot_answers(&rpc, 0, bitmap(&[]), None);
        first.load_taker_market_snapshot().await.unwrap();

        snapshot_answers(&rpc, 0, bitmap(&[]), None);
        second.load_taker_market_snapshot().await.unwrap();
        assert!(rpc.is_drained(), "the second reader skipped the immutables");

        let other = chain.market(Address::repeat_byte(0x12));
        immutables_answers(&rpc);
        snapshot_answers(&rpc, 0, bitmap(&[]), None);
        other.load_taker_market_snapshot().await.unwrap();
        assert!(rpc.is_drained(), "another market reads its own");
    }

    /// A non-positive spacing is rejected when the immutables are first
    /// read, and the rejection is not cached: the next snapshot asks again.
    #[tokio::test]
    async fn a_non_positive_tick_spacing_is_rejected_and_asked_again() {
        let (client, rpc) = mock::client();
        for spacing in [0, -30] {
            rpc.call::<Perp::POOL_IDCall>(&POOL_ID);
            rpc.call::<Perp::poolKeyCall>(&mock::pool_key(spacing));
            rpc.call::<Perp::EMA_WINDOWCall>(&U256::from(3_600u32));

            let err = client
                .market()
                .load_taker_market_snapshot()
                .await
                .unwrap_err();
            assert!(
                matches!(
                    err,
                    PerpCityError::Validation(ValidationError::InvalidConfig { .. })
                ),
                "{err}"
            );
            assert!(rpc.is_drained(), "three answers each time");
        }
    }
}
