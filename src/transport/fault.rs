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
/// Alloy calls every `-32005` retryable, since Infura returns it for
/// `exceeded project rate limit` — but Infura also returns it for
/// `query returned more than 10000 results`, a range rejection that the
/// log scan narrows and that no endpoint would answer differently. So for
/// that code the message decides.
pub(crate) fn is_rate_limit(payload: &ErrorPayload) -> bool {
    if payload.code != -32_005 {
        return payload.is_retry_err();
    }
    ErrorPayload::<()> {
        code: 0,
        message: payload.message.clone(),
        data: None,
    }
    .is_retry_err()
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
