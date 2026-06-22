//! Read operations: market data, positions, balances, and multicall batches.
//!
//! The client is bound to a single `Perp` market (`deployments.perp`). There is
//! no `PerpManager` and no `perp_id` — the market is identified by which `Perp`
//! contract the client points at. Positions are keyed by `posId` (ERC721 token
//! id) within that `Perp`.
//!
//! Pre-trade quoting (the old `quote_*` / `quoteClosePosition` family) is not
//! available: the frozen `Perp` exposes no on-chain quote/preview views. That
//! surface will return in a later stage (off-chain math or `eth_call`
//! simulation).

use alloy::primitives::{Address, U256};
use alloy::sol_types::{SolCall, SolValue};

use crate::constants::MULTICALL3;
use crate::contracts::{IBeacon, IERC20, IFees, IMarginRatios, IMulticall3, Perp, Position};
use crate::convert::{margin_ratio_to_leverage, price_x96_to_f64, scale_from_6dec};
use crate::errors::{ContractError, Result, ValidationError};
use crate::hft::state_cache::{CachedBounds, CachedFees};
use crate::types::{Bounds, Fees, OpenInterest, PerpData, PerpSnapshot};

use super::{PerpClient, SCALE_F64, i24_to_i32, now_secs, u24_to_u32};

/// Funding/utilization rates are scaled by 1e18 per day on-chain.
const WAD_F64: f64 = 1e18;

/// Convert an `int88` per-day funding rate (scaled by 1e18) to a human-readable
/// fraction. An 88-bit signed value always fits in `i128`.
fn funding_per_day_to_f64(rate: alloy::primitives::Signed<88, 2>) -> f64 {
    i128::try_from(rate).unwrap_or(0) as f64 / WAD_F64
}

impl PerpClient {
    // ── Read operations ──────────────────────────────────────────────

    /// Cache key for this client's market: the `Perp` address left-padded to 32 bytes.
    fn market_key(&self) -> [u8; 32] {
        self.deployments.perp.into_word().0
    }

    /// Get the full perp configuration, fees, and bounds for the market.
    ///
    /// Uses the [`crate::hft::state_cache::StateCache`] for fees and bounds (60s TTL).
    pub async fn get_perp_config(&self) -> Result<PerpData> {
        let perp = Perp::new(self.deployments.perp, &self.provider);

        let modules = perp.modules().call().await?;
        let pool_key = perp.poolKey().call().await?;
        let pool_state = perp.poolState().call().await?;
        let mark = price_x96_to_f64(pool_state.ammPrice)?;

        let fees = self.get_or_fetch_fees(modules.fees).await?;
        let bounds = self.get_or_fetch_bounds(modules.marginRatios).await?;

        Ok(PerpData {
            perp: self.deployments.perp,
            tick_spacing: i24_to_i32(pool_key.tickSpacing),
            mark,
            beacon: modules.beacon,
            bounds,
            fees,
        })
    }

    /// Get perp data: beacon, tick spacing, and current mark price.
    ///
    /// Lighter-weight than [`Self::get_perp_config`] — skips fees/bounds lookups.
    pub async fn get_perp_data(&self) -> Result<(Address, i32, f64)> {
        let perp = Perp::new(self.deployments.perp, &self.provider);
        let modules = perp.modules().call().await?;
        let pool_key = perp.poolKey().call().await?;
        let pool_state = perp.poolState().call().await?;
        let mark = price_x96_to_f64(pool_state.ammPrice)?;

        Ok((modules.beacon, i24_to_i32(pool_key.tickSpacing), mark))
    }

    /// Get an on-chain position by its NFT token ID.
    ///
    /// Returns the raw contract position struct. Use [`crate::math::position`]
    /// functions to compute derived values (entry price, PnL, etc.).
    pub async fn get_position(&self, pos_id: U256) -> Result<Position> {
        let perp = Perp::new(self.deployments.perp, &self.provider);
        let pos = perp.positions(pos_id).call().await?;

        // A non-existent or burned position decodes to an all-zero struct.
        if pos.margin == 0 && pos.delta.is_zero() {
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
        let perp = Perp::new(self.deployments.perp, &self.provider);
        let next_pos_id: U256 = perp.nextPosId().call().await?;

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
            match perp.ownerOf(pos_id).call().await {
                Ok(addr) if addr == owner => owned.push(pos_id),
                Ok(_) => {}
                Err(e @ alloy::contract::Error::TransportError(_)) => return Err(e.into()),
                Err(_) => {} // burned or non-existent token
            }
        }

        Ok(owned)
    }

    /// Get the current mark price for the market (AMM spot price).
    ///
    /// Reads the live Uniswap V4 pool price via `poolState`. Uses the fast
    /// cache layer (2s TTL).
    pub async fn get_mark_price(&self) -> Result<f64> {
        let now_ts = now_secs();
        let key = self.market_key();

        // Check cache
        {
            let cache = self.state_cache.lock().unwrap();
            if let Some(price) = cache.get_mark_price(&key, now_ts) {
                tracing::trace!(price, "mark price cache hit");
                return Ok(price);
            }
        }

        // Fetch from chain
        let perp = Perp::new(self.deployments.perp, &self.provider);
        let pool_state = perp.poolState().call().await?;
        let price = price_x96_to_f64(pool_state.ammPrice)?;

        tracing::debug!(price, "mark price fetched");

        // Update cache
        {
            let mut cache = self.state_cache.lock().unwrap();
            cache.put_mark_price(key, price, now_ts);
        }

        Ok(price)
    }

    /// Get the oracle index price from a beacon contract.
    ///
    /// The beacon address is available from `PerpData.beacon` (returned by
    /// [`get_perp_config`](Self::get_perp_config)).
    ///
    /// Note: `index()` is a state-mutating function on-chain; this performs an
    /// `eth_call` (simulation) and does not send a transaction.
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

    /// Get taker open interest for the market.
    pub async fn get_open_interest(&self) -> Result<OpenInterest> {
        let perp = Perp::new(self.deployments.perp, &self.provider);
        let oi = perp.openInterest().call().await?;

        Ok(OpenInterest {
            long_oi: oi.long as f64 / SCALE_F64,
            short_oi: oi.short as f64 / SCALE_F64,
        })
    }

    /// Get the current daily funding rate for the market.
    ///
    /// Reads `rates().fundingPerDay` (scaled by 1e18 per day). Positive means
    /// long-exposed positions pay short-exposed positions. Uses the fast cache
    /// layer (2s TTL).
    pub async fn get_funding_rate(&self) -> Result<f64> {
        let now_ts = now_secs();
        let key = self.market_key();

        // Check cache
        {
            let cache = self.state_cache.lock().unwrap();
            if let Some(rate) = cache.get_funding_rate(&key, now_ts) {
                tracing::trace!(rate, "funding rate cache hit");
                return Ok(rate);
            }
        }

        let perp = Perp::new(self.deployments.perp, &self.provider);
        let rates = perp.rates().call().await?;
        let daily_rate = funding_per_day_to_f64(rates.fundingPerDay);

        tracing::debug!(daily_rate, "funding rate fetched");

        // Update cache
        {
            let mut cache = self.state_cache.lock().unwrap();
            cache.put_funding_rate(key, daily_rate, now_ts);
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
            return Err(ContractError::MulticallFailed {
                reason: format!(
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
                return Err(ContractError::MulticallFailed {
                    reason: format!("USDC balanceOf failed for address {}", addresses[i]),
                }
                .into());
            }
            let usdc_raw = U256::abi_decode(&usdc_result.returnData).map_err(|e| {
                ValidationError::DecodeFailed {
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
                return Err(ContractError::MulticallFailed {
                    reason: format!("getEthBalance failed for address {}", addresses[i]),
                }
                .into());
            }
            let eth = U256::abi_decode(&eth_result.returnData).map_err(|e| {
                ValidationError::DecodeFailed {
                    context: format!("failed to decode ETH balance: {e}"),
                }
            })?;

            out.push((usdc, eth));
        }

        tracing::debug!(count = n, "batch balances fetched via multicall");
        Ok(out)
    }

    /// Get perp config and live market data in a single multicall (plus the
    /// beacon index read).
    ///
    /// Batches `modules` + `poolKey` + `poolState` + `rates` + `openInterest`
    /// against the `Perp` contract (5 reads → 1 CU), then calls `index()` on the
    /// beacon returned by the batch (1 CU). Replaces a startup sequence of
    /// several individual RPCs.
    ///
    /// Returns `(PerpData, PerpSnapshot)` — static config and live market data.
    pub async fn get_perp_snapshot(&self) -> Result<(PerpData, PerpSnapshot)> {
        let perp_addr = self.deployments.perp;

        let calls = vec![
            IMulticall3::Call3 {
                target: perp_addr,
                allowFailure: false,
                callData: Perp::modulesCall {}.abi_encode().into(),
            },
            IMulticall3::Call3 {
                target: perp_addr,
                allowFailure: false,
                callData: Perp::poolKeyCall {}.abi_encode().into(),
            },
            IMulticall3::Call3 {
                target: perp_addr,
                allowFailure: false,
                callData: Perp::poolStateCall {}.abi_encode().into(),
            },
            IMulticall3::Call3 {
                target: perp_addr,
                allowFailure: false,
                callData: Perp::ratesCall {}.abi_encode().into(),
            },
            IMulticall3::Call3 {
                target: perp_addr,
                allowFailure: false,
                callData: Perp::openInterestCall {}.abi_encode().into(),
            },
        ];

        let multicall = IMulticall3::new(MULTICALL3, &self.provider);
        let results = multicall.aggregate3(calls).call().await?;

        let call_names = ["modules", "poolKey", "poolState", "rates", "openInterest"];
        if results.len() != call_names.len() {
            return Err(ContractError::MulticallFailed {
                reason: format!(
                    "perp snapshot multicall returned {} results, expected {}",
                    results.len(),
                    call_names.len()
                ),
            }
            .into());
        }
        for (i, name) in call_names.iter().enumerate() {
            if !results[i].success {
                return Err(ContractError::MulticallFailed {
                    reason: format!("perp snapshot multicall: {name} call failed"),
                }
                .into());
            }
        }

        let decode_err = |name: &str, e: alloy::sol_types::Error| ValidationError::DecodeFailed {
            context: format!("failed to decode {name}: {e}"),
        };

        let modules = Perp::modulesCall::abi_decode_returns(&results[0].returnData)
            .map_err(|e| decode_err("modules", e))?;
        let pool_key = Perp::poolKeyCall::abi_decode_returns(&results[1].returnData)
            .map_err(|e| decode_err("poolKey", e))?;
        let pool_state = Perp::poolStateCall::abi_decode_returns(&results[2].returnData)
            .map_err(|e| decode_err("poolState", e))?;
        let rates = Perp::ratesCall::abi_decode_returns(&results[3].returnData)
            .map_err(|e| decode_err("rates", e))?;
        let oi = Perp::openInterestCall::abi_decode_returns(&results[4].returnData)
            .map_err(|e| decode_err("openInterest", e))?;

        let mark = price_x96_to_f64(pool_state.ammPrice)?;
        let funding_rate_daily = funding_per_day_to_f64(rates.fundingPerDay);
        let open_interest = OpenInterest {
            long_oi: oi.long as f64 / SCALE_F64,
            short_oi: oi.short as f64 / SCALE_F64,
        };

        // Index price from the beacon (1 CU).
        let index_price = self.get_index_price(modules.beacon).await?;

        // Fees/bounds (from cache or chain).
        let fees = self.get_or_fetch_fees(modules.fees).await?;
        let bounds = self.get_or_fetch_bounds(modules.marginRatios).await?;

        let perp_data = PerpData {
            perp: perp_addr,
            tick_spacing: i24_to_i32(pool_key.tickSpacing),
            mark,
            beacon: modules.beacon,
            bounds,
            fees,
        };

        let snapshot = PerpSnapshot {
            mark_price: mark,
            index_price,
            funding_rate_daily,
            open_interest,
        };

        tracing::debug!("perp snapshot fetched via multicall");
        Ok((perp_data, snapshot))
    }

    // ── Cache helpers ───────────────────────────────────────────────

    /// Get fees from cache or fetch from the `IFees` module at `fees_addr`.
    async fn get_or_fetch_fees(&self, fees_addr: Address) -> Result<Fees> {
        let now_ts = now_secs();
        let key: [u8; 20] = fees_addr.into();

        let cached = {
            let cache = self.state_cache.lock().unwrap();
            cache.get_fees(&key, now_ts).cloned()
        };

        match cached {
            Some(cached) => Ok(Fees::from(cached)),
            None => {
                let fees = self.fetch_fees(fees_addr).await?;
                let mut cache = self.state_cache.lock().unwrap();
                cache.put_fees(key, CachedFees::from(fees), now_ts);
                Ok(fees)
            }
        }
    }

    /// Get bounds from cache or fetch from the `IMarginRatios` module at `ratios_addr`.
    async fn get_or_fetch_bounds(&self, ratios_addr: Address) -> Result<Bounds> {
        let now_ts = now_secs();
        let key: [u8; 20] = ratios_addr.into();

        let cached = {
            let cache = self.state_cache.lock().unwrap();
            cache.get_bounds(&key, now_ts).cloned()
        };

        match cached {
            Some(cached) => Ok(Bounds::from(cached)),
            None => {
                let bounds = self.fetch_bounds(ratios_addr).await?;
                let mut cache = self.state_cache.lock().unwrap();
                cache.put_bounds(key, CachedBounds::from(bounds), now_ts);
                Ok(bounds)
            }
        }
    }

    /// Fetch fees from the `IFees` module contract.
    async fn fetch_fees(&self, fees_addr: Address) -> Result<Fees> {
        if fees_addr == Address::ZERO {
            return Err(ContractError::ModuleNotRegistered {
                module: "IFees".into(),
            }
            .into());
        }

        let fees_contract = IFees::new(fees_addr, &self.provider);

        let fee_result = fees_contract.fees().call().await?;
        let c_fee = u24_to_u32(fee_result.cFee);
        let ins_fee = u24_to_u32(fee_result.insFee);
        let lp_fee = u24_to_u32(fee_result.lpFee);

        let liq_fee = u24_to_u32(fees_contract.liqFee().call().await?);

        let scale = SCALE_F64;
        Ok(Fees {
            creator_fee: c_fee as f64 / scale,
            insurance_fee: ins_fee as f64 / scale,
            lp_fee: lp_fee as f64 / scale,
            liquidation_fee: liq_fee as f64 / scale,
        })
    }

    /// Fetch taker margin-ratio bounds from the `IMarginRatios` module contract.
    async fn fetch_bounds(&self, ratios_addr: Address) -> Result<Bounds> {
        if ratios_addr == Address::ZERO {
            return Err(ContractError::ModuleNotRegistered {
                module: "IMarginRatios".into(),
            }
            .into());
        }

        let ratios_contract = IMarginRatios::new(ratios_addr, &self.provider);
        let taker = ratios_contract.takerMarginRatios().call().await?;

        let scale = SCALE_F64;
        Ok(Bounds {
            min_margin: scale_from_6dec(crate::constants::MIN_OPENING_MARGIN as i128),
            // The initial margin ratio is the minimum margin → maximum leverage.
            min_taker_leverage: 1.0,
            max_taker_leverage: margin_ratio_to_leverage(u24_to_u32(taker.init))?,
            liquidation_taker_ratio: u24_to_u32(taker.liq) as f64 / scale,
        })
    }
}
