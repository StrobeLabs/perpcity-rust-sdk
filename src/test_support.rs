//! An in-memory JSON-RPC node for testing the log readers.
//!
//! [`FakeNode`] answers `eth_getLogs` from a fixed log set, rejects any
//! range wider than its span limit or holding more logs than its result
//! limit the way a capped provider does, and records every range it was
//! asked for, so tests can check that a scan covers its range exactly once.

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use alloy::network::Ethereum;
use alloy::primitives::{Address, B256, Bytes, Log as PrimitiveLog, LogData};
use alloy::providers::RootProvider;
use alloy::rpc::client::RpcClient;
use alloy::rpc::json_rpc::{
    ErrorPayload, RequestPacket, Response, ResponsePacket, ResponsePayload,
};
use alloy::rpc::types::{Block, Filter, Log};
use alloy::transports::{TransportError, TransportErrorKind, TransportFut};
use serde_json::value::RawValue;
use tower::Service;

/// How the node answers an `eth_getLogs` range it accepts.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Mode {
    /// Serve the logs in range.
    Serve,
    /// Fail every request with HTTP 429.
    RateLimited,
}

#[derive(Debug, Default)]
struct State {
    requests: Vec<(u64, u64)>,
    header_reads: Vec<u64>,
}

/// In-memory node. Clones share the request log.
#[derive(Debug, Clone)]
pub(crate) struct FakeNode {
    logs: Arc<Vec<Log>>,
    max_span: u64,
    max_results: usize,
    head: u64,
    mode: Mode,
    state: Arc<Mutex<State>>,
}

impl FakeNode {
    /// A node holding `logs` that accepts ranges of at most `max_span`
    /// blocks.
    pub(crate) fn new(logs: Vec<Log>, max_span: u64) -> Self {
        Self {
            logs: Arc::new(logs),
            max_span,
            max_results: usize::MAX,
            head: 0,
            mode: Mode::Serve,
            state: Arc::default(),
        }
    }

    /// Set the block number `eth_blockNumber` reports.
    pub(crate) fn with_head(mut self, head: u64) -> Self {
        self.head = head;
        self
    }

    /// Reject any range that holds more than `max_results` logs.
    pub(crate) fn with_max_results(mut self, max_results: usize) -> Self {
        self.max_results = max_results;
        self
    }

    pub(crate) fn with_mode(mut self, mode: Mode) -> Self {
        self.mode = mode;
        self
    }

    pub(crate) fn provider(&self) -> RootProvider<Ethereum> {
        RootProvider::new(RpcClient::new(self.clone(), true))
    }

    /// Every `eth_getLogs` range requested, in order.
    pub(crate) fn requests(&self) -> Vec<(u64, u64)> {
        self.state.lock().unwrap().requests.clone()
    }

    /// Every block whose header was read, in order.
    pub(crate) fn header_reads(&self) -> Vec<u64> {
        self.state.lock().unwrap().header_reads.clone()
    }

    fn answer(
        &self,
        method: &str,
        params: Option<&RawValue>,
    ) -> Result<ResponsePayload, TransportError> {
        match method {
            "eth_blockNumber" => Ok(success(&format!("0x{:x}", self.head))),
            "eth_getBlockByNumber" => {
                let (tag, _full): (String, bool) =
                    serde_json::from_str(params.unwrap().get()).unwrap();
                let number = parse_quantity(&tag);
                self.state.lock().unwrap().header_reads.push(number);
                let mut block = Block::<B256>::default();
                block.header.inner.number = number;
                block.header.inner.timestamp = timestamp_of(number);
                Ok(success(&block))
            }
            "eth_getLogs" => {
                let (filter,): (Filter,) = serde_json::from_str(params.unwrap().get()).unwrap();
                let from = filter.get_from_block().unwrap();
                let to = filter.get_to_block().unwrap();
                self.state.lock().unwrap().requests.push((from, to));
                if let Mode::RateLimited = self.mode {
                    return Err(TransportErrorKind::http_error(429, "rate limited".into()));
                }
                if to - from + 1 > self.max_span {
                    return Ok(ResponsePayload::Failure(ErrorPayload {
                        code: -32_602,
                        message: format!("block range exceeds {}", self.max_span).into(),
                        data: None,
                    }));
                }
                let logs: Vec<&Log> = self
                    .logs
                    .iter()
                    .filter(|log| {
                        let block = log.block_number.unwrap();
                        (from..=to).contains(&block)
                            && filter.matches_address(log.address())
                            && filter.matches_topics(log.topics())
                    })
                    .collect();
                if logs.len() > self.max_results {
                    return Ok(ResponsePayload::Failure(ErrorPayload {
                        code: -32_005,
                        message: format!("query returned more than {} results", self.max_results)
                            .into(),
                        data: None,
                    }));
                }
                Ok(success(&logs))
            }
            other => panic!("FakeNode does not serve {other}"),
        }
    }
}

impl Service<RequestPacket> for FakeNode {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        let RequestPacket::Single(req) = req else {
            panic!("FakeNode does not serve batches");
        };
        let answer = self.answer(req.method(), req.params());
        let id = req.id().clone();
        Box::pin(
            async move { answer.map(|payload| ResponsePacket::Single(Response { id, payload })) },
        )
    }
}

/// The timestamp the node reports for a block.
pub(crate) fn timestamp_of(block: u64) -> u64 {
    1_700_000_000 + block / 4
}

/// A mined log from `address` with one topic, the given data, and no
/// block timestamp.
pub(crate) fn mined_log(
    address: Address,
    topic0: B256,
    data: Bytes,
    block: u64,
    index: u64,
) -> Log {
    Log {
        inner: PrimitiveLog {
            address,
            data: LogData::new_unchecked(vec![topic0], data),
        },
        block_hash: Some(B256::with_last_byte(1)),
        block_number: Some(block),
        block_timestamp: None,
        transaction_hash: Some(B256::with_last_byte(2)),
        transaction_index: Some(0),
        log_index: Some(index),
        removed: false,
    }
}

fn success<T: serde::Serialize>(value: &T) -> ResponsePayload {
    ResponsePayload::Success(serde_json::value::to_raw_value(value).unwrap())
}

fn parse_quantity(hex: &str) -> u64 {
    u64::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap()
}
