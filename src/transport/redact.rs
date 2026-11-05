//! Keeping endpoint credentials out of logs and errors.
//!
//! Hosted RPC providers carry the API key in the URL (Alchemy puts it in the
//! path, others in the query string or userinfo), so a logged endpoint URL is
//! a logged credential. Everything the transport logs uses [`redact_url`] or
//! [`redact_text`], and every HTTP endpoint is a [`UrlStrippingHttp`], whose
//! errors and request spans never hold the URL.

use std::task::{Context, Poll};

use alloy::rpc::json_rpc::{RequestPacket, ResponsePacket};
use alloy::transports::{RpcError, TransportError, TransportErrorKind, TransportFut};
use reqwest::header::{CONTENT_TYPE, HeaderValue};

/// Label used when an endpoint string does not parse as a URL with a host.
const UNPARSEABLE: &str = "<unparseable endpoint>";

/// What stands in for the part of a URL that was cut.
const CUT: &str = "<redacted>";

/// The schemes [`redact_text`] recognises as URLs, longest first.
const SCHEMES: [&str; 4] = ["https", "http", "wss", "ws"];

/// The shortest bare URL part [`redact_in`] cuts. An API key is longer, and
/// cutting a shorter part would garble the message around it.
const MIN_BARE_PART: usize = 8;

/// Characters that end a URL found in free text.
const URL_END: &str = "\"'<>()[]{},`";

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

/// `text` with every `http`, `https`, `ws` or `wss` URL in it cut to its
/// scheme and host: userinfo, path, query and fragment become `/<redacted>`.
/// A URL with none of them is kept as written; one that does not parse with a
/// host becomes a fixed placeholder.
///
/// For error text that may quote an endpoint, such as an error chain logged
/// by the caller.
///
/// ```
/// use perpcity_sdk::transport::redact_text;
/// assert_eq!(
///     redact_text("error sending request for url (https://arb-mainnet.g.alchemy.com/v2/secret)"),
///     "error sending request for url (https://arb-mainnet.g.alchemy.com/<redacted>)"
/// );
/// ```
pub fn redact_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(sep) = rest.find("://") {
        let before = rest[..sep].to_ascii_lowercase();
        let after = &rest[sep + 3..];
        let end = sep + 3 + url_len(after);
        match SCHEMES.iter().find(|scheme| before.ends_with(**scheme)) {
            Some(scheme) => {
                let start = sep - scheme.len();
                out.push_str(&rest[..start]);
                out.push_str(&cut_url(&rest[start..end]));
            }
            None => out.push_str(&rest[..end]),
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// The length of the URL that `after` (the text after `://`) starts with: up
/// to whitespace or a [`URL_END`] character, except inside the brackets of an
/// IPv6 host.
fn url_len(after: &str) -> usize {
    let mut in_host = true;
    let mut in_brackets = false;
    for (i, c) in after.char_indices() {
        match c {
            '[' if in_host => in_brackets = true,
            ']' if in_brackets => in_brackets = false,
            '/' | '\\' | '?' | '#' if !in_brackets => in_host = false,
            _ if in_brackets => {}
            _ if c.is_whitespace() || URL_END.contains(c) => return i,
            _ => {}
        }
    }
    after.len()
}

/// `url` with its userinfo, path, query and fragment replaced by
/// `/<redacted>`, or as written when it has none of them.
fn cut_url(url: &str) -> String {
    let Ok(parsed) = url::Url::parse(url) else {
        return UNPARSEABLE.to_string();
    };
    let Some(host) = parsed.host_str() else {
        return UNPARSEABLE.to_string();
    };
    if secret_parts(&parsed).is_empty() {
        return url.to_string();
    }
    match parsed.port() {
        Some(port) => format!("{}://{host}:{port}/{CUT}", parsed.scheme()),
        None => format!("{}://{host}/{CUT}", parsed.scheme()),
    }
}

/// The parts of `url` that may hold a credential: username, password, a path
/// other than `/`, query and fragment.
fn secret_parts(url: &url::Url) -> Vec<&str> {
    let path = Some(url.path()).filter(|p| !p.is_empty() && *p != "/");
    [
        Some(url.username()).filter(|u| !u.is_empty()),
        url.password(),
        path,
        url.query(),
        url.fragment(),
    ]
    .into_iter()
    .flatten()
    .filter(|p| !p.is_empty())
    .collect()
}

/// `msg` with endpoint `url` cut wherever it appears: as written, as any URL
/// [`redact_text`] finds, and as its bare path, query or userinfo of
/// [`MIN_BARE_PART`] characters or more, which an error page can quote
/// without the scheme and host.
pub(crate) fn redact_in(msg: &str, url: &str) -> String {
    let msg = if url.is_empty() {
        msg.to_string()
    } else {
        msg.replace(url, &cut_url(url))
    };
    let mut msg = redact_text(&msg);
    if let Ok(parsed) = url::Url::parse(url) {
        let mut parts = secret_parts(&parsed);
        parts.retain(|p| p.len() >= MIN_BARE_PART);
        parts.sort_by_key(|p| std::cmp::Reverse(p.len()));
        for part in parts {
            msg = msg.replace(part, CUT);
        }
    }
    msg
}

/// `err` with every trace of endpoint `url` cut from its text. An error whose
/// text holds none keeps its type; one that does becomes a custom error with
/// the redacted text, since the original cannot be edited in place.
pub(crate) fn redact_error(err: TransportError, url: &str) -> TransportError {
    match err {
        RpcError::Transport(TransportErrorKind::HttpError(http)) => {
            TransportErrorKind::http_error(http.status, redact_in(&http.body, url))
        }
        RpcError::DeserError { err, text } => RpcError::DeserError {
            err,
            text: redact_in(&text, url),
        },
        other => {
            let text = other.to_string();
            let redacted = redact_in(&text, url);
            if redacted == text {
                other
            } else {
                TransportErrorKind::custom_str(&redacted)
            }
        }
    }
}

/// An alloy-compatible HTTP transport whose errors and request spans never
/// carry the endpoint URL. It speaks to the node as alloy's `Http` does,
/// without the span alloy opens with the full URL on every request.
#[derive(Clone)]
pub(crate) struct UrlStrippingHttp {
    client: reqwest::Client,
    url: url::Url,
}

impl std::fmt::Debug for UrlStrippingHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UrlStrippingHttp")
            .field("host", &redact_url(self.url.as_str()))
            .finish()
    }
}

impl UrlStrippingHttp {
    pub(crate) fn new(url: url::Url) -> Self {
        Self {
            client: reqwest::Client::default(),
            url,
        }
    }

    async fn send(self, req: RequestPacket) -> Result<ResponsePacket, TransportError> {
        let payload = serde_json::to_vec(&req).map_err(RpcError::ser_err)?;
        let resp = self
            .client
            .post(self.url.clone())
            .headers(req.headers())
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .body(payload)
            .send()
            .await
            .map_err(|e| TransportErrorKind::custom(e.without_url()))?;
        let status = resp.status();
        let body = resp
            .bytes()
            .await
            .map_err(|e| TransportErrorKind::custom(e.without_url()))?;
        let text = || redact_in(&String::from_utf8_lossy(&body), self.url.as_str());
        if !status.is_success() {
            return Err(TransportErrorKind::http_error(status.as_u16(), text()));
        }
        serde_json::from_slice(&body).map_err(|err| TransportError::deser_err(err, text()))
    }
}

impl tower::Service<RequestPacket> for UrlStrippingHttp {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        Box::pin(self.clone().send(req))
    }
}

#[cfg(test)]
mod tests {
    use alloy::rpc::json_rpc::{Id, Request, SerializedRequest};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tower::Service;

    use super::*;

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
    fn redact_text_cuts_every_credential_bearing_part() {
        for (input, want) in [
            (
                format!("for url (https://arb-mainnet.g.alchemy.com/v2/{KEY})"),
                "for url (https://arb-mainnet.g.alchemy.com/<redacted>)",
            ),
            (
                format!("dial wss://u:{KEY}@host.example:8546/ws failed"),
                "dial wss://host.example:8546/<redacted> failed",
            ),
            (
                format!("GET https://host.example/?apikey={KEY}#frag returned 401"),
                "GET https://host.example/<redacted> returned 401",
            ),
            (
                format!("HTTPS://HOST.example/v2/{KEY}"),
                "https://host.example/<redacted>",
            ),
            (
                format!("dial http://[::1]:8545/v2/{KEY} failed"),
                "dial http://[::1]:8545/<redacted> failed",
            ),
            (
                format!("dial ws://u:{KEY}@[2001:db8::1]/ws] failed"),
                "dial ws://[2001:db8::1]/<redacted>] failed",
            ),
            (
                format!("url=xhttps://host.example/v2/{KEY}"),
                "url=xhttps://host.example/<redacted>",
            ),
        ] {
            assert_eq!(redact_text(&input), want, "{input:?}");
        }
    }

    #[test]
    fn redact_text_keeps_urls_with_nothing_to_cut() {
        let text = "a https://host.example and http://127.0.0.1:8545/ b";
        assert_eq!(redact_text(text), text);
    }

    #[test]
    fn redact_text_leaves_other_schemes_and_plain_text_alone() {
        let text = format!("ftp://host.example/{KEY} and postgres://u:p@db/x");
        assert_eq!(redact_text(&text), text);
        assert_eq!(redact_text("no urls here"), "no urls here");
    }

    #[test]
    fn redact_text_never_echoes_unparseable_urls() {
        let out = redact_text(&format!("to https://host.example:99999/{KEY} failed"));
        assert_eq!(out, format!("to {UNPARSEABLE} failed"));
    }

    #[test]
    fn redact_text_is_idempotent() {
        let once = redact_text(&format!("x https://host.example/v2/{KEY} y"));
        assert_eq!(redact_text(&once), once);
    }

    #[test]
    fn redact_in_replaces_every_occurrence() {
        let url = format!("wss://host.example/v2/{KEY}");
        let msg = format!("connect to {url} failed; retry {url}");
        let out = redact_in(&msg, &url);
        assert_eq!(
            out,
            "connect to wss://host.example/<redacted> failed; retry wss://host.example/<redacted>"
        );
    }

    #[test]
    fn redact_in_cuts_the_path_quoted_without_its_host() {
        let url = format!("https://host.example/v2/{KEY}?tier=pro");
        let msg = format!("<p>The requested URL /v2/{KEY} was not found (tier=pro)</p>");
        let out = redact_in(&msg, &url);
        assert!(!out.contains(KEY), "{out}");
        assert_eq!(
            out,
            "<p>The requested URL <redacted> was not found (<redacted>)</p>"
        );
    }

    #[test]
    fn redact_in_leaves_short_bare_parts_alone() {
        let url = format!("https://u:{KEY}@host.example/rpc");
        let msg = format!("unauthorized user u at /rpc, key {KEY}");
        assert_eq!(
            redact_in(&msg, &url),
            "unauthorized user u at /rpc, key <redacted>"
        );
    }

    #[test]
    fn redact_error_keeps_the_type_of_an_error_with_nothing_to_cut() {
        let url = format!("https://host.example/v2/{KEY}");
        let err = redact_error(TransportErrorKind::backend_gone(), &url);
        assert!(matches!(
            err,
            RpcError::Transport(TransportErrorKind::BackendGone)
        ));
    }

    #[test]
    fn redact_error_cuts_the_url_from_every_kind() {
        let url = format!("wss://host.example/v2/{KEY}");
        let quoted = format!("unable to connect to {url}");
        for err in [
            TransportErrorKind::custom_str(&quoted),
            TransportErrorKind::http_error(404, quoted.clone()),
            TransportError::deser_err(serde_json::from_str::<u8>("x").unwrap_err(), quoted.clone()),
        ] {
            let out = redact_error(err, &url);
            assert!(!out.to_string().contains(KEY), "{out}");
            assert!(!format!("{out:?}").contains(KEY), "{out:?}");
        }
    }

    fn request() -> RequestPacket {
        let req: SerializedRequest = Request::new("eth_blockNumber", Id::Number(1), ())
            .try_into()
            .unwrap();
        req.into()
    }

    /// A local endpoint that answers every request with `status` and `body`.
    async fn answering(status: &str, body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: text/html\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let response = response.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    if socket.read(&mut buf).await.unwrap_or(0) > 0 {
                        let _ = socket.write_all(response.as_bytes()).await;
                    }
                });
            }
        });
        format!("http://{addr}/v2/{KEY}")
    }

    #[tokio::test]
    async fn answers_reach_the_caller() {
        let url = answering("200 OK", r#"{"jsonrpc":"2.0","id":1,"result":"0x10"}"#).await;
        let resp = UrlStrippingHttp::new(url.parse().unwrap())
            .call(request())
            .await
            .unwrap();
        let single = resp.as_single().expect("a single response");
        assert_eq!(single.payload.as_success().unwrap().get(), r#""0x10""#);
    }

    /// A refused connection is the common case that makes reqwest print the
    /// URL; the transport must not.
    #[tokio::test]
    async fn connection_errors_do_not_carry_the_url() {
        let url: url::Url = format!("http://127.0.0.1:1/v2/{KEY}").parse().unwrap();
        let err = UrlStrippingHttp::new(url)
            .call(request())
            .await
            .unwrap_err();
        assert!(!err.to_string().contains(KEY), "{err}");
        assert!(!format!("{err:?}").contains(KEY), "{err:?}");
        assert!(
            matches!(&err, RpcError::Transport(TransportErrorKind::Custom(e)) if e.is::<reqwest::Error>()),
            "the reqwest error stays typed: {err:?}"
        );
    }

    /// An error page that quotes the request path keeps its status, without
    /// the key.
    #[tokio::test]
    async fn error_bodies_do_not_carry_the_url() {
        let url = answering(
            "404 Not Found",
            "<p>The requested URL /v2/alch_secretkey123 was not found</p>",
        )
        .await;
        let err = UrlStrippingHttp::new(url.parse().unwrap())
            .call(request())
            .await
            .unwrap_err();
        assert!(!err.to_string().contains(KEY), "{err}");
        assert!(
            matches!(&err, RpcError::Transport(TransportErrorKind::HttpError(h)) if h.status == 404),
            "{err:?}"
        );
    }

    #[test]
    fn debug_shows_only_the_host() {
        let url: url::Url = format!("https://host.example/v2/{KEY}").parse().unwrap();
        let out = format!("{:?}", UrlStrippingHttp::new(url));
        assert!(!out.contains(KEY), "{out}");
        assert!(out.contains("host.example"), "{out}");
    }
}
