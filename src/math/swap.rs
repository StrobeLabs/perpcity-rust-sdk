//! Exact concentrated-liquidity taker swap simulation.
//!
//! PerpCity specifies token0 (`perp`) for both directions: positive deltas are
//! exact-output buys and negative deltas are exact-input sells. The pool fee is
//! zero, so this module implements precisely those two Uniswap V4 paths using
//! integer Q64.96 arithmetic and Solidity-compatible rounding.

use std::collections::BTreeMap;
use std::ops::Bound::{Excluded, Included, Unbounded};

use alloy::primitives::{B256, U256, U512};
use serde::{Deserialize, Serialize};

use crate::constants::{MAX_SWAP_SQRT_PRICE_X96, MIN_SWAP_SQRT_PRICE_X96, Q96};
use crate::errors::ValidationError;
use crate::math::tick::{get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio};

/// Liquidity stored at an initialized tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickLiquidity {
    /// Total liquidity referencing the tick.
    pub gross: u128,
    /// Liquidity change when crossing from left to right.
    pub net: i128,
}

/// Why a quote could not be executed exactly as requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuoteLimit {
    /// The requested amount filled.
    Filled,
    /// The requested target price was reached.
    TargetPrice,
    /// The configured price-impact bound stopped the quote.
    PriceImpact,
    /// The protocol terminal price was reached.
    TerminalPrice,
    /// There was not enough initialized liquidity to fill the amount.
    InsufficientLiquidity,
}

/// Constraints applied to a target-price quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteConstraints {
    /// Enforce the price-impact module's current bounds.
    pub enforce_price_impact: bool,
    /// Optional absolute cap on the perp atoms traded.
    pub max_perp: Option<u128>,
}

impl Default for QuoteConstraints {
    fn default() -> Self {
        Self {
            enforce_price_impact: true,
            max_perp: None,
        }
    }
}

/// Immutable, block-referenced concentrated-liquidity market snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TakerMarketSnapshot {
    /// Block containing all state in this snapshot.
    pub block_number: u64,
    /// Canonical block hash.
    pub block_hash: B256,
    /// Block timestamp.
    pub block_timestamp: u64,
    /// Current Q64.96 square-root price.
    pub sqrt_price_x96: U256,
    /// Current pool tick.
    pub tick: i32,
    /// Active liquidity at the current tick.
    pub liquidity: u128,
    /// Initialized ticks keyed in ascending order.
    pub ticks: BTreeMap<i32, TickLiquidity>,
    /// Minimum price the Perp swap itself can reach.
    pub protocol_sqrt_min_x96: U256,
    /// Maximum price the Perp swap itself can reach.
    pub protocol_sqrt_max_x96: U256,
    /// Current lower bound returned by the price-impact module.
    pub impact_sqrt_min_x96: U256,
    /// Current upper bound returned by the price-impact module.
    pub impact_sqrt_max_x96: U256,
}

impl Default for TakerMarketSnapshot {
    fn default() -> Self {
        Self {
            block_number: 0,
            block_hash: B256::ZERO,
            block_timestamp: 0,
            sqrt_price_x96: Q96,
            tick: 0,
            liquidity: 0,
            ticks: BTreeMap::new(),
            protocol_sqrt_min_x96: MIN_SWAP_SQRT_PRICE_X96,
            protocol_sqrt_max_x96: MAX_SWAP_SQRT_PRICE_X96,
            impact_sqrt_min_x96: MIN_SWAP_SQRT_PRICE_X96,
            impact_sqrt_max_x96: MAX_SWAP_SQRT_PRICE_X96,
        }
    }
}

/// Exact result of a local taker simulation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TakerQuote {
    /// Requested signed perp atoms.
    pub requested_perp_delta: i128,
    /// Filled signed perp atoms.
    pub perp_delta: i128,
    /// Signed token1 atoms: negative when paid, positive when received.
    pub usd_delta: i128,
    /// Starting Q64.96 square-root price.
    pub sqrt_price_start_x96: U256,
    /// Ending Q64.96 square-root price.
    pub sqrt_price_after_x96: U256,
    /// Ending tick using V4 boundary semantics.
    pub tick_after: i32,
    /// Active liquidity after the swap.
    pub liquidity_after: u128,
    /// Initialized ticks crossed by the swap.
    pub ticks_crossed: Vec<i32>,
    /// Whether the complete requested amount filled.
    pub fully_filled: bool,
    /// Whether the resulting price is accepted by the price-impact module.
    pub price_impact_allowed: bool,
    /// The first constraint encountered.
    pub limit: QuoteLimit,
    /// Reference block number.
    pub block_number: u64,
    /// Reference block hash.
    pub block_hash: B256,
}

impl TakerQuote {
    /// Derive the contract `amt1Limit` with a directional slippage cushion.
    ///
    /// Buys return the maximum USD payment; sells return the minimum USD
    /// proceeds. `slippage_bps=25` corresponds to the Legion default of 0.25%.
    pub fn amt1_limit(&self, slippage_bps: u32) -> u128 {
        let amount = self.usd_delta.unsigned_abs();
        let bps = slippage_bps as u128;
        if self.perp_delta > 0 {
            amount.saturating_mul(10_000 + bps).saturating_add(9_999) / 10_000
        } else {
            amount.saturating_mul(10_000u128.saturating_sub(bps)) / 10_000
        }
    }

    /// Average execution price in human-readable token1/token0 units.
    pub fn effective_price(&self) -> Option<f64> {
        (self.perp_delta != 0)
            .then(|| self.usd_delta.unsigned_abs() as f64 / self.perp_delta.unsigned_abs() as f64)
    }
}

#[derive(Debug)]
struct Simulation {
    perp_delta: i128,
    usd_delta: i128,
    sqrt_after: U256,
    tick_after: i32,
    liquidity_after: u128,
    crossed: Vec<i32>,
    fully_filled: bool,
    hit_limit: bool,
}

impl TakerMarketSnapshot {
    /// Quote an exact signed perp delta using the snapshot's protocol price
    /// limits, then evaluate the resulting price against the module bounds.
    pub fn quote_perp(&self, perp_delta: i128) -> Result<TakerQuote, ValidationError> {
        if perp_delta == 0 {
            return Ok(self.finish_quote(
                perp_delta,
                Simulation {
                    perp_delta: 0,
                    usd_delta: 0,
                    sqrt_after: self.sqrt_price_x96,
                    tick_after: self.tick,
                    liquidity_after: self.liquidity,
                    crossed: Vec::new(),
                    fully_filled: true,
                    hit_limit: false,
                },
                QuoteLimit::Filled,
            ));
        }
        let limit = if perp_delta < 0 {
            self.protocol_sqrt_min_x96
        } else {
            self.protocol_sqrt_max_x96
        };
        let sim = self.simulate(perp_delta, limit)?;
        let reason = if sim.fully_filled {
            QuoteLimit::Filled
        } else if sim.hit_limit {
            QuoteLimit::TerminalPrice
        } else {
            QuoteLimit::InsufficientLiquidity
        };
        Ok(self.finish_quote(perp_delta, sim, reason))
    }

    /// Size and quote the largest trade toward `target_sqrt_price_x96` without
    /// overshooting it or an enabled price-impact bound.
    pub fn quote_to_price(
        &self,
        target_sqrt_price_x96: U256,
        constraints: QuoteConstraints,
    ) -> Result<TakerQuote, ValidationError> {
        if target_sqrt_price_x96 == self.sqrt_price_x96 {
            return self.quote_perp(0);
        }
        let buy = target_sqrt_price_x96 > self.sqrt_price_x96;
        let protocol_limit = if buy {
            self.protocol_sqrt_max_x96
        } else {
            self.protocol_sqrt_min_x96
        };
        let mut limit = if buy {
            target_sqrt_price_x96.min(protocol_limit)
        } else {
            target_sqrt_price_x96.max(protocol_limit)
        };
        let mut reason = if limit == protocol_limit && target_sqrt_price_x96 != protocol_limit {
            QuoteLimit::TerminalPrice
        } else {
            QuoteLimit::TargetPrice
        };
        if constraints.enforce_price_impact {
            let bounded = if buy {
                limit.min(self.impact_sqrt_max_x96)
            } else {
                limit.max(self.impact_sqrt_min_x96)
            };
            if bounded != limit {
                reason = QuoteLimit::PriceImpact;
                limit = bounded;
            }
        }
        if (buy && limit <= self.sqrt_price_x96) || (!buy && limit >= self.sqrt_price_x96) {
            let mut quote = self.quote_perp(0)?;
            quote.requested_perp_delta = 0;
            quote.limit = QuoteLimit::PriceImpact;
            return Ok(quote);
        }

        let probe = if buy { i128::MAX } else { -i128::MAX };
        let capacity = self.simulate(probe, limit)?.perp_delta;
        let capped = constraints.max_perp.map_or(capacity, |max| {
            let max = max.min(i128::MAX as u128) as i128;
            if buy {
                capacity.min(max)
            } else {
                capacity.max(-max)
            }
        });
        if capped != capacity {
            reason = QuoteLimit::Filled;
        }
        let sim = self.simulate(capped, protocol_limit)?;
        Ok(self.finish_quote(capped, sim, reason))
    }

    /// Return a detached snapshot with a hypothetical maker liquidity change.
    pub fn with_liquidity_delta(
        &self,
        lower: i32,
        upper: i32,
        delta: i128,
    ) -> Result<Self, ValidationError> {
        if lower >= upper || delta == i128::MIN {
            return Err(ValidationError::InvalidTickRange { lower, upper });
        }
        let mut next = self.clone();
        apply_tick_delta(&mut next.ticks, lower, delta, delta)?;
        apply_tick_delta(&mut next.ticks, upper, -delta, delta)?;
        if self.tick >= lower && self.tick < upper {
            next.liquidity = add_liquidity(self.liquidity, delta)?;
        }
        Ok(next)
    }

    fn finish_quote(&self, requested: i128, sim: Simulation, mut reason: QuoteLimit) -> TakerQuote {
        let allowed = sim.sqrt_after >= self.impact_sqrt_min_x96
            && sim.sqrt_after <= self.impact_sqrt_max_x96;
        if sim.fully_filled && !allowed {
            reason = QuoteLimit::PriceImpact;
        }
        TakerQuote {
            requested_perp_delta: requested,
            perp_delta: sim.perp_delta,
            usd_delta: sim.usd_delta,
            sqrt_price_start_x96: self.sqrt_price_x96,
            sqrt_price_after_x96: sim.sqrt_after,
            tick_after: sim.tick_after,
            liquidity_after: sim.liquidity_after,
            ticks_crossed: sim.crossed,
            fully_filled: sim.fully_filled,
            price_impact_allowed: allowed,
            limit: reason,
            block_number: self.block_number,
            block_hash: self.block_hash,
        }
    }

    fn simulate(&self, requested: i128, sqrt_limit: U256) -> Result<Simulation, ValidationError> {
        let zero_for_one = requested < 0;
        let exact_amount = requested.unsigned_abs();
        let mut remaining = U256::from(exact_amount);
        let mut calculated = U256::ZERO;
        let mut sqrt = self.sqrt_price_x96;
        let mut tick = self.tick;
        let mut liquidity = self.liquidity;
        let mut crossed = Vec::new();

        while !remaining.is_zero() && sqrt != sqrt_limit {
            let next = if zero_for_one {
                self.ticks
                    .range((Unbounded, Included(tick)))
                    .next_back()
                    .map(|(&t, _)| t)
            } else {
                self.ticks
                    .range((Excluded(tick), Unbounded))
                    .next()
                    .map(|(&t, _)| t)
            };
            let next_tick = next.unwrap_or(if zero_for_one { -887_272 } else { 887_272 });
            let sqrt_next = get_sqrt_ratio_at_tick(next_tick)?;
            let target = if zero_for_one {
                sqrt_next.max(sqrt_limit)
            } else {
                sqrt_next.min(sqrt_limit)
            };

            let (sqrt_after, used, other) = if liquidity == 0 {
                (target, U256::ZERO, U256::ZERO)
            } else if zero_for_one {
                let to_target = amount0_delta(target, sqrt, liquidity, true)?;
                if remaining >= to_target {
                    (
                        target,
                        to_target,
                        amount1_delta(target, sqrt, liquidity, false)?,
                    )
                } else {
                    let after = next_sqrt_from_amount0(sqrt, liquidity, remaining, true)?;
                    (
                        after,
                        remaining,
                        amount1_delta(after, sqrt, liquidity, false)?,
                    )
                }
            } else {
                let to_target = amount0_delta(sqrt, target, liquidity, false)?;
                if remaining >= to_target {
                    (
                        target,
                        to_target,
                        amount1_delta(sqrt, target, liquidity, true)?,
                    )
                } else {
                    let after = next_sqrt_from_amount0(sqrt, liquidity, remaining, false)?;
                    (
                        after,
                        remaining,
                        amount1_delta(sqrt, after, liquidity, true)?,
                    )
                }
            };
            remaining -= used;
            calculated += other;
            let start = sqrt;
            sqrt = sqrt_after;

            if sqrt == sqrt_next {
                if let Some(info) = self.ticks.get(&next_tick) {
                    let net = if zero_for_one { -info.net } else { info.net };
                    liquidity = add_liquidity(liquidity, net)?;
                    crossed.push(next_tick);
                }
                tick = if zero_for_one {
                    next_tick - 1
                } else {
                    next_tick
                };
            } else if sqrt != start {
                tick = get_tick_at_sqrt_ratio(sqrt)?;
            }
            if used.is_zero() && other.is_zero() && sqrt == start {
                break;
            }
        }

        let filled_u = U256::from(exact_amount) - remaining;
        let filled = u256_to_i128(filled_u)?;
        let usd = u256_to_i128(calculated)?;
        Ok(Simulation {
            perp_delta: if zero_for_one { -filled } else { filled },
            usd_delta: if zero_for_one { usd } else { -usd },
            sqrt_after: sqrt,
            tick_after: tick,
            liquidity_after: liquidity,
            crossed,
            fully_filled: remaining.is_zero(),
            hit_limit: sqrt == sqrt_limit,
        })
    }
}

fn apply_tick_delta(
    ticks: &mut BTreeMap<i32, TickLiquidity>,
    tick: i32,
    net_delta: i128,
    gross_delta: i128,
) -> Result<(), ValidationError> {
    let entry = ticks.entry(tick).or_default();
    entry.net = entry
        .net
        .checked_add(net_delta)
        .ok_or_else(|| ValidationError::Overflow {
            context: "tick liquidityNet".into(),
        })?;
    if gross_delta >= 0 {
        entry.gross = entry
            .gross
            .checked_add(gross_delta as u128)
            .ok_or_else(|| ValidationError::Overflow {
                context: "tick liquidityGross".into(),
            })?;
    } else {
        entry.gross = entry
            .gross
            .checked_sub(gross_delta.unsigned_abs())
            .ok_or_else(|| ValidationError::Overflow {
                context: "tick liquidityGross".into(),
            })?;
    }
    if entry.gross == 0 {
        ticks.remove(&tick);
    }
    Ok(())
}

fn add_liquidity(liquidity: u128, delta: i128) -> Result<u128, ValidationError> {
    if delta >= 0 {
        liquidity.checked_add(delta as u128)
    } else {
        liquidity.checked_sub(delta.unsigned_abs())
    }
    .ok_or_else(|| ValidationError::Overflow {
        context: "active liquidity".into(),
    })
}

fn amount0_delta(
    a: U256,
    b: U256,
    liquidity: u128,
    round_up: bool,
) -> Result<U256, ValidationError> {
    let (lower, upper) = if a <= b { (a, b) } else { (b, a) };
    if lower.is_zero() {
        return Err(ValidationError::InvalidPrice {
            reason: "zero sqrt price".into(),
        });
    }
    let numerator1: U256 = U256::from(liquidity) << 96;
    let numerator2 = upper - lower;
    let first = mul_div(numerator1, numerator2, upper, round_up)?;
    Ok(div(first, lower, round_up))
}

fn amount1_delta(
    a: U256,
    b: U256,
    liquidity: u128,
    round_up: bool,
) -> Result<U256, ValidationError> {
    let diff = if a >= b { a - b } else { b - a };
    mul_div(U256::from(liquidity), diff, Q96, round_up)
}

fn next_sqrt_from_amount0(
    sqrt: U256,
    liquidity: u128,
    amount: U256,
    add: bool,
) -> Result<U256, ValidationError> {
    if amount.is_zero() {
        return Ok(sqrt);
    }
    let numerator: U256 = U256::from(liquidity) << 96;
    let product: U512 = amount.widening_mul(sqrt);
    let numerator_w = U512::from(numerator);
    let denominator = if add {
        numerator_w + product
    } else {
        numerator_w
            .checked_sub(product)
            .ok_or_else(|| ValidationError::Overflow {
                context: "sqrt price denominator".into(),
            })?
    };
    if denominator.is_zero() {
        return Err(ValidationError::Overflow {
            context: "zero sqrt denominator".into(),
        });
    }
    let value = div_ceil_512(numerator.widening_mul(sqrt), denominator);
    u512_to_u256(value)
}

fn mul_div(a: U256, b: U256, d: U256, round_up: bool) -> Result<U256, ValidationError> {
    if d.is_zero() {
        return Err(ValidationError::Overflow {
            context: "division by zero".into(),
        });
    }
    let product: U512 = a.widening_mul(b);
    let divisor = U512::from(d);
    let mut q = product / divisor;
    if round_up && product % divisor != U512::ZERO {
        q += U512::ONE;
    }
    u512_to_u256(q)
}

fn div(value: U256, denominator: U256, round_up: bool) -> U256 {
    let q = value / denominator;
    if round_up && value % denominator != U256::ZERO {
        q + U256::ONE
    } else {
        q
    }
}

fn div_ceil_512(value: U512, denominator: U512) -> U512 {
    let q = value / denominator;
    if value % denominator == U512::ZERO {
        q
    } else {
        q + U512::ONE
    }
}

fn u512_to_u256(value: U512) -> Result<U256, ValidationError> {
    if value > U512::from(U256::MAX) {
        return Err(ValidationError::Overflow {
            context: "U512 to U256".into(),
        });
    }
    Ok(value.to::<U256>())
}

fn u256_to_i128(value: U256) -> Result<i128, ValidationError> {
    if value > U256::from(i128::MAX as u128) {
        return Err(ValidationError::Overflow {
            context: "swap amount exceeds int128".into(),
        });
    }
    Ok(value.to::<u128>() as i128)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> TakerMarketSnapshot {
        let mut market = TakerMarketSnapshot::default();
        market.liquidity = 1_000_000_000_000;
        market.ticks.insert(
            -300,
            TickLiquidity {
                gross: market.liquidity,
                net: market.liquidity as i128,
            },
        );
        market.ticks.insert(
            300,
            TickLiquidity {
                gross: market.liquidity,
                net: -(market.liquidity as i128),
            },
        );
        market
    }

    #[test]
    fn buy_and_sell_move_price_in_expected_direction() {
        let market = book();
        let buy = market.quote_perp(1_000_000).unwrap();
        let sell = market.quote_perp(-1_000_000).unwrap();
        assert!(buy.fully_filled && sell.fully_filled);
        assert!(buy.sqrt_price_after_x96 > market.sqrt_price_x96);
        assert!(sell.sqrt_price_after_x96 < market.sqrt_price_x96);
        assert!(buy.usd_delta < 0 && sell.usd_delta > 0);
    }

    #[test]
    fn directional_amount_limits_round_safely() {
        let market = book();
        let buy = market.quote_perp(1_000_000).unwrap();
        let sell = market.quote_perp(-1_000_000).unwrap();
        assert!(buy.amt1_limit(25) >= buy.usd_delta.unsigned_abs());
        assert!(sell.amt1_limit(25) <= sell.usd_delta.unsigned_abs());
    }

    #[test]
    fn target_quote_respects_impact_bound() {
        let mut market = book();
        market.impact_sqrt_max_x96 = get_sqrt_ratio_at_tick(10).unwrap();
        let quote = market
            .quote_to_price(
                get_sqrt_ratio_at_tick(100).unwrap(),
                QuoteConstraints::default(),
            )
            .unwrap();
        assert!(quote.sqrt_price_after_x96 <= market.impact_sqrt_max_x96);
        assert!(quote.price_impact_allowed);
        assert_eq!(quote.limit, QuoteLimit::PriceImpact);
    }
}
