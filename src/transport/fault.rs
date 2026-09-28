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
            if payload.is_retry_err() {
                Fault::Endpoint
            } else {
                Fault::Request
            }
        }
        RpcError::Transport(TransportErrorKind::HttpError(http)) => match http.status {
            429 | 503 => Fault::Endpoint,
            401 | 403 => Fault::Endpoint,
            500..=599 => Fault::Endpoint,
            _ => Fault::Request,
        },
        // A request this transport cannot serialize, or a response it
        // cannot parse into the caller's type, is about the request.
        RpcError::SerError(_) | RpcError::DeserError { .. } => Fault::Request,
        RpcError::UnsupportedFeature(_) => Fault::Request,
        // Missing batch responses, a gone backend, an unavailable
        // subscription, a local usage error, a null where a value was
        // required: the endpoint did not deliver.
        _ => Fault::Endpoint,
    }
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
        for status in [429, 503, 500, 502, 504] {
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
}
