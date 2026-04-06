//! Read operations: market data, balances, quotes, and multicall batches.

use alloy::primitives::{Address, B256, I256, U256};
use alloy::sol_types::{SolCall, SolValue};

use crate::constants::MULTICALL3;
use crate::contracts::{IBeacon, IERC20, IFees, IMarginRatios, IMulticall3, PerpManager};
use crate::convert::{
    leverage_to_margin_ratio, margin_ratio_to_leverage, price_x96_to_f64, scale_from_6dec,
    scale_to_6dec, sqrt_price_x96_to_price,
};
use crate::errors::{ContractError, Result, ValidationError};
use crate::hft::state_cache::{CachedBounds, CachedFees};
use crate::math::tick::{align_tick_down, align_tick_up, price_to_tick};
use crate::types::{
    Bounds, Fees, LiveDetails, OpenInterest, OpenMakerParams, OpenMakerQuote, OpenTakerParams,
    OpenTakerQuote, PerpData, PerpSnapshot, SwapQuote,
};

use super::{
    PerpClient, SCALE_F64, funding_x96_to_daily, i24_to_i32, i32_to_i24, i128_from_i256, now_secs,
    u24_to_u32, u32_to_u24,
};

impl PerpClient {
    // ── Read operations ──────────────────────────────────────────────

    /// Get the full perp configuration, fees, and bounds for a market.
    ///
    /// Uses the [`crate::hft::state_cache::StateCache`] for fees and bounds (60s TTL). The perp
    /// config itself is always fetched fresh (it's cheap and rarely changes).
    pub async fn get_perp_config(&self, perp_id: B256) -> Result<PerpData> {
        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);

        // Fetch perp config — sol!(rpc) returns the struct directly
        let config: PerpManager::PerpConfig = contract.cfgs(perp_id).call().await?;

        // Zero beacon means the perp was never created
        if config.beacon == Address::ZERO {
            return Err(ContractError::PerpNotFound { perp_id }.into());
        }

        let beacon = config.beacon;

        // Fetch mark price via TWAP (short window = ~current price)
        let sqrt_price_x96: U256 = contract
            .timeWeightedAvgSqrtPriceX96(perp_id, 1)
            .call()
            .await?;
        let mark = sqrt_price_x96_to_price(sqrt_price_x96)?;

        let fees = self.get_or_fetch_fees(&config).await?;
        let bounds = self.get_or_fetch_bounds(&config).await?;

        Ok(PerpData {
            id: perp_id,
            tick_spacing: i24_to_i32(config.key.tickSpacing),
            mark,
            beacon,
            bounds,
            fees,
        })
    }

    /// Get perp data: beacon, tick spacing, and current mark price.
    ///
    /// Lighter-weight than [`Self::get_perp_config`] — skips fees/bounds lookups.
    pub async fn get_perp_data(&self, perp_id: B256) -> Result<(Address, i32, f64)> {
        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let config: PerpManager::PerpConfig = contract.cfgs(perp_id).call().await?;

        let sqrt_price_x96: U256 = contract
            .timeWeightedAvgSqrtPriceX96(perp_id, 1)
            .call()
            .await?;
        let mark = sqrt_price_x96_to_price(sqrt_price_x96)?;

        Ok((config.beacon, i24_to_i32(config.key.tickSpacing), mark))
    }

    /// Get an on-chain position by its NFT token ID.
    ///
    /// Returns the raw contract position struct. Use [`crate::math::position`]
    /// functions to compute derived values (entry price, PnL, etc.).
    pub async fn get_position(&self, pos_id: U256) -> Result<PerpManager::Position> {
        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let pos: PerpManager::Position = contract.positions(pos_id).call().await?;

        // Check if position exists (empty perpId = uninitialized)
        if pos.perpId == B256::ZERO {
            return Err(ContractError::PositionNotFound { pos_id }.into());
        }

        Ok(pos)
    }

    /// Get all position IDs owned by an address.
    ///
    /// Iterates through all minted position NFTs (1..nextPosId) and returns
    /// those owned by `owner`. Burned or non-existent tokens are skipped.
    ///
    /// **Note:** This is O(n) in total positions ever minted. For high-throughput
    /// use cases, prefer the bot API's position endpoints instead.
    pub async fn get_positions_by_owner(&self, owner: Address) -> Result<Vec<U256>> {
        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let next_pos_id: U256 = contract.nextPosId().call().await?;

        let total: u64 = next_pos_id
            .try_into()
            .map_err(|_| ValidationError::Overflow {
                context: "nextPosId exceeds u64".into(),
            })?;
        if total <= 1 {
            return Ok(vec![]);
        }

        let mut owned = Vec::new();
        for id in 1..total {
            let pos_id = U256::from(id);
            // ownerOf reverts for burned/non-existent tokens — those
            // surface as contract errors, which we skip. Other transport
            // errors propagate so network failures aren't silently ignored.
            match contract.ownerOf(pos_id).call().await {
                Ok(addr) if addr == owner => owned.push(pos_id),
                Ok(_) => {}
                Err(e @ alloy::contract::Error::TransportError(_)) => return Err(e.into()),
                Err(_) => {} // burned or non-existent token
            }
        }

        Ok(owned)
    }

    /// Get the current mark price for a perp (TWAP with 1-second lookback).
    ///
    /// Uses the fast cache layer (2s TTL).
    pub async fn get_mark_price(&self, perp_id: B256) -> Result<f64> {
        let now_ts = now_secs();
        let perp_bytes: [u8; 32] = perp_id.into();

        // Check cache
        {
            let cache = self.state_cache.lock().unwrap();
            if let Some(price) = cache.get_mark_price(&perp_bytes, now_ts) {
                tracing::trace!(%perp_id, price, "mark price cache hit");
                return Ok(price);
            }
        }

        // Fetch from chain
        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let sqrt_price_x96: U256 = contract
            .timeWeightedAvgSqrtPriceX96(perp_id, 1)
            .call()
            .await?;
        let price = sqrt_price_x96_to_price(sqrt_price_x96)?;

        tracing::debug!(%perp_id, price, "mark price fetched");

        // Update cache
        {
            let mut cache = self.state_cache.lock().unwrap();
            cache.put_mark_price(perp_bytes, price, now_ts);
        }

        Ok(price)
    }

    /// Get the oracle index price from a beacon contract.
    ///
    /// The beacon address is available from `PerpData.beacon` (returned by
    /// [`get_perp_config`](Self::get_perp_config)).
    pub async fn get_index_price(&self, beacon: Address) -> Result<f64> {
        let contract = IBeacon::new(beacon, &self.provider);
        let index_x96: U256 = contract.index().call().await?;

        if index_x96.is_zero() {
            return Err(ValidationError::InvalidPrice {
                reason: "beacon returned zero index".into(),
            }
            .into());
        }

        let index = price_x96_to_f64(index_x96)?;
        Ok(index)
    }

    /// Simulate closing a position to get live PnL, funding, and liquidation status.
    ///
    /// This is a read-only call (no transaction sent).
    pub async fn get_live_details(&self, pos_id: U256) -> Result<LiveDetails> {
        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let result = contract.quoteClosePosition(pos_id).call().await?;

        // Check for unexpected revert reason
        if !result.unexpectedReason.is_empty() {
            return Err(ContractError::QuoteReverted {
                reason: format!(
                    "quoteClosePosition reverted: 0x{}",
                    alloy::primitives::hex::encode(&result.unexpectedReason)
                ),
            }
            .into());
        }

        let scale = SCALE_F64;
        Ok(LiveDetails {
            pnl: i128_from_i256(result.pnl) as f64 / scale,
            funding_payment: i128_from_i256(result.funding) as f64 / scale,
            effective_margin: i128_from_i256(result.netMargin) as f64 / scale,
            is_liquidatable: result.wasLiquidated,
        })
    }

    /// Get taker open interest for a perp market.
    pub async fn get_open_interest(&self, perp_id: B256) -> Result<OpenInterest> {
        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let result = contract.takerOpenInterest(perp_id).call().await?;

        let scale = SCALE_F64;
        Ok(OpenInterest {
            long_oi: result.longOI as f64 / scale,
            short_oi: result.shortOI as f64 / scale,
        })
    }

    /// Simulate opening a taker position without sending a transaction.
    ///
    /// Returns the perp and USD deltas that would result from the trade.
    /// Useful for estimating price impact before committing capital.
    pub async fn quote_open_taker(
        &self,
        perp_id: B256,
        params: &OpenTakerParams,
    ) -> Result<OpenTakerQuote> {
        let margin_scaled = scale_to_6dec(params.margin)?;
        if margin_scaled <= 0 {
            return Err(ValidationError::InvalidMargin {
                reason: format!("margin must be positive, got {}", params.margin),
            }
            .into());
        }
        let margin_ratio = leverage_to_margin_ratio(params.leverage)?;

        let wire_params = PerpManager::OpenTakerPositionParams {
            holder: self.address,
            isLong: params.is_long,
            margin: margin_scaled as u128,
            marginRatio: u32_to_u24(margin_ratio),
            unspecifiedAmountLimit: params.unspecified_amount_limit,
        };

        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let result = contract
            .quoteOpenTakerPosition(perp_id, wire_params)
            .call()
            .await?;

        if !result.unexpectedReason.is_empty() {
            return Err(ContractError::QuoteReverted {
                reason: format!(
                    "quoteOpenTakerPosition reverted: 0x{}",
                    alloy::primitives::hex::encode(&result.unexpectedReason)
                ),
            }
            .into());
        }

        let scale = SCALE_F64;
        Ok(OpenTakerQuote {
            perp_delta: i128_from_i256(result.perpDelta) as f64 / scale,
            usd_delta: i128_from_i256(result.usdDelta) as f64 / scale,
        })
    }

    /// Simulate opening a maker (LP) position without sending a transaction.
    ///
    /// Returns the perp and USD deltas that would result from the position.
    pub async fn quote_open_maker(
        &self,
        perp_id: B256,
        params: &OpenMakerParams,
    ) -> Result<OpenMakerQuote> {
        let margin_scaled = scale_to_6dec(params.margin)?;
        if margin_scaled <= 0 {
            return Err(ValidationError::InvalidMargin {
                reason: format!("margin must be positive, got {}", params.margin),
            }
            .into());
        }

        let tick_lower = align_tick_down(
            price_to_tick(params.price_lower)?,
            crate::constants::TICK_SPACING,
        );
        let tick_upper = align_tick_up(
            price_to_tick(params.price_upper)?,
            crate::constants::TICK_SPACING,
        );

        let wire_params = PerpManager::OpenMakerPositionParams {
            holder: self.address,
            margin: margin_scaled as u128,
            tickLower: i32_to_i24(tick_lower),
            tickUpper: i32_to_i24(tick_upper),
            liquidity: alloy::primitives::Uint::<120, 2>::from(params.liquidity),
            maxAmt0In: params.max_amt0_in,
            maxAmt1In: params.max_amt1_in,
        };

        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let result = contract
            .quoteOpenMakerPosition(perp_id, wire_params)
            .call()
            .await?;

        if !result.unexpectedReason.is_empty() {
            return Err(ContractError::QuoteReverted {
                reason: format!(
                    "quoteOpenMakerPosition reverted: 0x{}",
                    alloy::primitives::hex::encode(&result.unexpectedReason)
                ),
            }
            .into());
        }

        let scale = SCALE_F64;
        Ok(OpenMakerQuote {
            perp_delta: i128_from_i256(result.perpDelta) as f64 / scale,
            usd_delta: i128_from_i256(result.usdDelta) as f64 / scale,
        })
    }

    /// Simulate a raw swap in a perp's Uniswap V4 pool without executing.
    ///
    /// This is the lowest-level quote — it simulates a single pool swap and
    /// returns the resulting token deltas. Use this to estimate price impact
    /// for a given trade size.
    ///
    /// # Arguments
    ///
    /// * `perp_id` — The perp market to quote against.
    /// * `zero_for_one` — Swap direction: `true` sells token0 for token1.
    /// * `is_exact_in` — `true` if `amount` is the exact input; `false` for exact output.
    /// * `amount` — The swap amount (scaled to 6 decimals).
    /// * `sqrt_price_limit_x96` — Price limit in sqrtPriceX96 format. Use `0` for no limit.
    pub async fn quote_swap(
        &self,
        perp_id: B256,
        zero_for_one: bool,
        is_exact_in: bool,
        amount: U256,
        sqrt_price_limit_x96: U256,
    ) -> Result<SwapQuote> {
        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let sqrt_limit = alloy::primitives::Uint::<160, 3>::from(sqrt_price_limit_x96);
        let result = contract
            .quoteSwap(perp_id, zero_for_one, is_exact_in, amount, sqrt_limit)
            .call()
            .await?;

        if !result.unexpectedReason.is_empty() {
            return Err(ContractError::QuoteReverted {
                reason: format!(
                    "quoteSwap reverted: 0x{}",
                    alloy::primitives::hex::encode(&result.unexpectedReason)
                ),
            }
            .into());
        }

        let scale = SCALE_F64;
        Ok(SwapQuote {
            perp_delta: i128_from_i256(result.perpDelta) as f64 / scale,
            usd_delta: i128_from_i256(result.usdDelta) as f64 / scale,
        })
    }

    /// Get the funding rate per second for a perp, converted to a daily rate.
    ///
    /// Uses the fast cache layer (2s TTL).
    pub async fn get_funding_rate(&self, perp_id: B256) -> Result<f64> {
        let now_ts = now_secs();
        let perp_bytes: [u8; 32] = perp_id.into();

        // Check cache
        {
            let cache = self.state_cache.lock().unwrap();
            if let Some(rate) = cache.get_funding_rate(&perp_bytes, now_ts) {
                tracing::trace!(%perp_id, rate, "funding rate cache hit");
                return Ok(rate);
            }
        }

        let contract = PerpManager::new(self.deployments.perp_manager, &self.provider);
        let funding_x96: I256 = contract.fundingPerSecondX96(perp_id).call().await?;

        let daily_rate = funding_x96_to_daily(funding_x96);

        tracing::debug!(%perp_id, daily_rate, "funding rate fetched");

        // Update cache
        {
            let mut cache = self.state_cache.lock().unwrap();
            cache.put_funding_rate(perp_bytes, daily_rate, now_ts);
        }

        Ok(daily_rate)
    }

    /// Get the USDC balance of the signer's address.
    ///
    /// Uses the fast cache layer (2s TTL).
    pub async fn get_usdc_balance(&self) -> Result<f64> {
        let now_ts = now_secs();

        // Check cache
        {
            let cache = self.state_cache.lock().unwrap();
            if let Some(bal) = cache.get_usdc_balance(now_ts) {
                tracing::trace!(balance = bal, "USDC balance cache hit");
                return Ok(bal);
            }
        }

        let usdc = IERC20::new(self.deployments.usdc, &self.provider);
        let raw: U256 = usdc.balanceOf(self.address).call().await?;
        let raw_i128 = i128::try_from(raw).map_err(|_| ValidationError::Overflow {
            context: format!("USDC balance {} exceeds i128::MAX", raw),
        })?;
        let balance = scale_from_6dec(raw_i128);

        tracing::debug!(balance, "USDC balance fetched");

        // Update cache
        {
            let mut cache = self.state_cache.lock().unwrap();
            cache.put_usdc_balance(balance, now_ts);
        }

        Ok(balance)
    }

    // ── Batch reads (via Multicall3) ──────────────────────────────────

    /// Get the USDC and ETH balances of an address in a single RPC call.
    ///
    /// Uses Multicall3 to bundle a `balanceOf` (USDC) and `getEthBalance`
    /// (native ETH) into one `eth_call`. The RPC provider charges 1 CU
    /// regardless of how many sub-calls the multicall executes.
    ///
    /// Returns `(usdc_balance, eth_balance)` where USDC is in human units
    /// (e.g. `100.0` = 100 USDC) and ETH is in wei.
    pub async fn get_balances(&self, address: Address) -> Result<(f64, U256)> {
        let results = self.get_balances_batch(&[address]).await?;
        Ok(results.into_iter().next().unwrap())
    }

    /// Get the USDC and ETH balances for multiple addresses in a single RPC call.
    ///
    /// Uses Multicall3 to bundle N × `balanceOf` + N × `getEthBalance` into
    /// one `eth_call`. For 10 addresses, this is 1 CU instead of 20.
    ///
    /// Returns a `Vec<(usdc_balance, eth_balance)>` in the same order as
    /// the input addresses.
    pub async fn get_balances_batch(&self, addresses: &[Address]) -> Result<Vec<(f64, U256)>> {
        if addresses.is_empty() {
            return Ok(Vec::new());
        }

        let usdc_addr = self.deployments.usdc;
        let n = addresses.len();

        // Build sub-calls: N × USDC balanceOf + N × ETH getEthBalance
        let mut calls = Vec::with_capacity(2 * n);

        for &addr in addresses {
            // USDC balanceOf(addr)
            let calldata = IERC20::balanceOfCall { account: addr }.abi_encode();
            calls.push(IMulticall3::Call3 {
                target: usdc_addr,
                allowFailure: false,
                callData: calldata.into(),
            });
        }

        for &addr in addresses {
            // getEthBalance(addr) — Multicall3 built-in
            let calldata = IMulticall3::getEthBalanceCall { addr }.abi_encode();
            calls.push(IMulticall3::Call3 {
                target: MULTICALL3,
                allowFailure: false,
                callData: calldata.into(),
            });
        }

        let multicall = IMulticall3::new(MULTICALL3, &self.provider);
        let results = multicall.aggregate3(calls).call().await?;

        if results.len() != 2 * n {
            return Err(ValidationError::Overflow {
                context: format!(
                    "multicall returned {} results, expected {}",
                    results.len(),
                    2 * n
                ),
            }
            .into());
        }

        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            // Decode USDC balance (first N results)
            let usdc_result = &results[i];
            if !usdc_result.success {
                return Err(ValidationError::Overflow {
                    context: format!("USDC balanceOf failed for address {}", addresses[i]),
                }
                .into());
            }
            let usdc_raw = U256::abi_decode(&usdc_result.returnData).map_err(|e| {
                ValidationError::Overflow {
                    context: format!("failed to decode USDC balance: {e}"),
                }
            })?;
            let usdc_i128 = i128::try_from(usdc_raw).map_err(|_| ValidationError::Overflow {
                context: format!("USDC balance {} exceeds i128::MAX", usdc_raw),
            })?;
            let usdc = scale_from_6dec(usdc_i128);

            // Decode ETH balance (last N results)
            let eth_result = &results[n + i];
            if !eth_result.success {
                return Err(ValidationError::Overflow {
                    context: format!("getEthBalance failed for address {}", addresses[i]),
                }
                .into());
            }
            let eth = U256::abi_decode(&eth_result.returnData).map_err(|e| {
                ValidationError::Overflow {
                    context: format!("failed to decode ETH balance: {e}"),
                }
            })?;

            out.push((usdc, eth));
        }

        tracing::debug!(count = n, "batch balances fetched via multicall");
        Ok(out)
    }

    /// Get perp config and live market data in two multicalls (2 CUs total).
    ///
    /// Phase 1 multicalls `cfgs` + `timeWeightedAvgSqrtPriceX96` +
    /// `fundingPerSecondX96` + `takerOpenInterest` against PerpManager
    /// (4 reads → 1 CU). Phase 2 calls `index()` on the beacon returned
    /// by phase 1 (1 CU).
    ///
    /// Returns `(PerpData, PerpSnapshot)` — static config and live market
    /// data. Replaces the typical startup sequence of 5+ individual RPCs.
    pub async fn get_perp_snapshot(&self, perp_id: B256) -> Result<(PerpData, PerpSnapshot)> {
        let pm = self.deployments.perp_manager;

        // Phase 1: multicall cfgs + mark + funding + OI against PerpManager
        let calls = vec![
            IMulticall3::Call3 {
                target: pm,
                allowFailure: false,
                callData: PerpManager::cfgsCall { perpId: perp_id }
                    .abi_encode()
                    .into(),
            },
            IMulticall3::Call3 {
                target: pm,
                allowFailure: false,
                callData: PerpManager::timeWeightedAvgSqrtPriceX96Call {
                    perpId: perp_id,
                    lookbackWindow: 1,
                }
                .abi_encode()
                .into(),
            },
            IMulticall3::Call3 {
                target: pm,
                allowFailure: false,
                callData: PerpManager::fundingPerSecondX96Call { perpId: perp_id }
                    .abi_encode()
                    .into(),
            },
            IMulticall3::Call3 {
                target: pm,
                allowFailure: false,
                callData: PerpManager::takerOpenInterestCall { perpId: perp_id }
                    .abi_encode()
                    .into(),
            },
        ];

        let multicall = IMulticall3::new(MULTICALL3, &self.provider);
        let results = multicall.aggregate3(calls).call().await?;

        if results.len() != 4 {
            return Err(ValidationError::Overflow {
                context: format!(
                    "perp snapshot multicall returned {} results, expected 4",
                    results.len()
                ),
            }
            .into());
        }

        let call_names = [
            "cfgs",
            "timeWeightedAvgSqrtPriceX96",
            "fundingPerSecondX96",
            "takerOpenInterest",
        ];
        for (i, name) in call_names.iter().enumerate() {
            if !results[i].success {
                return Err(ValidationError::Overflow {
                    context: format!("perp snapshot multicall: {name} call failed"),
                }
                .into());
            }
        }

        // Decode cfgs
        let config = PerpManager::PerpConfig::abi_decode(&results[0].returnData).map_err(|e| {
            ValidationError::Overflow {
                context: format!("failed to decode PerpConfig: {e}"),
            }
        })?;

        if config.beacon == Address::ZERO {
            return Err(ContractError::PerpNotFound { perp_id }.into());
        }

        // Decode mark price
        let sqrt_price_x96 =
            U256::abi_decode(&results[1].returnData).map_err(|e| ValidationError::Overflow {
                context: format!("failed to decode mark price: {e}"),
            })?;
        let mark = sqrt_price_x96_to_price(sqrt_price_x96)?;

        // Decode funding rate
        let funding_x96 =
            I256::abi_decode(&results[2].returnData).map_err(|e| ValidationError::Overflow {
                context: format!("failed to decode funding rate: {e}"),
            })?;
        let funding_rate_daily = funding_x96_to_daily(funding_x96);

        // Decode OI — takerOpenInterest returns (uint128 longOI, uint128 shortOI)
        let (long_oi, short_oi) =
            <(u128, u128)>::abi_decode(&results[3].returnData).map_err(|e| {
                ValidationError::Overflow {
                    context: format!("failed to decode open interest: {e}"),
                }
            })?;
        let open_interest = OpenInterest {
            long_oi: long_oi as f64 / SCALE_F64,
            short_oi: short_oi as f64 / SCALE_F64,
        };

        // Phase 2: fetch index price from beacon (1 CU)
        let index_price = self.get_index_price(config.beacon).await?;

        // Build PerpData (fetch fees/bounds from cache or chain)
        let fees = self.get_or_fetch_fees(&config).await?;
        let bounds = self.get_or_fetch_bounds(&config).await?;

        let perp_data = PerpData {
            id: perp_id,
            tick_spacing: i24_to_i32(config.key.tickSpacing),
            mark,
            beacon: config.beacon,
            bounds,
            fees,
        };

        let snapshot = PerpSnapshot {
            mark_price: mark,
            index_price,
            funding_rate_daily,
            open_interest,
        };

        tracing::debug!(%perp_id, "perp snapshot fetched via multicall");
        Ok((perp_data, snapshot))
    }

    // ── Cache helpers ───────────────────────────────────────────────

    /// Get fees from cache or fetch from chain.
    async fn get_or_fetch_fees(&self, config: &PerpManager::PerpConfig) -> Result<Fees> {
        let now_ts = now_secs();
        let fees_addr: [u8; 20] = config.fees.into();

        let cached = {
            let cache = self.state_cache.lock().unwrap();
            cache.get_fees(&fees_addr, now_ts).cloned()
        };

        match cached {
            Some(cached) => Ok(Fees::from(cached)),
            None => {
                let fees = self.fetch_fees(config).await?;
                let mut cache = self.state_cache.lock().unwrap();
                cache.put_fees(fees_addr, CachedFees::from(fees), now_ts);
                Ok(fees)
            }
        }
    }

    /// Get bounds from cache or fetch from chain.
    async fn get_or_fetch_bounds(&self, config: &PerpManager::PerpConfig) -> Result<Bounds> {
        let now_ts = now_secs();
        let ratios_addr: [u8; 20] = config.marginRatios.into();

        let cached = {
            let cache = self.state_cache.lock().unwrap();
            cache.get_bounds(&ratios_addr, now_ts).cloned()
        };

        match cached {
            Some(cached) => Ok(Bounds::from(cached)),
            None => {
                let bounds = self.fetch_bounds(config).await?;
                let mut cache = self.state_cache.lock().unwrap();
                cache.put_bounds(ratios_addr, CachedBounds::from(bounds), now_ts);
                Ok(bounds)
            }
        }
    }

    /// Fetch fees from the IFees module contract.
    async fn fetch_fees(&self, config: &PerpManager::PerpConfig) -> Result<Fees> {
        if config.fees == Address::ZERO {
            return Err(ContractError::ModuleNotRegistered {
                module: "IFees".into(),
            }
            .into());
        }

        let fees_contract = IFees::new(config.fees, &self.provider);

        let fee_result = fees_contract.fees(config.clone()).call().await?;
        let c_fee = u24_to_u32(fee_result.cFee);
        let ins_fee = u24_to_u32(fee_result.insFee);
        let lp_fee = u24_to_u32(fee_result.lpFee);

        let liq_result = fees_contract.liquidationFee(config.clone()).call().await?;
        let liq_fee = u24_to_u32(liq_result);

        let scale = SCALE_F64;
        Ok(Fees {
            creator_fee: c_fee as f64 / scale,
            insurance_fee: ins_fee as f64 / scale,
            lp_fee: lp_fee as f64 / scale,
            liquidation_fee: liq_fee as f64 / scale,
        })
    }

    /// Fetch margin ratio bounds from the IMarginRatios module contract.
    async fn fetch_bounds(&self, config: &PerpManager::PerpConfig) -> Result<Bounds> {
        if config.marginRatios == Address::ZERO {
            return Err(ContractError::ModuleNotRegistered {
                module: "IMarginRatios".into(),
            }
            .into());
        }

        let ratios_contract = IMarginRatios::new(config.marginRatios, &self.provider);

        let ratios: IMarginRatios::MarginRatios = ratios_contract
            .marginRatios(config.clone(), false)
            .call()
            .await?;

        let scale = SCALE_F64;
        Ok(Bounds {
            min_margin: scale_from_6dec(crate::constants::MIN_OPENING_MARGIN as i128),
            min_taker_leverage: margin_ratio_to_leverage(u24_to_u32(ratios.max))?,
            max_taker_leverage: margin_ratio_to_leverage(u24_to_u32(ratios.min))?,
            liquidation_taker_ratio: u24_to_u32(ratios.liq) as f64 / scale,
        })
    }
}
