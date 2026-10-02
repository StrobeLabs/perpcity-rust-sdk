//! Exact concentrated-liquidity taker swap simulation.
//!
//! PerpCity specifies token0 (`perp`) for both directions: positive deltas are
//! exact-output buys and negative deltas are exact-input sells. The pool fee is
//! zero, so this module implements precisely those two Uniswap V4 paths using
//! integer Q64.96 arithmetic and Solidity-compatible rounding.

use std::collections::BTreeMap;
use std::ops::Bound::{Excluded, Included, Unbounded};

use alloy::primitives::{U256, U512};
use serde::{Deserialize, Serialize};

use crate::constants::{MAX_SWAP_SQRT_PRICE_X96, MIN_SWAP_SQRT_PRICE_X96, Q96};
use crate::errors::ValidationError;
use crate::math::BlockContext;
use crate::math::tick::{
    UNISWAP_MAX_TICK, UNISWAP_MIN_TICK, get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio,
};
use crate::units::fixed_point::{Rounding, mul_div, u512_to_u256};
use crate::units::{LDelta, LUnits, PerpAtoms, PerpDelta, SqrtPrice, UsdcAtoms, UsdcDelta};

/// Liquidity stored at an initialized tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickLiquidity {
    /// Total liquidity referencing the tick.
    pub gross: LUnits,
    /// Liquidity change when crossing from left to right.
    pub net: LDelta,
}

/// The constraint that stopped the quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuoteLimit {
    /// Nothing stopped it: the requested amount filled.
    Filled,
    /// The requested target price was reached.
    TargetPrice,
    /// The configured price-impact bound stopped the quote.
    PriceImpact,
    /// The protocol terminal price was reached.
    TerminalPrice,
    /// The caller's [`QuoteConstraints::max_perp`] cap bound the size.
    MaxPerp,
    /// There was not enough initialized liquidity to fill the amount.
    InsufficientLiquidity,
}

/// Constraints applied to a target-price quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteConstraints {
    /// Enforce the price-impact module's current bounds.
    pub enforce_price_impact: bool,
    /// Optional absolute cap on the perp traded.
    pub max_perp: Option<PerpAtoms>,
}

impl Default for QuoteConstraints {
    fn default() -> Self {
        Self {
            enforce_price_impact: true,
            max_perp: None,
        }
    }
}

/// The pool at one block: its price, its active liquidity, its initialized
/// ticks, and the bounds a taker swap runs within. Read by
/// [`StateAt::pool`](crate::StateAt::pool).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolSnapshot {
    /// Block containing all state in this snapshot.
    pub block: BlockContext,
    /// The pool's current price.
    pub sqrt_price: SqrtPrice,
    /// Current pool tick.
    pub tick: i32,
    /// Active liquidity at the current tick.
    pub liquidity: LUnits,
    /// Initialized ticks keyed in ascending order.
    pub ticks: BTreeMap<i32, TickLiquidity>,
    /// Minimum price the Perp swap itself can reach.
    pub protocol_sqrt_min: SqrtPrice,
    /// Maximum price the Perp swap itself can reach.
    pub protocol_sqrt_max: SqrtPrice,
    /// Current lower bound returned by the price-impact module.
    pub impact_sqrt_min: SqrtPrice,
    /// Current upper bound returned by the price-impact module.
    pub impact_sqrt_max: SqrtPrice,
}

impl Default for PoolSnapshot {
    /// Test and scaffolding convenience: the block fields and price are
    /// placeholders, not a valid pool. Real snapshots come from
    /// [`StateAt::pool`](crate::StateAt::pool).
    fn default() -> Self {
        Self {
            block: BlockContext::default(),
            sqrt_price: SqrtPrice::from_x96(Q96),
            tick: 0,
            liquidity: LUnits::ZERO,
            ticks: BTreeMap::new(),
            protocol_sqrt_min: SqrtPrice::from_x96(MIN_SWAP_SQRT_PRICE_X96),
            protocol_sqrt_max: SqrtPrice::from_x96(MAX_SWAP_SQRT_PRICE_X96),
            impact_sqrt_min: SqrtPrice::from_x96(MIN_SWAP_SQRT_PRICE_X96),
            impact_sqrt_max: SqrtPrice::from_x96(MAX_SWAP_SQRT_PRICE_X96),
        }
    }
}

/// Exact result of a local taker simulation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TakerQuote {
    /// The exposure the caller asked for.
    pub requested_perp_delta: PerpDelta,
    /// The exposure that filled.
    pub perp_delta: PerpDelta,
    /// The USDC moved: negative when paid, positive when received.
    pub usd_delta: UsdcDelta,
    /// The pool's price before the swap.
    pub sqrt_price_start: SqrtPrice,
    /// The pool's price after it.
    pub sqrt_price_after: SqrtPrice,
    /// Ending tick using V4 boundary semantics.
    pub tick_after: i32,
    /// Active liquidity after the swap.
    pub liquidity_after: LUnits,
    /// Initialized ticks crossed by the swap.
    pub ticks_crossed: Vec<i32>,
    /// Whether the complete requested amount filled.
    pub fully_filled: bool,
    /// Whether the resulting price is accepted by the price-impact module.
    pub price_impact_allowed: bool,
    /// The first constraint encountered.
    pub limit: QuoteLimit,
    /// The block the quoted snapshot was read at.
    pub block: BlockContext,
}

impl TakerQuote {
    /// Derive the contract `amt1Limit` with a directional slippage cushion.
    ///
    /// Buys return the maximum USD payment; sells return the minimum USD
    /// proceeds. `slippage_bps=25` is a 0.25% cushion.
    /// `slippage_bps` is clamped to 10 000 (100%) so a sell can never
    /// silently produce a zero minimum-proceeds limit from an oversized
    /// cushion.
    pub fn amt1_limit(&self, slippage_bps: u32) -> UsdcAtoms {
        debug_assert!(
            slippage_bps <= 10_000,
            "slippage_bps {slippage_bps} exceeds 100%"
        );
        let amount = self.usd_delta.magnitude().atoms();
        let bps = (slippage_bps as u128).min(10_000);
        UsdcAtoms::new(if self.perp_delta.atoms() > 0 {
            amount.saturating_mul(10_000 + bps).saturating_add(9_999) / 10_000
        } else {
            amount.saturating_mul(10_000 - bps) / 10_000
        })
    }

    /// Average execution price in human-readable token1/token0 units.
    pub fn effective_price(&self) -> Option<f64> {
        (!self.perp_delta.is_zero()).then(|| {
            self.usd_delta.magnitude().atoms() as f64 / self.perp_delta.magnitude().atoms() as f64
        })
    }
}

/// One swap step: the price reached, the exact-side amount consumed, and the
/// other-side amount exchanged getting there.
struct StepResult {
    sqrt_after: U256,
    used: U256,
    other: U256,
}

#[derive(Debug)]
struct Simulation {
    perp_delta: i128,
    usd_delta: i128,
    sqrt_after: U256,
    tick_after: i32,
    liquidity_after: LUnits,
    crossed: Vec<i32>,
    fully_filled: bool,
    hit_limit: bool,
}

impl PoolSnapshot {
    /// Quote an exact signed perp delta using the snapshot's protocol price
    /// limits, then evaluate the resulting price against the module bounds.
    ///
    /// The simulation reproduces the contract's per-step rounding, with one
    /// documented divergence: Uniswap V4 splits a price move at bitmap word
    /// boundaries and rounds each segment, while this simulator steps directly
    /// between initialized ticks and rounds once. When a segment with nonzero
    /// liquidity spans multiple words, on-chain amounts can exceed the local
    /// quote by a few atoms — covered by the [`TakerQuote::amt1_limit`]
    /// cushion, but not byte-exact against receipts.
    pub fn quote_perp(&self, perp_delta: PerpDelta) -> Result<TakerQuote, ValidationError> {
        let limit = if perp_delta.is_negative() {
            self.protocol_sqrt_min.x96()
        } else {
            self.protocol_sqrt_max.x96()
        };
        let sim = self.simulate(perp_delta.atoms(), limit)?;
        let reason = if sim.fully_filled {
            QuoteLimit::Filled
        } else if sim.hit_limit {
            QuoteLimit::TerminalPrice
        } else {
            QuoteLimit::InsufficientLiquidity
        };
        Ok(self.finish_quote(perp_delta.atoms(), sim, reason))
    }

    /// Size and quote the largest trade toward `target_sqrt_price` without
    /// overshooting it or an enabled price-impact bound.
    pub fn quote_to_price(
        &self,
        target_sqrt_price: SqrtPrice,
        constraints: QuoteConstraints,
    ) -> Result<TakerQuote, ValidationError> {
        if target_sqrt_price == self.sqrt_price {
            return self.quote_perp(PerpDelta::ZERO);
        }
        let (target, current) = (target_sqrt_price.x96(), self.sqrt_price.x96());
        let buy = target > current;
        let protocol_limit = if buy {
            self.protocol_sqrt_max.x96()
        } else {
            self.protocol_sqrt_min.x96()
        };
        let mut limit = if buy {
            target.min(protocol_limit)
        } else {
            target.max(protocol_limit)
        };
        let mut reason = if limit == protocol_limit && target != protocol_limit {
            QuoteLimit::TerminalPrice
        } else {
            QuoteLimit::TargetPrice
        };
        if constraints.enforce_price_impact {
            let bounded = if buy {
                limit.min(self.impact_sqrt_max.x96())
            } else {
                limit.max(self.impact_sqrt_min.x96())
            };
            if bounded != limit {
                reason = QuoteLimit::PriceImpact;
                limit = bounded;
            }
        }
        if (buy && limit <= current) || (!buy && limit >= current) {
            let mut quote = self.quote_perp(PerpDelta::ZERO)?;
            quote.requested_perp_delta = PerpDelta::ZERO;
            quote.limit = QuoteLimit::PriceImpact;
            return Ok(quote);
        }

        let probe = if buy { i128::MAX } else { -i128::MAX };
        let capacity = self.simulate(probe, limit)?.perp_delta;
        let capped = constraints.max_perp.map_or(capacity, |max| {
            let max = max.atoms().min(i128::MAX as u128) as i128;
            if buy {
                capacity.min(max)
            } else {
                capacity.max(-max)
            }
        });
        if capped != capacity {
            reason = QuoteLimit::MaxPerp;
        }
        let sim = self.simulate(capped, protocol_limit)?;
        Ok(self.finish_quote(capped, sim, reason))
    }

    /// Return a detached snapshot with a hypothetical maker liquidity change.
    pub fn with_liquidity_delta(
        &self,
        lower: i32,
        upper: i32,
        delta: LDelta,
    ) -> Result<Self, ValidationError> {
        if lower >= upper {
            return Err(ValidationError::InvalidTickRange { lower, upper });
        }
        // The upper tick carries the negation of the lower tick's change,
        // which is where `i128::MIN` is refused.
        let removed = delta.negated()?;
        let mut next = self.clone();
        apply_tick_delta(&mut next.ticks, lower, delta, delta)?;
        apply_tick_delta(&mut next.ticks, upper, removed, delta)?;
        if self.tick >= lower && self.tick < upper {
            next.liquidity = self
                .liquidity
                .checked_add_signed(delta, "active liquidity")?;
        }
        Ok(next)
    }

    fn finish_quote(&self, requested: i128, sim: Simulation, mut reason: QuoteLimit) -> TakerQuote {
        let allowed = sim.sqrt_after >= self.impact_sqrt_min.x96()
            && sim.sqrt_after <= self.impact_sqrt_max.x96();
        if sim.fully_filled && !allowed {
            reason = QuoteLimit::PriceImpact;
        }
        TakerQuote {
            requested_perp_delta: PerpDelta::new(requested),
            perp_delta: PerpDelta::new(sim.perp_delta),
            usd_delta: UsdcDelta::new(sim.usd_delta),
            sqrt_price_start: self.sqrt_price,
            sqrt_price_after: SqrtPrice::from_x96(sim.sqrt_after),
            tick_after: sim.tick_after,
            liquidity_after: sim.liquidity_after,
            ticks_crossed: sim.crossed,
            fully_filled: sim.fully_filled,
            price_impact_allowed: allowed,
            limit: reason,
            block: self.block,
        }
    }

    fn simulate(&self, requested: i128, sqrt_limit: U256) -> Result<Simulation, ValidationError> {
        let zero_for_one = requested < 0;
        let exact_amount = requested.unsigned_abs();
        let mut remaining = U256::from(exact_amount);
        let mut calculated = U256::ZERO;
        let mut sqrt = self.sqrt_price.x96();
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
            let next_tick = next.unwrap_or(if zero_for_one {
                UNISWAP_MIN_TICK
            } else {
                UNISWAP_MAX_TICK
            });
            let sqrt_next = get_sqrt_ratio_at_tick(next_tick)?.x96();
            let target = if zero_for_one {
                sqrt_next.max(sqrt_limit)
            } else {
                sqrt_next.min(sqrt_limit)
            };

            let StepResult {
                sqrt_after,
                used,
                other,
            } = if liquidity.is_zero() {
                StepResult {
                    sqrt_after: target,
                    used: U256::ZERO,
                    other: U256::ZERO,
                }
            } else if zero_for_one {
                let l = liquidity.units();
                let to_target = amount0_delta(target, sqrt, l, Rounding::Up)?;
                if remaining >= to_target {
                    StepResult {
                        sqrt_after: target,
                        used: to_target,
                        other: amount1_delta(target, sqrt, l, Rounding::TowardZero)?,
                    }
                } else {
                    let after = next_sqrt_from_amount0(sqrt, l, remaining, true)?;
                    StepResult {
                        sqrt_after: after,
                        used: remaining,
                        other: amount1_delta(after, sqrt, l, Rounding::TowardZero)?,
                    }
                }
            } else {
                let l = liquidity.units();
                let to_target = amount0_delta(sqrt, target, l, Rounding::TowardZero)?;
                if remaining >= to_target {
                    StepResult {
                        sqrt_after: target,
                        used: to_target,
                        other: amount1_delta(sqrt, target, l, Rounding::Up)?,
                    }
                } else {
                    let after = next_sqrt_from_amount0(sqrt, l, remaining, false)?;
                    StepResult {
                        sqrt_after: after,
                        used: remaining,
                        other: amount1_delta(sqrt, after, l, Rounding::Up)?,
                    }
                }
            };
            remaining -= used;
            calculated += other;
            let start = sqrt;
            sqrt = sqrt_after;

            let mut crossed_this_step = false;
            if sqrt == sqrt_next {
                if let Some(info) = self.ticks.get(&next_tick) {
                    let net = if zero_for_one {
                        info.net.negated()?
                    } else {
                        info.net
                    };
                    liquidity = liquidity.checked_add_signed(net, "active liquidity")?;
                    crossed.push(next_tick);
                    crossed_this_step = true;
                }
                tick = if zero_for_one {
                    next_tick - 1
                } else {
                    next_tick
                };
            } else if sqrt != start {
                tick = get_tick_at_sqrt_ratio(SqrtPrice::from_x96(sqrt))?;
            }
            // A zero-amount step that crossed a tick is progress (the price
            // sat exactly on an initialized boundary); only a step that moved
            // nothing AND crossed nothing means the liquidity is exhausted.
            if used.is_zero() && other.is_zero() && sqrt == start && !crossed_this_step {
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

/// The active liquidity a tick map implies at `tick`: the net of every
/// initialized tick at or below it. A map read from a pool must reproduce
/// the liquidity the pool reports, or it is not that pool's.
///
/// # Errors
///
/// [`ValidationError::Overflow`] when the running sum leaves `u128`, which
/// a consistent map cannot do.
pub(crate) fn active_liquidity(
    ticks: &BTreeMap<i32, TickLiquidity>,
    tick: i32,
) -> Result<LUnits, ValidationError> {
    ticks
        .range(..=tick)
        .try_fold(LUnits::ZERO, |active, (_, info)| {
            active.checked_add_signed(info.net, "reconstructing active liquidity")
        })
}

fn apply_tick_delta(
    ticks: &mut BTreeMap<i32, TickLiquidity>,
    tick: i32,
    net_delta: LDelta,
    gross_delta: LDelta,
) -> Result<(), ValidationError> {
    let entry = ticks.entry(tick).or_default();
    entry.net = entry.net.checked_add(net_delta, "tick liquidityNet")?;
    entry.gross = entry
        .gross
        .checked_add_signed(gross_delta, "tick liquidityGross")?;
    if entry.gross.is_zero() {
        ticks.remove(&tick);
    }
    Ok(())
}

/// Uniswap `SqrtPriceMath.getAmount0Delta`: token0 owed between two sqrt
/// prices for `liquidity`, with Solidity-compatible rounding.
pub(crate) fn amount0_delta(
    a: U256,
    b: U256,
    liquidity: u128,
    rounding: Rounding,
) -> Result<U256, ValidationError> {
    let (lower, upper) = if a <= b { (a, b) } else { (b, a) };
    if lower.is_zero() {
        return Err(ValidationError::InvalidPrice {
            reason: "zero sqrt price".into(),
        });
    }
    let numerator1: U256 = U256::from(liquidity) << 96;
    let numerator2 = upper - lower;
    let first = mul_div(numerator1, numerator2, upper, rounding)?;
    Ok(div(first, lower, rounding))
}

/// Uniswap `SqrtPriceMath.getAmount1Delta`: token1 owed between two sqrt
/// prices for `liquidity`, with Solidity-compatible rounding.
pub(crate) fn amount1_delta(
    a: U256,
    b: U256,
    liquidity: u128,
    rounding: Rounding,
) -> Result<U256, ValidationError> {
    let diff = if a >= b { a - b } else { b - a };
    mul_div(U256::from(liquidity), diff, Q96, rounding)
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
    let value = numerator.widening_mul(sqrt).div_ceil(denominator);
    u512_to_u256(value)
}

fn div(value: U256, denominator: U256, rounding: Rounding) -> U256 {
    match rounding {
        Rounding::Up => value.div_ceil(denominator),
        Rounding::TowardZero => value / denominator,
    }
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

    /// Liquidity standing at a tick, gross then net.
    fn tick_liquidity(gross: u128, net: i128) -> TickLiquidity {
        TickLiquidity {
            gross: LUnits::new(gross),
            net: LDelta::new(net),
        }
    }

    /// A depth in the pool's units.
    fn l(units: u128) -> LUnits {
        LUnits::new(units)
    }

    /// Only ticks at or below the current one count, each by its net.
    #[test]
    fn active_liquidity_is_the_net_at_or_below_the_tick() {
        let ticks = BTreeMap::from([
            (-60, tick_liquidity(5, 5)),
            (0, tick_liquidity(3, -2)),
            (60, tick_liquidity(7, 7)),
        ]);
        assert_eq!(active_liquidity(&ticks, -61).unwrap(), l(0));
        assert_eq!(active_liquidity(&ticks, -60).unwrap(), l(5));
        assert_eq!(active_liquidity(&ticks, 0).unwrap(), l(3));
        assert_eq!(active_liquidity(&ticks, 59).unwrap(), l(3));
        assert_eq!(active_liquidity(&ticks, 60).unwrap(), l(10));
    }

    /// More liquidity leaving than ever entered is a map that cannot be a
    /// pool's, not a wrapped sum.
    #[test]
    fn a_net_below_zero_is_an_overflow() {
        let ticks = BTreeMap::from([(0, tick_liquidity(1, -1))]);
        assert!(matches!(
            active_liquidity(&ticks, 0),
            Err(ValidationError::Overflow { .. })
        ));
    }

    /// An exposure in perp atoms: positive long, negative short.
    fn perp(atoms: i128) -> PerpDelta {
        PerpDelta::new(atoms)
    }

    fn pool() -> PoolSnapshot {
        let depth: u128 = 1_000_000_000_000;
        let mut market = PoolSnapshot {
            liquidity: l(depth),
            ..Default::default()
        };
        market
            .ticks
            .insert(-300, tick_liquidity(depth, depth as i128));
        market
            .ticks
            .insert(300, tick_liquidity(depth, -(depth as i128)));
        market
    }

    #[test]
    fn buy_and_sell_move_price_in_expected_direction() {
        let market = pool();
        let buy = market.quote_perp(perp(1_000_000)).unwrap();
        let sell = market.quote_perp(perp(-1_000_000)).unwrap();
        assert!(buy.fully_filled && sell.fully_filled);
        assert!(buy.sqrt_price_after > market.sqrt_price);
        assert!(sell.sqrt_price_after < market.sqrt_price);
        assert!(buy.usd_delta < UsdcDelta::ZERO && sell.usd_delta > UsdcDelta::ZERO);
    }

    #[test]
    fn directional_amount_limits_round_safely() {
        let market = pool();
        let buy = market.quote_perp(perp(1_000_000)).unwrap();
        let sell = market.quote_perp(perp(-1_000_000)).unwrap();
        assert!(buy.amt1_limit(25) >= buy.usd_delta.magnitude());
        assert!(sell.amt1_limit(25) <= sell.usd_delta.magnitude());
    }

    /// A pool whose current price sits exactly on the initialized tick at 0:
    /// −600 (+L), 0 (+M), 600 (−(L+M)).
    fn pool_at_boundary(tick: i32) -> PoolSnapshot {
        let lower: u128 = 1_000_000_000_000;
        let middle: u128 = 500_000_000_000;
        // Active liquidity is the sum of net for initialized ticks <= tick.
        let active = if tick >= 0 { lower + middle } else { lower };
        let mut market = PoolSnapshot {
            sqrt_price: get_sqrt_ratio_at_tick(0).unwrap(),
            tick,
            liquidity: l(active),
            ..Default::default()
        };
        market
            .ticks
            .insert(-600, tick_liquidity(lower, lower as i128));
        market
            .ticks
            .insert(0, tick_liquidity(middle, middle as i128));
        market.ticks.insert(
            600,
            tick_liquidity(lower + middle, -((lower + middle) as i128)),
        );
        market
    }

    #[test]
    fn a_sell_from_a_price_exactly_on_a_tick_boundary_keeps_filling() {
        // tick == 0 and sqrt == ratio(0): the first step crosses tick 0 with
        // zero amounts. The swap must continue into the liquidity below
        // instead of reporting the liquidity exhausted.
        let market = pool_at_boundary(0);
        let sell = market.quote_perp(perp(-1_000_000)).unwrap();
        assert!(sell.fully_filled, "limit={:?}", sell.limit);
        assert_eq!(sell.limit, QuoteLimit::Filled);
        assert_eq!(sell.perp_delta, perp(-1_000_000));
        assert!(sell.usd_delta > UsdcDelta::ZERO);
        assert_eq!(sell.ticks_crossed, vec![0]);
        assert!(sell.sqrt_price_after < market.sqrt_price);
    }

    #[test]
    fn a_buy_from_a_price_exactly_on_a_tick_boundary_keeps_filling() {
        // tick == −1 with sqrt == ratio(0): the state a sell leaves behind
        // when it stops exactly on the boundary. A buy's first step crosses
        // tick 0 upward with zero amounts and must keep going.
        let market = pool_at_boundary(-1);
        let buy = market.quote_perp(perp(1_000_000)).unwrap();
        assert!(buy.fully_filled, "limit={:?}", buy.limit);
        assert_eq!(buy.limit, QuoteLimit::Filled);
        assert_eq!(buy.perp_delta, perp(1_000_000));
        assert!(buy.usd_delta < UsdcDelta::ZERO);
        assert_eq!(buy.ticks_crossed, vec![0]);
        assert!(buy.sqrt_price_after > market.sqrt_price);
    }

    #[test]
    fn a_binding_max_perp_cap_is_reported_as_max_perp() {
        let market = pool();
        let uncapped = market
            .quote_to_price(
                get_sqrt_ratio_at_tick(100).unwrap(),
                QuoteConstraints {
                    enforce_price_impact: false,
                    max_perp: None,
                },
            )
            .unwrap();
        assert!(uncapped.perp_delta > perp(1));

        let cap = PerpAtoms::new(uncapped.perp_delta.magnitude().atoms() / 2);
        let capped = market
            .quote_to_price(
                get_sqrt_ratio_at_tick(100).unwrap(),
                QuoteConstraints {
                    enforce_price_impact: false,
                    max_perp: Some(cap),
                },
            )
            .unwrap();
        assert_eq!(capped.limit, QuoteLimit::MaxPerp);
        assert_eq!(capped.perp_delta.magnitude(), cap);
        assert!(capped.sqrt_price_after < uncapped.sqrt_price_after);
    }

    #[test]
    fn target_quote_respects_impact_bound() {
        let mut market = pool();
        market.impact_sqrt_max = get_sqrt_ratio_at_tick(10).unwrap();
        let quote = market
            .quote_to_price(
                get_sqrt_ratio_at_tick(100).unwrap(),
                QuoteConstraints::default(),
            )
            .unwrap();
        assert!(quote.sqrt_price_after <= market.impact_sqrt_max);
        assert!(quote.price_impact_allowed);
        assert_eq!(quote.limit, QuoteLimit::PriceImpact);
    }
}
