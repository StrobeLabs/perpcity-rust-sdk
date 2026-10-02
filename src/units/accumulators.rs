//! The market's cumulative accumulators, and the one crossing between two
//! of them.
//!
//! Each of these is a word the contract only ever adds to. A position never
//! reads the level; it reads the growth since its own checkpoint, and the
//! rule for taking that difference is not the same for all of them. Funding
//! and the utilization earnings are ordered, so a checkpoint ahead of the
//! market's cumulative is two reads that disagree and must fail. Uniswap's
//! fee growth is modular: the word is allowed to wrap, the difference is
//! still correct across the wrap, and a checked subtraction there would
//! reject a position that is perfectly healthy.
//!
//! That difference is in the signature. `since` returns a `Result` on the
//! ordered accumulators and a plain value on the modular one, so the rule
//! is not a comment a later reader can miss.
//!
//! All four are Q96 but for [`FeeGrowth`], which is Q128 because Uniswap
//! stores it so.

use alloy::primitives::{I256, U256};

use crate::constants::Q96;
use crate::errors::ValidationError;

use super::accumulator;
use super::fixed_point::{Rounding, add_i, add_u, s_full_mul_div, sub_i, sub_u};
use super::price::SqrtPrice;

accumulator! {
    /// Cumulative funding, USDC per perp token.
    ///
    /// The market keeps one of these and every position keeps a checkpoint
    /// of it; what a position owes is its perp delta times the growth
    /// between the two. A maker's band keeps three more, for the funding
    /// below it, within it, and within it per unit of sqrt-price exposure.
    Funding(I256), from_x96 / x96
}

accumulator! {
    /// Cumulative funding per unit of sqrt-price exposure.
    ///
    /// The within-band leg of a maker's funding accumulates in this form,
    /// because a maker's exposure across a band is linear in the square
    /// root of the price rather than in the price. It is the same quantity
    /// as [`Funding`] divided by a [`SqrtPrice`], and as a primitive it is
    /// the same signed Q96 word, which is why it is a type: the two are
    /// subtracted from their own checkpoints one line apart.
    FundingPerSqrtPrice(I256), from_x96 / x96
}

accumulator! {
    /// Cumulative utilization earnings, USDC per perp token of capacity.
    ///
    /// The market accrues one of these per side; a maker earns its capacity
    /// on that side times the growth since its checkpoint. Unsigned: the
    /// contract only ever adds to it.
    Earnings(U256), from_x96 / x96
}

accumulator! {
    /// Uniswap's fee growth per unit of liquidity, Q128.
    ///
    /// What a maker has earned in the quote token is its liquidity times
    /// the growth of this inside its band. Unlike the others it is modular
    /// rather than ordered; see [`Self::since`].
    FeeGrowth(U256), from_x128 / x128
}

impl Funding {
    /// The funding accrued since `checkpoint`.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] when `checkpoint` is ahead of this
    /// cumulative. That is not a negative accrual; it is a checkpoint and a
    /// market cumulative that cannot both be true, so it fails rather than
    /// settling a number no block produced.
    pub fn since(self, checkpoint: Self, context: &'static str) -> Result<Self, ValidationError> {
        Ok(Self(sub_i(self.0, checkpoint.0, context)?))
    }

    /// This cumulative advanced by `growth`, which is how a replay of the
    /// contract's accrual carries it from the market's last touch to a later
    /// block.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] when the sum leaves the word.
    pub fn advanced_by(self, growth: Self, context: &'static str) -> Result<Self, ValidationError> {
        Ok(Self(add_i(self.0, growth.0, context)?))
    }

    /// The same funding per unit of sqrt-price exposure.
    ///
    /// One of the two crossings in this module's units: these are both
    /// signed Q96 words, and using one where the other belongs is wrong by
    /// a factor of a price's square root.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] when the intermediate leaves `I256`,
    /// and the same when `sqrt_price` is zero, which no initialised pool
    /// has.
    pub fn per_sqrt_price(
        self,
        sqrt_price: SqrtPrice,
    ) -> Result<FundingPerSqrtPrice, ValidationError> {
        Ok(FundingPerSqrtPrice::from_x96(s_full_mul_div(
            self.0,
            I256::from_raw(Q96),
            sqrt_price.x96(),
            Rounding::TowardZero,
        )?))
    }
}

impl FundingPerSqrtPrice {
    /// The funding accrued since `checkpoint`, per unit of sqrt-price
    /// exposure.
    ///
    /// # Errors
    ///
    /// As [`Funding::since`].
    pub fn since(self, checkpoint: Self, context: &'static str) -> Result<Self, ValidationError> {
        Ok(Self(sub_i(self.0, checkpoint.0, context)?))
    }

    /// This cumulative advanced by `growth`.
    ///
    /// # Errors
    ///
    /// As [`Funding::advanced_by`].
    pub fn advanced_by(self, growth: Self, context: &'static str) -> Result<Self, ValidationError> {
        Ok(Self(add_i(self.0, growth.0, context)?))
    }
}

impl Earnings {
    /// The earnings accrued since `checkpoint`.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] when `checkpoint` is ahead of this
    /// cumulative, which the contract never produces: it only adds.
    pub fn since(self, checkpoint: Self, context: &'static str) -> Result<Self, ValidationError> {
        Ok(Self(sub_u(self.0, checkpoint.0, context)?))
    }

    /// This cumulative advanced by `growth`.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Overflow`] when the sum leaves the word.
    pub fn advanced_by(self, growth: Self, context: &'static str) -> Result<Self, ValidationError> {
        Ok(Self(add_u(self.0, growth.0, context)?))
    }
}

impl FeeGrowth {
    /// The fee growth since `checkpoint`.
    ///
    /// Modular, not ordered. Uniswap lets this word wrap and relies on the
    /// difference being correct across the wrap, so this cannot fail and
    /// must not: a checked subtraction here would reject a position whose
    /// band happens to straddle the wrap.
    pub fn since(self, checkpoint: Self) -> Self {
        Self(self.0.wrapping_sub(checkpoint.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An ordered accumulator refuses a checkpoint ahead of itself; the
    /// modular one subtracts across the wrap instead.
    #[test]
    fn the_difference_rule_is_the_accumulator_s_own() {
        let (small, large) = (U256::from(50u64), U256::from(100u64));
        assert!(
            Earnings::from_x96(small)
                .since(Earnings::from_x96(large), "earnings")
                .is_err()
        );
        assert!(
            Funding::from_x96(I256::unchecked_from(-1))
                .since(Funding::from_x96(I256::ZERO), "funding")
                .is_ok(),
            "signed funding may fall, it just may not disagree with a checkpoint"
        );

        // 50 − 100 in Q128 wraps to the top of the range, and that is the
        // fee growth Uniswap means.
        let wrapped = FeeGrowth::from_x128(small).since(FeeGrowth::from_x128(large));
        assert_eq!(wrapped.x128(), small.wrapping_sub(large));
        assert_eq!(wrapped.x128(), U256::MAX - U256::from(49u64));
    }

    /// The crossing divides by the root, so at a price of one it is the
    /// identity and above one it shrinks.
    #[test]
    fn funding_crosses_to_the_per_root_form() {
        let funding = Funding::from_x96(I256::from_raw(Q96));
        let at_one = funding.per_sqrt_price(SqrtPrice::from_x96(Q96)).unwrap();
        assert_eq!(at_one.x96(), I256::from_raw(Q96));

        let at_four = funding
            .per_sqrt_price(SqrtPrice::from_x96(Q96 * U256::from(4u64)))
            .unwrap();
        assert_eq!(at_four.x96(), I256::from_raw(Q96 / U256::from(4u64)));
        assert!(
            funding
                .per_sqrt_price(SqrtPrice::from_x96(U256::ZERO))
                .is_err()
        );
    }

    /// The wire form is the bare word, so a persisted accumulator reads
    /// back as the number it was.
    #[test]
    fn the_wire_form_is_the_word_alone() {
        let json = serde_json::to_string(&FeeGrowth::from_x128(U256::from(7u64))).unwrap();
        assert_eq!(json, "\"0x7\"");
        assert_eq!(
            serde_json::from_str::<FeeGrowth>(&json).unwrap().x128(),
            U256::from(7u64)
        );
    }
}
