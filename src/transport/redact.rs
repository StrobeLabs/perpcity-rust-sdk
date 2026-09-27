//! Keeping endpoint credentials out of logs and errors.
//!
//! Hosted RPC providers carry the API key in the URL (Alchemy puts it in the
//! path, others in the query string or userinfo), so a logged endpoint URL is
//! a logged credential. Everything the transport logs uses [`redact_url`], and
//! every HTTP endpoint is wrapped so reqwest errors (which print
//! `for url (<full url>)`) lose the URL before they reach a caller.

use std::task::{Context, Poll};

use alloy::rpc::json_rpc::{RequestPacket, ResponsePacket};
use alloy::transports::http::Http;
use alloy::transports::{RpcError, TransportError, TransportErrorKind, TransportFut};

/// Label used when an endpoint string does not parse as a URL with a host.
const UNPARSEABLE: &str = "<unparseable endpoint>";

/// Reduce an endpoint URL to its host (and port), dropping the scheme,
/// userinfo, path, query and fragment, any of which may hold an API key.
///
/// The URL is parsed the same way the transport parses it (so `\` counts as a
/// path separator for http/ws URLs). Anything without a host becomes a fixed
/// placeholder rather than echoing the input.
///
/// ```
/// use perpcity_sdk::transport::redact_url;
/// assert_eq!(
///     redact_url("https://arb-mainnet.g.alchemy.com/v2/secret"),
///     "arb-mainnet.g.alchemy.com"
/// );
/// ```
pub fn redact_url(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return UNPARSEABLE.to_string();
    };
    match (parsed.host_str(), parsed.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        (None, _) => UNPARSEABLE.to_string(),
    }
}

/// Replace every occurrence of `url` in `msg` with its redacted form. For
/// errors from transports that embed the URL in their message and cannot be
/// stripped structurally (the WebSocket stack).
pub(crate) fn redact_in(msg: &str, url: &str) -> String {
    if url.is_empty() {
        return msg.to_string();
    }
    msg.replace(url, &redact_url(url))
}

/// Drop the request URL from a reqwest transport error, keeping its kind
/// (timeout, connect, ...) and source chain intact.
pub(crate) fn strip_url(err: TransportError) -> TransportError {
    match err {
        RpcError::Transport(TransportErrorKind::Custom(inner)) => {
            match inner.downcast::<reqwest::Error>() {
                Ok(e) => TransportErrorKind::custom(e.without_url()),
                Err(inner) => RpcError::Transport(TransportErrorKind::Custom(inner)),
            }
        }
        other => other,
    }
}

/// An alloy HTTP transport whose errors never carry the endpoint URL.
#[derive(Clone)]
pub(crate) struct UrlStrippingHttp(Http<reqwest::Client>);

impl std::fmt::Debug for UrlStrippingHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UrlStrippingHttp")
            .field("host", &redact_url(self.0.url()))
            .finish()
    }
}

impl UrlStrippingHttp {
    pub(crate) fn new(url: url::Url) -> Self {
        Self(Http::new(url))
    }
}

impl tower::Service<RequestPacket> for UrlStrippingHttp {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.0.poll_ready(cx).map_err(strip_url)
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        let fut = self.0.call(req);
        Box::pin(async move { fut.await.map_err(strip_url) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::Service;

    const KEY: &str = "alch_secretkey123";

    #[test]
    fn redact_url_keeps_only_the_host() {
        assert_eq!(
            redact_url(&format!("https://arb-mainnet.g.alchemy.com/v2/{KEY}")),
            "arb-mainnet.g.alchemy.com"
        );
        assert_eq!(
            redact_url("wss://u:pass@host.example:8546/ws?apikey=k#f"),
            "host.example:8546"
        );
        assert_eq!(redact_url("https://host.example?apikey=k"), "host.example");
        assert_eq!(redact_url("http://127.0.0.1:8545"), "127.0.0.1:8545");
    }

    #[test]
    fn redact_url_treats_backslash_as_a_path_separator() {
        let out = redact_url(&format!("https://host.example\\v2\\{KEY}"));
        assert_eq!(out, "host.example");
    }

    #[test]
    fn redact_url_never_echoes_unparseable_input() {
        for input in [
            format!("https://host.example:99999\\v2\\{KEY}"),
            format!("not a url/{KEY}"),
            format!("localhost:8545/{KEY}"),
            String::new(),
        ] {
            let out = redact_url(&input);
            assert!(!out.contains(KEY), "{input:?} -> {out}");
            assert_eq!(out, UNPARSEABLE, "{input:?}");
        }
    }

    #[test]
    fn redact_in_replaces_every_occurrence() {
        let url = format!("wss://host.example/v2/{KEY}");
        let msg = format!("connect to {url} failed; retry {url}");
        let out = redact_in(&msg, &url);
        assert!(!out.contains(KEY), "{out}");
        assert_eq!(out, "connect to host.example failed; retry host.example");
    }

    #[test]
    fn strip_url_leaves_non_reqwest_errors_alone() {
        let err = strip_url(TransportErrorKind::custom_str("boom"));
        assert_eq!(
            err.to_string(),
            TransportErrorKind::custom_str("boom").to_string()
        );
    }

    /// A refused connection is the common case that makes reqwest print the
    /// URL; the wrapped transport must not.
    #[tokio::test]
    async fn connection_errors_do_not_carry_the_url() {
        let url: url::Url = format!("http://127.0.0.1:1/v2/{KEY}").parse().unwrap();

        let raw = Http::new(url.clone())
            .call(RequestPacket::Batch(vec![]))
            .await
            .unwrap_err();
        assert!(
            raw.to_string().contains(KEY),
            "precondition: unwrapped reqwest error embeds the URL: {raw}"
        );

        let err = UrlStrippingHttp::new(url)
            .call(RequestPacket::Batch(vec![]))
            .await
            .unwrap_err();
        assert!(!err.to_string().contains(KEY), "{err}");
        assert!(!format!("{err:?}").contains(KEY), "{err:?}");
    }
}
