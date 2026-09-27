//! An ERC-20's `Transfer` events between address sets.

use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};
use alloy::sol_types::SolEvent;
use serde::{Deserialize, Serialize};

use crate::constants::LOG_FILTER_MAX_TOPIC_VALUES;
use crate::contracts::IERC20;
use crate::errors::{Result, ValidationError};
use crate::feeds::events::decode_raw;

use super::scan::{check_block_range, get_logs_chunked};

/// One ERC-20 `Transfer` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenTransfer {
    /// Block the transfer landed in.
    pub block_number: u64,
    /// Position of the transfer's log in its block.
    pub log_index: u64,
    /// Transaction that emitted the transfer.
    pub tx_hash: B256,
    /// Sender.
    pub from: Address,
    /// Recipient.
    pub to: Address,
    /// Amount in the token's smallest unit.
    pub value: U256,
}

/// Every `Transfer` of `token` in blocks `from_block..=to_block` whose
/// sender is in `senders` and whose recipient is in `recipients`, in chain
/// order.
///
/// `None` matches any address and `Some(&[])` matches none, so a set that
/// comes out empty reads nothing rather than every transfer. At least one
/// side must be `Some`: an unfiltered query reads every transfer the token
/// ever made. The sets go to the node as topic filters, so the scan
/// returns only matching logs, in one scan of the range.
///
/// # Errors
///
/// [`ValidationError::InvalidConfig`] for the zero token, two `None`
/// sets, or a set of more than 1,000 addresses (the most one topic position
/// of a filter holds on geth-based nodes, Arbitrum Nitro among them),
/// [`ValidationError::InvalidBlockRange`] if
/// `from_block > to_block`, [`ValidationError::DecodeFailed`] for a log
/// that does not decode as an ERC-20 `Transfer` (for example, an ERC-721
/// `Transfer`, which shares the topic but indexes its third argument), or
/// an error from [`get_logs_chunked`]. Any error fails the whole call.
pub async fn token_transfers<P: Provider>(
    provider: &P,
    token: Address,
    senders: Option<&[Address]>,
    recipients: Option<&[Address]>,
    from_block: u64,
    to_block: u64,
) -> Result<Vec<TokenTransfer>> {
    let Some(filter) = transfer_filter(token, senders, recipients, from_block, to_block)? else {
        return Ok(Vec::new());
    };
    get_logs_chunked(provider, &filter, from_block, to_block)
        .await?
        .iter()
        .map(decode_transfer)
        .collect()
}

/// The filter for [`token_transfers`]; `None` when an empty set means
/// nothing can match.
fn transfer_filter(
    token: Address,
    senders: Option<&[Address]>,
    recipients: Option<&[Address]>,
    from_block: u64,
    to_block: u64,
) -> std::result::Result<Option<Filter>, ValidationError> {
    if token.is_zero() {
        return Err(ValidationError::InvalidConfig {
            reason: "token address is zero".into(),
        });
    }
    if senders.is_none() && recipients.is_none() {
        return Err(ValidationError::InvalidConfig {
            reason: "a transfer query needs a sender or a recipient set".into(),
        });
    }
    check_block_range(from_block, to_block)?;
    let (senders, recipients) = (topic_values(senders)?, topic_values(recipients)?);
    if senders.as_ref().is_some_and(Vec::is_empty) || recipients.as_ref().is_some_and(Vec::is_empty)
    {
        return Ok(None);
    }
    let mut filter = Filter::new()
        .address(token)
        .event_signature(IERC20::Transfer::SIGNATURE_HASH);
    if let Some(senders) = senders {
        filter = filter.topic1(senders);
    }
    if let Some(recipients) = recipients {
        filter = filter.topic2(recipients);
    }
    Ok(Some(filter))
}

/// An address set as topic-filter values; `None` (any address) for no
/// set. A set over the node's per-position limit is refused, since one
/// filter cannot carry it.
fn topic_values(
    set: Option<&[Address]>,
) -> std::result::Result<Option<Vec<B256>>, ValidationError> {
    let Some(set) = set else {
        return Ok(None);
    };
    if set.len() > LOG_FILTER_MAX_TOPIC_VALUES {
        return Err(ValidationError::InvalidConfig {
            reason: format!(
                "{} addresses exceed the {LOG_FILTER_MAX_TOPIC_VALUES} a log filter position accepts",
                set.len()
            ),
        });
    }
    Ok(Some(
        set.iter().map(|address| address.into_word()).collect(),
    ))
}

fn decode_transfer(log: &Log) -> Result<TokenTransfer> {
    let decoded = log
        .block_number
        .zip(log.log_index)
        .zip(log.transaction_hash)
        .and_then(|((block, index), tx_hash)| {
            let event = decode_raw::<IERC20::Transfer>(log)?;
            Some(TokenTransfer {
                block_number: block,
                log_index: index,
                tx_hash,
                from: event.from,
                to: event.to,
                value: event.value,
            })
        });
    decoded.ok_or_else(|| {
        ValidationError::DecodeFailed {
            context: format!(
                "Transfer log from {} in tx {:?}",
                log.address(),
                log.transaction_hash
            ),
        }
        .into()
    })
}
