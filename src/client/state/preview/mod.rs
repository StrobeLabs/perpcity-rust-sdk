//! Block-pinned v1 liquidation previews, close capacity and collateral totals.

use alloy::primitives::U256;
use alloy::sol_types::{SolCall, SolError};

use crate::contracts::v1;
use crate::errors::Result;
use crate::math::capacity::Capacity;
use crate::units::PerpAtoms;

use super::{RowOutcome, StateAt, decode_exact, decode_exact_row};

/// V1 collateral cap and accounted collateral, in USDC atoms at one block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TvlCap {
    /// Maximum accounted collateral.
    pub cap: U256,
    /// Collateral already counted against the cap.
    pub accounted: U256,
}

impl TvlCap {
    /// Whether the resulting accounted TVL fits the cap.
    pub fn allows_deposit(&self, amount: U256) -> bool {
        self.accounted
            .checked_add(amount)
            .is_some_and(|total| total <= self.cap)
    }
}

impl StateAt {
    /// Read the v1 liquidation preview at this block.
    pub async fn preview_position(&self, pos_id: U256) -> Result<v1::PositionPreview> {
        self.require_read_era(true, "preview_position").await?;
        let bytes = v1::Perp::new(self.market.perp, self.market.chain.provider())
            .previewPosition(pos_id)
            .block(self.id())
            .call_raw()
            .await
            .map_err(|e| self.read_error(e))?;
        decode_exact::<v1::Perp::previewPositionCall>(pos_id, &bytes, "previewPosition")
    }

    /// Read v1 previews in input order. A missing position has no row.
    pub async fn preview_positions(
        &self,
        pos_ids: &[U256],
    ) -> Result<Vec<RowOutcome<v1::PositionPreview>>> {
        if pos_ids.is_empty() {
            return Ok(Vec::new());
        }
        self.require_read_era(true, "preview_positions").await?;
        Ok(self
            .rows(
                pos_ids,
                |pos_id| [v1::Perp::previewPositionCall { posId: pos_id }.abi_encode()],
                |id, [row]| {
                    if !row.success
                        && row.returnData[..] == v1::Perp::PositionDoesNotExist::SELECTOR
                    {
                        return Ok(None);
                    }
                    if !row.success {
                        return Err(crate::client::chain::row_revert(&row.returnData));
                    }
                    decode_exact_row::<v1::Perp::previewPositionCall>(id, row, "previewPosition")
                        .map(Some)
                },
            )
            .await)
    }

    /// Physical long-close and short-close depth at this block, in perp atoms.
    pub async fn physical_close_capacity(&self) -> Result<Capacity> {
        self.require_read_era(true, "physical_close_capacity")
            .await?;
        let depth = v1::Perp::new(self.market.perp, self.market.chain.provider())
            .physicalCloseCapacity()
            .block(self.id())
            .call()
            .await
            .map_err(|e| self.read_error(e))?;
        Ok(Capacity::new(
            PerpAtoms::new(depth.longClose),
            PerpAtoms::new(depth.shortClose),
        ))
    }

    /// Read the two collateral totals at this block.
    pub async fn tvl_cap(&self) -> Result<TvlCap> {
        self.require_read_era(true, "tvl_cap").await?;
        let perp = v1::Perp::new(self.market.perp, self.market.chain.provider());
        let (cap, accounted) = self
            .market
            .chain
            .multicall_at(self.id())
            .add(perp.tvlCap())
            .add(perp.accountedTvl())
            .aggregate()
            .await
            .map_err(|e| self.multicall_read_error(e))?;
        Ok(TvlCap { cap, accounted })
    }
}
