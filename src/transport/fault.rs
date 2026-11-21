//! Whose fault a failed request was.
//!
//! The transport has two reasons to care: whether to count the failure
//! against an endpoint's health (and so eventually stop routing to it),
//! and whether repeating the request could succeed. Both follow from one
//! question — did the endpoint fail, or did it answer by declining?
//!
//! [`crate::history`]'s scan asks a different question of the same errors
//! ("would a narrower block range pass?") and answers it in
//! `history::scan::classify`. The two taxonomies agree on which statuses
//! mean the endpoint is in trouble; they must, since a range rejection is
//! the scan's ordinary signal and no reason to take an endpoint out of
//! service.

use alloy::rpc::json_rpc::ErrorPayload;
use alloy::transports::{RpcError, TransportError, TransportErrorKind};
use serde_json::Value;

use crate::errors::decode::extract_revert_data;

/// What a failed request is evidence about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    /// The endpoint failed: no answer, a dropped connection, a rate
    /// limit, a server error, or a refusal to serve this caller at all.
    /// Counts against its health, and another endpoint may do better.
    Endpoint,
    /// The endpoint answered, declining the request: a block range too
    /// wide, a payload too large, a call it will not parse. It is
    /// working — every endpoint would decline the same request, and
    /// repeating it earns the same answer.
    Request,
}

/// Whose fault `error` was.
///
/// A rate limit is the endpoint's: it is a statement about capacity, not
/// about the request, and waiting or moving elsewhere helps. So is an
/// auth refusal, which is specific to the credentials this endpoint
/// carries. Every other client error is the request's — including the
/// `4xx` that providers use for `eth_getLogs` ranges they consider too
/// wide, which is the scan's ordinary narrowing signal.
pub(crate) fn fault(error: &TransportError) -> Fault {
    match error {
        // An error *response* means the endpoint answered. Only a rate
        // limit says anything about the endpoint itself.
        RpcError::ErrorResp(payload) => {
            if is_rate_limit(payload) {
                Fault::Endpoint
            } else {
                Fault::Request
            }
        }
        RpcError::Transport(TransportErrorKind::HttpError(http)) => match http.status {
            // Overloaded, or timed out before it could answer.
            408 | 429 | 503 => Fault::Endpoint,
            // Credentials belong to the endpoint, not to the request.
            401 | 403 => Fault::Endpoint,
            500..=599 => Fault::Endpoint,
            _ => Fault::Request,
        },
        // A request this transport could not serialize never reached the
        // endpoint, and serializing it again will fail the same way.
        RpcError::SerError(_) => Fault::Request,
        // A response that will not parse is a malformed answer — a proxy's
        // error page, a truncated body — which another endpoint may not
        // give. A caller asking for a type the node words differently
        // lands here too, and pays one retry for the ambiguity.
        RpcError::DeserError { .. } => Fault::Endpoint,
        RpcError::UnsupportedFeature(_) => Fault::Request,
        // Missing batch responses, a gone backend, an unavailable
        // subscription, a local usage error, a null where a value was
        // required: the endpoint did not deliver.
        _ => Fault::Endpoint,
    }
}

/// Whether a JSON-RPC error is a rate limit, per alloy's retry rules.
///
/// An execution error ([`is_execution_error`]) is never one, whatever its
/// message says: a contract can revert with the words "rate limit", and
/// every endpoint returns the same revert.
///
/// Alloy calls every `-32005` retryable, since Infura returns it for
/// `exceeded project rate limit` — but Infura also returns it for
/// `query returned more than 10000 results`, a range rejection that the
/// log scan narrows and that no endpoint would answer differently. So for
/// that code the message decides.
pub(crate) fn is_rate_limit(payload: &ErrorPayload) -> bool {
    if is_execution_error(payload) {
        return false;
    }
    is_rate_limit_reply(payload)
}

fn is_rate_limit_reply(payload: &ErrorPayload) -> bool {
    let message = payload.message.to_ascii_lowercase();
    if payload.code == -32_005
        && ["block range", "query returned", "log response size"]
            .iter()
            .any(|wording| message.contains(wording))
    {
        return false;
    }

    // Use Alloy's vendor codes and wordings with normalized case. For
    // -32005, require evidence of capacity in the message.
    let capacity_limit = message.contains("capacity exceeded")
        || message.contains("requests limited to")
        || message.contains("compute units per second capacity")
        || message.contains("concurrent requests")
        || message.contains("daily request count exceeded")
        || (message.contains("exceeded") && message.contains("requests per second"))
        || (payload.code == -32_005
            && ["limit exceeded", "exceeded the limit", "quota"]
                .iter()
                .any(|wording| message.contains(wording)));
    capacity_limit
        || ErrorPayload::<()> {
            code: if payload.code == -32_005 {
                0
            } else {
                payload.code
            },
            message: message.into(),
            data: None,
        }
        .is_retry_err()
}

/// Evidence that a call ran or was stopped by the EVM.
pub(crate) enum ExecutionEvidence {
    Revert(Option<String>),
    OutOfGas,
    Halt,
}

pub(crate) fn is_execution_error(payload: &ErrorPayload) -> bool {
    execution_evidence(payload).is_some()
}

/// Read execution evidence from the answer and recognized error envelopes.
pub(crate) fn execution_evidence(resp: &ErrorPayload) -> Option<ExecutionEvidence> {
    fn message_reverts(message: &str) -> bool {
        let message = message.to_ascii_lowercase();
        [
            "execution reverted",
            "transaction reverted without a reason string",
            "reverted with reason string",
            "reverted with custom error",
            "reverted with panic code",
            "vm exception while processing transaction: revert",
            "reverted 0x",
        ]
        .iter()
        .any(|marker| message.contains(marker))
    }

    fn message_evidence(message: &str) -> Option<ExecutionEvidence> {
        let message = message.to_ascii_lowercase();
        if message.contains("out of gas") || message.contains("gas required exceeds") {
            return Some(ExecutionEvidence::OutOfGas);
        }
        [
            "vm exception",
            "vm execution error",
            "invalid opcode",
            "invalid jump",
            "stack underflow",
            "stack limit",
            "write protection",
            "return data out of bounds",
        ]
        .iter()
        .any(|wording| message.contains(wording))
        .then_some(ExecutionEvidence::Halt)
    }

    #[derive(Default)]
    struct Evidence {
        revert: bool,
        data: Option<String>,
        halt: Option<ExecutionEvidence>,
    }

    impl Evidence {
        fn read_hex(&mut self, text: &str) {
            let text = text.trim();
            let hex = text
                .get(..9)
                .filter(|prefix| prefix.eq_ignore_ascii_case("reverted "))
                .map_or(text, |_| text[9..].trim_start());
            if let Some(digits) = hex.strip_prefix("0x")
                && !digits.is_empty()
                && digits.len().is_multiple_of(2)
                && digits.bytes().all(|digit| digit.is_ascii_hexdigit())
                && self.data.as_ref().is_none_or(|old| hex.len() > old.len())
            {
                self.data = Some(hex.to_owned());
            }
        }

        fn read_error(&mut self, code: Option<i64>, message: Option<&str>) -> bool {
            let explicit_revert = code == Some(3) || message.is_some_and(message_reverts);
            if explicit_revert {
                self.revert = true;
                if let Some(message) = message {
                    self.read_hex(extract_revert_data(message).unwrap_or(message));
                }
            } else if self.halt.is_none() {
                self.halt = message.and_then(message_evidence);
            }
            explicit_revert
        }

        fn read_data(&mut self, value: &Value, explicit_revert: bool) {
            if let Value::String(text) = value {
                if explicit_revert {
                    self.read_hex(text);
                }
            } else {
                self.visit(value);
            }
        }

        fn visit(&mut self, value: &Value) {
            match value {
                Value::Array(items) if !items.iter().any(request_shaped) => {
                    for item in items {
                        self.visit(item);
                    }
                }
                Value::Object(fields) if !request_shaped(value) => {
                    let code = fields.get("code").and_then(Value::as_i64);
                    let message = fields.get("message").and_then(Value::as_str);
                    let explicit_revert = self.read_error(code, message);
                    if let Some(value) = fields.get("data") {
                        self.read_data(value, explicit_revert);
                    }
                    if let Some(value) = fields.get("originalError") {
                        self.visit(value);
                    }
                }
                _ => {}
            }
        }
    }

    fn request_shaped(value: &Value) -> bool {
        value.as_object().is_some_and(|fields| {
            ["to", "from", "input", "gas", "value", "method", "params"]
                .iter()
                .any(|key| fields.contains_key(*key))
        })
    }

    let mut evidence = Evidence::default();
    let explicit_revert = evidence.read_error(Some(resp.code), Some(&resp.message));
    if let Some(value) = resp
        .data
        .as_ref()
        .and_then(|data| serde_json::from_str(data.get()).ok())
    {
        evidence.read_data(&value, explicit_revert);
    }
    if evidence.revert {
        Some(ExecutionEvidence::Revert(evidence.data))
    } else {
        evidence.halt
    }
}

/// Parts of a node's answer to `eth_sendRawTransaction` that reject the
/// transaction outright, so that node does not hold it. The fee, pool and
/// funds errors of geth, which Nitro runs (`transaction underpriced`,
/// `replacement transaction underpriced`, `max fee per gas less than block
/// base fee`, `max priority fee per gas higher than max fee per gas`,
/// `tx fee (…) exceeds the configured cap (…)`), the older `fee cap less
/// than block base fee`, and the wordings of other clients for the same
/// answers.
const REJECTIONS: [&str; 11] = [
    "underpriced",
    "replacement fee too low",
    "gas price too low",
    "less than block base fee",
    "higher than max fee per gas",
    "exceeds the configured cap",
    "nonce too high",
    "insufficient funds",
    "intrinsic gas too low",
    "exceeds block gas limit",
    "invalid sender",
];

/// Whether a node's error answer to `eth_sendRawTransaction` is an
/// explicit rejection ([`REJECTIONS`]), so that node does not hold the
/// transaction. Every other answer is not: `already known`, `nonce too
/// low` (the nonce is used, possibly by this transaction) and a wording
/// not in the list can come from a node that holds it or mined it.
pub(crate) fn is_rejection(payload: &ErrorPayload) -> bool {
    let message = payload.message.to_ascii_lowercase();
    !message.contains("already known") && REJECTIONS.iter().any(|r| message.contains(r))
}

/// Whether a node's error `message` is its refusal to serve pruned state:
/// Nitro's "historical state … is not available", geth's "missing trie
/// node". The message is the only place a node says so.
pub(crate) fn state_pruned(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("historical state") || message.contains("missing trie node")
}

/// Whether a node's error `message` says it does not have the block a
/// call was pinned to: Nitro's "unsupported block number N" for a number
/// above its head and "requested block ahead of current block and the
/// hash is not currently canonical" for a hash it has not seen, geth's
/// "header not found" for a hash or number it has not seen and "header
/// for hash not found", and the "unknown block" / "block not found"
/// wordings of other clients and gateways. Another node, or the same one a moment later, can
/// have it.
pub(crate) fn lacks_block(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("unsupported block number")
        || message.contains("header not found")
        || message.contains("header for hash not found")
        || message.contains("hash is not currently canonical")
        || message.contains("unknown block")
        || message.contains("block not found")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::rpc::json_rpc::ErrorPayload;

    fn http(status: u16) -> TransportError {
        TransportErrorKind::http_error(status, String::new())
    }

    fn rpc_error(code: i64, message: &str) -> TransportError {
        RpcError::ErrorResp(ErrorPayload {
            code,
            message: message.to_string().into(),
            data: None,
        })
    }

    /// The case this taxonomy exists for: a provider that declines a wide
    /// `eth_getLogs` range with a client error is working correctly, and
    /// the scan's next, narrower request is the right response.
    #[test]
    fn a_declined_range_is_the_requests_fault() {
        assert_eq!(fault(&http(400)), Fault::Request);
        assert_eq!(fault(&http(413)), Fault::Request);
        assert_eq!(
            fault(&rpc_error(-32_602, "Log response size exceeded")),
            Fault::Request
        );
    }

    #[test]
    fn overload_auth_and_server_errors_are_the_endpoints() {
        // 408 is the one client error that is about the endpoint: it did
        // not get the request in time, and asking again can work.
        for status in [408, 429, 503, 500, 502, 504] {
            assert_eq!(fault(&http(status)), Fault::Endpoint, "status {status}");
        }
        // Credentials are a property of the endpoint, not the request.
        for status in [401, 403] {
            assert_eq!(fault(&http(status)), Fault::Endpoint, "status {status}");
        }
    }

    #[test]
    fn a_rate_limit_in_an_error_response_is_the_endpoints() {
        assert_eq!(
            fault(&rpc_error(
                429,
                "Your app has exceeded its compute units per second capacity"
            )),
            Fault::Endpoint
        );
    }

    /// Infura answers both a rate limit and a range rejection with
    /// `-32005`, and alloy calls the code retryable either way, so only
    /// the message separates them. Getting this wrong would count a
    /// narrowing rejection against the endpoint — the whole point of this
    /// taxonomy — for every Infura-shaped provider.
    #[test]
    fn the_message_decides_what_minus_32005_means() {
        assert_eq!(
            fault(&rpc_error(-32_005, "exceeded project rate limit")),
            Fault::Endpoint
        );
        assert_eq!(
            fault(&rpc_error(
                -32_005,
                "query returned more than 10000 results"
            )),
            Fault::Request
        );
    }

    /// A contract that reverts with the words "rate limit" declined the
    /// call; the endpoint did not limit it. Code 3, a revert wording and
    /// revert data each make the error the request's, before the rate-limit
    /// wording is read; a plain rate limit stays the endpoint's.
    #[test]
    fn an_execution_revert_is_never_a_rate_limit() {
        let with_data = |code: i64, message: &str, data: &str| {
            RpcError::ErrorResp(ErrorPayload {
                code,
                message: message.to_string().into(),
                data: Some(serde_json::value::RawValue::from_string(data.to_owned()).unwrap()),
            })
        };
        for revert in [
            rpc_error(3, "execution reverted: rate limit"),
            rpc_error(3, "rate limit exceeded"),
            rpc_error(-32_000, "execution reverted: rate limit exceeded"),
            rpc_error(429, "VM Exception while processing transaction: revert"),
            rpc_error(-32_015, "VM execution error: too many requests"),
            with_data(
                -32_000,
                "rate limit",
                r#"{"originalError":{"code":3,"data":"0x08c379a0"}}"#,
            ),
        ] {
            assert_eq!(fault(&revert), Fault::Request, "{revert}");
        }
        assert_eq!(fault(&rpc_error(-32_000, "rate limit")), Fault::Endpoint);
        assert_eq!(
            fault(&with_data(-32_000, "rate limit", r#""0x08c379a0""#)),
            Fault::Endpoint
        );
        assert_eq!(
            fault(&with_data(-32_000, "rate limit", r#"{"see":"https://x"}"#)),
            Fault::Endpoint
        );
    }

    /// A request that never serialized cannot be the endpoint's fault; a
    /// response that will not parse probably is.
    #[test]
    fn serialization_is_the_requests_fault_and_deserialization_the_endpoints() {
        let ser = RpcError::SerError(serde_json::from_str::<u8>("x").unwrap_err());
        assert_eq!(fault(&ser), Fault::Request);

        let deser = RpcError::deser_err(serde_json::from_str::<u8>("x").unwrap_err(), "<html>");
        assert_eq!(fault(&deser), Fault::Endpoint);
    }

    #[test]
    fn an_unanswered_request_is_the_endpoints() {
        assert_eq!(
            fault(&RpcError::Transport(TransportErrorKind::BackendGone)),
            Fault::Endpoint
        );
        assert_eq!(
            fault(&TransportError::local_usage_str("request timed out")),
            Fault::Endpoint
        );
        assert_eq!(fault(&RpcError::NullResp), Fault::Endpoint);
    }

    /// Only an explicit rejection says the node does not hold a sent
    /// transaction; a used nonce, a known one and an unknown wording do
    /// not.
    #[test]
    fn only_an_explicit_rejection_is_one() {
        for (answer, rejected) in [
            ("replacement transaction underpriced", true),
            (
                "transaction underpriced: tip needed 1, tip permitted 0",
                true,
            ),
            (
                "max fee per gas less than block base fee: address 0x1, maxFeePerGas: 1, baseFee: 2",
                true,
            ),
            ("fee cap less than block base fee", true),
            ("max priority fee per gas higher than max fee per gas", true),
            (
                "tx fee (1.20 ether) exceeds the configured cap (1.00 ether)",
                true,
            ),
            ("gas price too low", true),
            ("replacement fee too low", true),
            ("nonce too high", true),
            ("insufficient funds for gas * price + value", true),
            ("intrinsic gas too low", true),
            ("exceeds block gas limit", true),
            ("invalid sender", true),
            ("nonce too low", false),
            ("already known", false),
            ("Known transaction", false),
            ("upstream timeout", false),
            ("unknown error", false),
        ] {
            let payload = ErrorPayload {
                code: -32_000,
                message: answer.to_owned().into(),
                data: None,
            };
            assert_eq!(is_rejection(&payload), rejected, "{answer}");
        }
    }
}
