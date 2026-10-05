//! A market's tape as a file: the raw logs, which a decoder can be rerun
//! over, and a manifest that says what they are and how to check them.
//!
//! A recording holds logs, never what was decoded from them. A decoder
//! fixed after the fact reruns over the logs; a recording of decoded rows
//! would carry the bug for good. The manifest names the chain, the market's
//! three addresses, the range, the hash of the range's last block, the
//! decoder's version and what it could not decode, so a reader knows what
//! the file is without a node, and [`Recording::check_tail`] asks a node
//! whether the chain still agrees with the file's end.

use std::fs;
use std::path::Path;
use std::result::Result as StdResult;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::primitives::{B256, keccak256};
use alloy::providers::Provider;
use alloy::rpc::types::Log;
use serde::{Deserialize, Serialize};

use crate::errors::{ContractError, Result, ValidationError};
use crate::events::decode_log;

use super::History;
use super::tape::{TapeAddresses, TapeEvent, market_logs_with, stamp_timestamps};

/// The recording format this crate writes and reads.
pub const FORMAT: u32 = 1;

/// What a recording is, stated beside its logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The file format, [`FORMAT`].
    pub format: u32,
    /// The chain the logs were read from.
    pub chain_id: u64,
    /// The market's three addresses.
    pub addresses: TapeAddresses,
    /// First block scanned, inclusive.
    pub from_block: u64,
    /// Last block scanned, inclusive.
    pub to_block: u64,
    /// The hash of `to_block`'s header when the recording was made: what a
    /// later check compares the chain against.
    pub tip_hash: B256,
    /// Unix seconds when the recording was made.
    pub recorded_at: u64,
    /// The crate that recorded it, whose decoder read these logs.
    pub crate_version: String,
    /// Logs held.
    pub logs: u64,
    /// Keccak-256 of the log file's bytes exactly as written, so a log
    /// reordered, substituted or lost is refused at read rather than
    /// decoded into a different tape.
    pub logs_hash: B256,
    /// Logs of this vocabulary that would not decode when recorded: the
    /// gaps the tape has, counted rather than dropped.
    pub undecodable: u64,
}

/// A market's raw logs over a range, with their manifest. The manifest is
/// read through [`manifest`](Self::manifest) and never written to: it
/// describes these logs, and a caller that could edit it could write a
/// file whose manifest describes other logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recording {
    manifest: Manifest,
    logs: Vec<Log>,
}

/// What a tail check found: whether the chain still ends where the
/// recording does, and whether the last blocks still hold the same logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TailCheck {
    /// The chain's header at `to_block` still has the manifest's hash.
    pub tip_hash_matches: bool,
    /// A fresh scan of the last `blocks` returns exactly the recorded logs.
    pub logs_match: bool,
    /// Blocks rescanned, from the recording's end backward.
    pub blocks: u64,
    /// Recorded logs in that span.
    pub recorded: usize,
    /// Logs the fresh scan returned in that span.
    pub rescanned: usize,
}

impl TailCheck {
    /// Both agree: the recording's end is still the chain's.
    pub fn holds(&self) -> bool {
        self.tip_hash_matches && self.logs_match
    }
}

impl<P: Provider> History<P> {
    /// Record a market's raw logs over `from_block..=to_block`, or to the
    /// lagged head, with the manifest that describes them. Each log carries
    /// its block's timestamp, read from the header when the provider omits
    /// it, so the recording decodes with no node.
    ///
    /// # Errors
    ///
    /// As [`market_tape`](super::market_tape), plus the header and chain
    /// id reads.
    pub async fn record(
        &self,
        addresses: TapeAddresses,
        from_block: u64,
        to_block: Option<u64>,
    ) -> Result<Recording> {
        let to = self.resolve(to_block).await?;
        let mut logs = market_logs_with(
            &self.provider,
            addresses,
            from_block,
            to,
            &self.widths,
            self.in_flight,
        )
        .await?;
        stamp_timestamps(&self.provider, &mut logs).await?;
        let (chain_id, tip_hash) = tokio::try_join!(
            async { self.provider.get_chain_id().await.map_err(Into::into) },
            self.header_hash(to),
        )?;
        let undecodable = logs.iter().filter(|log| decode_log(log).is_err()).count() as u64;
        let logs_hash = keccak256(lines_of(&logs)?);
        Ok(Recording {
            manifest: Manifest {
                format: FORMAT,
                chain_id,
                addresses,
                from_block,
                to_block: to,
                tip_hash,
                recorded_at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                crate_version: env!("CARGO_PKG_VERSION").to_string(),
                logs: logs.len() as u64,
                logs_hash,
                undecodable,
            },
            logs,
        })
    }

    /// The hash of `number`'s header.
    async fn header_hash(&self, number: u64) -> Result<B256> {
        let block = self
            .provider
            .get_block_by_number(number.into())
            .await?
            .ok_or(ContractError::BlockUnavailable { number })?;
        Ok(block.header.hash)
    }
}

impl Recording {
    /// What the logs are.
    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The logs, in chain order.
    pub fn logs(&self) -> &[Log] {
        &self.logs
    }

    /// The tape: every log decoded with this crate's decoder, stamped from
    /// the timestamp the recording carries. A log of another vocabulary is
    /// skipped; one of this vocabulary that will not decode is an error,
    /// since a recording is read offline and has no node to count against.
    ///
    /// # Errors
    ///
    /// [`ValidationError::DecodeFailed`] for a log this vocabulary names
    /// and cannot read, or one with no timestamp.
    pub fn tape(&self) -> Result<Vec<TapeEvent>> {
        let mut rows = Vec::with_capacity(self.logs.len());
        for log in &self.logs {
            let Some(event) = decode_log(log)? else {
                continue;
            };
            let timestamp = log
                .block_timestamp
                .ok_or_else(|| ValidationError::DecodeFailed {
                    context: format!(
                        "recorded log from {} in block {:?} has no timestamp",
                        log.address(),
                        log.block_number
                    ),
                })?;
            rows.push(TapeEvent::stamped(log, event, timestamp)?);
        }
        Ok(rows)
    }

    /// Write the recording into `dir`: `manifest.json`, and `logs.jsonl`
    /// with one log per line in chain order.
    ///
    /// # Errors
    ///
    /// The filesystem's, as [`PerpCityError::Io`](crate::PerpCityError::Io).
    pub fn write(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir)?;
        fs::write(dir.join(LOGS), lines_of(&self.logs)?)?;
        fs::write(
            dir.join(MANIFEST),
            serde_json::to_string_pretty(&self.manifest)?,
        )?;
        Ok(())
    }

    /// Read a recording [`write`](Self::write) made.
    ///
    /// # Errors
    ///
    /// The filesystem's and the parser's; [`ValidationError::DecodeFailed`]
    /// for a manifest of another [`FORMAT`], or a log file whose hash is not
    /// the manifest's: a log reordered, substituted or lost, or a manifest
    /// and a log file from two different recordings.
    pub fn read(dir: &Path) -> Result<Self> {
        let manifest: Manifest = serde_json::from_str(&fs::read_to_string(dir.join(MANIFEST))?)?;
        if manifest.format != FORMAT {
            return Err(ValidationError::DecodeFailed {
                context: format!(
                    "recording format {} is not this crate's {FORMAT}",
                    manifest.format
                ),
            }
            .into());
        }
        let bytes = fs::read(dir.join(LOGS))?;
        let hash = keccak256(&bytes);
        if hash != manifest.logs_hash {
            return Err(ValidationError::DecodeFailed {
                context: format!(
                    "the log file hashes to {hash}, not the manifest's {}: edited, truncated, or not this recording's",
                    manifest.logs_hash
                ),
            }
            .into());
        }
        let logs = String::from_utf8_lossy(&bytes)
            .lines()
            .map(serde_json::from_str::<Log>)
            .collect::<StdResult<Vec<_>, _>>()?;
        Ok(Self { manifest, logs })
    }

    /// Ask the chain whether the recording's end still stands: the header
    /// at `to_block` has the manifest's hash, and a fresh scan of the last
    /// `blocks` blocks returns exactly the logs recorded there. A reorg
    /// past the recording's end, or a node that served a short range when
    /// the recording was made, fails one or both.
    ///
    /// # Errors
    ///
    /// The scan's and the header read's.
    pub async fn check_tail<P: Provider>(
        &self,
        history: &History<P>,
        blocks: u64,
    ) -> Result<TailCheck> {
        let to = self.manifest.to_block;
        let from = to
            .saturating_sub(blocks.saturating_sub(1))
            .max(self.manifest.from_block);
        let (mut rescanned, hash) = tokio::try_join!(
            market_logs_with(
                &history.provider,
                self.manifest.addresses,
                from,
                to,
                &history.widths,
                history.in_flight,
            ),
            history.header_hash(to),
        )?;
        stamp_timestamps(&history.provider, &mut rescanned).await?;
        let recorded: Vec<&Log> = self
            .logs
            .iter()
            .filter(|log| log.block_number.is_some_and(|b| b >= from))
            .collect();
        Ok(TailCheck {
            tip_hash_matches: hash == self.manifest.tip_hash,
            logs_match: recorded.len() == rescanned.len()
                && recorded.iter().zip(&rescanned).all(|(a, b)| *a == b),
            blocks: to - from + 1,
            recorded: recorded.len(),
            rescanned: rescanned.len(),
        })
    }
}

const MANIFEST: &str = "manifest.json";
const LOGS: &str = "logs.jsonl";

/// The log file's bytes: one log per line, in chain order. The one
/// serialization, so the hash the manifest carries is of exactly what
/// [`Recording::write`] puts on disk.
fn lines_of(logs: &[Log]) -> Result<String> {
    let mut lines = String::new();
    for log in logs {
        lines.push_str(&serde_json::to_string(log)?);
        lines.push('\n');
    }
    Ok(lines)
}
