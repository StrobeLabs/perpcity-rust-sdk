//! Simulations with block overrides: which endpoints serve them, and the
//! plain request that tells an endpoint's refusal of the overrides from a
//! refusal of the call.
//!
//! A `v1.0.0` trade is simulated at the head's state in the next second,
//! with the block overrides as the fourth param of `eth_call` or
//! `eth_estimateGas`. An endpoint that does not take that param answers
//! `-32602` (invalid params). The router then sends the same call without
//! the overrides, at the same pinned block, to the same endpoint, and only
//! a conclusive answer to that (a result, or a revert with data) records
//! the endpoint as refusing the overrides for that method. A record lasts
//! [`REFUSAL_TTL_MS`], so an endpoint that is upgraded, or was refused by
//! a flaky backend, is asked again.

use std::sync::atomic::{AtomicU64, Ordering};

use alloy::rpc::json_rpc::{
    Request, RequestPacket, ResponsePacket, ResponsePayload, SerializedRequest,
};
use serde_json::Value;
use serde_json::value::RawValue;

use super::fault::lacks_block;

/// How long an endpoint's refusal of block overrides is believed: ten
/// minutes.
pub(crate) const REFUSAL_TTL_MS: u64 = 600_000;

/// The methods a simulation with block overrides uses, which an endpoint
/// can serve differently.
pub(crate) const METHODS: [&str; 2] = ["eth_call", "eth_estimateGas"];

/// One endpoint's refusal of block overrides for one method, recorded by
/// [`HftTransport`](super::provider::HftTransport) and read out by
/// [`HftTransport::override_refusals`](super::provider::HftTransport::override_refusals).
/// While it lasts, the transport sends no simulation with block overrides
/// for `method` to `endpoint`; when no endpoint is left, a `v1.0.0`
/// trade's automatic gas limit fails as transient.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct OverrideRefusal {
    /// The endpoint's host-only label, safe to log.
    pub endpoint: String,
    /// `eth_call` or `eth_estimateGas`.
    pub method: &'static str,
    /// Unix milliseconds at which the record expires.
    pub until_ms: u64,
}

/// Per endpoint: until when each of [`METHODS`] is refused with block
/// overrides, in Unix milliseconds (0: never refused).
#[derive(Debug, Default)]
pub(crate) struct Refusals([AtomicU64; 2]);

impl Refusals {
    /// Whether method `method` (an index into [`METHODS`]) is refused at
    /// `now_ms`.
    pub(crate) fn refuses(&self, method: usize, now_ms: u64) -> bool {
        self.0[method].load(Ordering::Relaxed) > now_ms
    }

    /// Record a refusal of `method` from `now_ms`, for [`REFUSAL_TTL_MS`].
    pub(crate) fn record(&self, method: usize, now_ms: u64) {
        self.0[method].store(now_ms + REFUSAL_TTL_MS, Ordering::Relaxed);
    }

    /// Until when `method` is refused, if it is at `now_ms`.
    pub(crate) fn until(&self, method: usize, now_ms: u64) -> Option<u64> {
        let until = self.0[method].load(Ordering::Relaxed);
        (until > now_ms).then_some(until)
    }
}

/// A simulation with block overrides, as the router routes it.
pub(crate) struct Simulation {
    /// The method's index in [`METHODS`].
    pub(crate) method: usize,
    /// The same call without the overrides, at the same pinned block.
    pub(crate) plain: RequestPacket,
}

impl Simulation {
    /// The simulation `req` is, if it is one: a single `eth_call` or
    /// `eth_estimateGas` with a fourth param, the block overrides.
    pub(crate) fn of(req: &RequestPacket) -> Option<Self> {
        let RequestPacket::Single(call) = req else {
            return None;
        };
        let method = METHODS.iter().position(|m| *m == call.method())?;
        let params: Vec<Box<RawValue>> = serde_json::from_str(call.params()?.get()).ok()?;
        let [tx, block, state, _overrides] = <[Box<RawValue>; 4]>::try_from(params).ok()?;
        let plain: SerializedRequest =
            Request::new(METHODS[method], call.id().clone(), [tx, block, state])
                .serialize()
                .ok()?;
        Some(Self {
            method,
            plain: RequestPacket::Single(plain),
        })
    }
}

/// The error `response` answers a single request with, if any.
fn error_answer(response: &ResponsePacket) -> Option<&alloy::rpc::json_rpc::ErrorPayload> {
    match response {
        ResponsePacket::Single(answer) => answer.payload.as_error(),
        ResponsePacket::Batch(_) => None,
    }
}

/// Whether `response` refuses a request's params (JSON-RPC `-32602`), as
/// an endpoint without block overrides refuses a fourth one.
pub(crate) fn refuses_params(response: &ResponsePacket) -> bool {
    error_answer(response).is_some_and(|error| error.code == -32602)
}

/// Whether `response` says the endpoint lacks the block a call was pinned
/// to ([`lacks_block`]).
pub(crate) fn lacks_pinned_block(response: &ResponsePacket) -> bool {
    error_answer(response).is_some_and(|error| lacks_block(&error.message))
}

/// Whether `response` to a plain simulation is the call's own outcome: a
/// result, or a revert that carries data. Anything else (another refusal,
/// a rate limit, an error with no data) says nothing about the overrides.
pub(crate) fn is_conclusive(response: &ResponsePacket) -> bool {
    let ResponsePacket::Single(answer) = response else {
        return false;
    };
    match &answer.payload {
        ResponsePayload::Success(_) => true,
        ResponsePayload::Failure(error) => error
            .data
            .as_ref()
            .and_then(|data| serde_json::from_str::<Value>(data.get()).ok())
            .and_then(|data| data.as_str().map(str::to_owned))
            .and_then(|data| data.strip_prefix("0x").map(str::len))
            .is_some_and(|hex_len| hex_len >= 8),
    }
}

#[cfg(test)]
mod tests {
    use alloy::rpc::json_rpc::{ErrorPayload, Id, Response};

    use super::*;

    fn request(method: &'static str, params: Value) -> RequestPacket {
        RequestPacket::Single(
            Request::new(method, Id::Number(7), params)
                .serialize()
                .unwrap(),
        )
    }

    fn answer(payload: ResponsePayload) -> ResponsePacket {
        ResponsePacket::Single(Response {
            id: Id::Number(7),
            payload,
        })
    }

    fn failure(code: i64, data: Option<&str>) -> ResponsePacket {
        answer(ResponsePayload::Failure(ErrorPayload {
            code,
            message: "m".into(),
            data: data.map(|d| RawValue::from_string(d.to_owned()).unwrap()),
        }))
    }

    #[test]
    fn a_simulation_is_a_single_call_or_estimate_with_four_params() {
        let tx = serde_json::json!({"to": "0x01", "input": "0xabcdef01"});
        let overrides = serde_json::json!({"number": "0x1f5", "time": "0x3e9"});
        for (method, index) in [("eth_call", 0), ("eth_estimateGas", 1)] {
            let req = request(method, serde_json::json!([tx, "0x1f4", {}, overrides]));
            let simulation = Simulation::of(&req).unwrap();
            assert_eq!(simulation.method, index);
            let RequestPacket::Single(plain) = &simulation.plain else {
                panic!("a single request");
            };
            assert_eq!(plain.method(), method);
            assert_eq!(plain.id(), &Id::Number(7));
            let params: Value = serde_json::from_str(plain.params().unwrap().get()).unwrap();
            assert_eq!(params, serde_json::json!([tx, "0x1f4", {}]));
        }
        for req in [
            request("eth_call", serde_json::json!([tx, "0x1f4"])),
            request("eth_call", serde_json::json!([tx, "0x1f4", {}])),
            request(
                "eth_getBalance",
                serde_json::json!([tx, "0x1f4", {}, overrides]),
            ),
        ] {
            assert!(Simulation::of(&req).is_none());
        }
    }

    #[test]
    fn only_a_result_or_revert_data_is_conclusive() {
        assert!(is_conclusive(&answer(ResponsePayload::Success(
            RawValue::from_string("\"0x\"".into()).unwrap()
        ))));
        assert!(is_conclusive(&failure(3, Some("\"0x7939f424\""))));
        assert!(is_conclusive(&failure(-32000, Some("\"0x7939f42400\""))));
        assert!(!is_conclusive(&failure(3, None)));
        assert!(!is_conclusive(&failure(3, Some("\"0x\""))));
        assert!(!is_conclusive(&failure(-32602, None)));
        assert!(!is_conclusive(&failure(-32005, Some("{\"see\":\"x\"}"))));
    }

    #[test]
    fn a_refusal_expires() {
        let refusals = Refusals::default();
        assert!(!refusals.refuses(0, 1_000));
        refusals.record(0, 1_000);
        assert!(refusals.refuses(0, 1_000 + REFUSAL_TTL_MS - 1));
        assert!(!refusals.refuses(0, 1_000 + REFUSAL_TTL_MS));
        assert!(!refusals.refuses(1, 1_000), "per method");
        assert_eq!(refusals.until(0, 2_000), Some(1_000 + REFUSAL_TTL_MS));
        assert_eq!(refusals.until(1, 2_000), None);
    }
}
