//! Nothing the transport traces holds an endpoint's key.
//!
//! Its own binary, so no other test registers the transport's callsites
//! before the capturing subscriber is installed: `tracing` caches a
//! callsite's interest, and one first hit with no subscriber stays silent.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use alloy::eips::BlockId;
use alloy::primitives::{Address, B256, U256};
use alloy::providers::{Provider, RootProvider};
use alloy::rpc::client::RpcClient;
use alloy::rpc::types::{BlockOverrides, TransactionRequest};
use alloy::transports::BoxTransport;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};

use perpcity_sdk::transport::config::TransportConfig;
use perpcity_sdk::transport::provider::HftTransport;

const KEY: &str = "alch_secretkey123";

/// Every span and event field recorded at every level, as text, and the
/// fields of each warning on their own.
#[derive(Clone, Default)]
struct Captured {
    all: Arc<Mutex<String>>,
    warnings: Arc<Mutex<Vec<String>>>,
}

/// The fields of one span or event, as text.
struct Fields<'a>(&'a mut String);

impl Visit for Fields<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, "{}={value:?} ", field.name());
    }
}

impl Captured {
    fn all(&self) -> String {
        self.all.lock().unwrap().clone()
    }

    fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *self.warnings.lock().unwrap())
    }
}

impl Subscriber for Captured {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, span: &Attributes<'_>) -> Id {
        span.record(&mut Fields(&mut self.all.lock().unwrap()));
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, values: &Record<'_>) {
        values.record(&mut Fields(&mut self.all.lock().unwrap()));
    }
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut fields = String::new();
        event.record(&mut Fields(&mut fields));
        self.all.lock().unwrap().push_str(&fields);
        if *event.metadata().level() == Level::WARN {
            self.warnings.lock().unwrap().push(fields);
        }
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

/// A keyed local endpoint whose error page quotes the request path.
async fn not_found_endpoint() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body = format!("<p>The requested URL /v2/{KEY} was not found</p>");
    let response = format!(
        "HTTP/1.1 404 Not Found\r\ncontent-type: text/html\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let response = response.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                while socket.read(&mut buf).await.unwrap_or(0) > 0 {
                    if socket.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    format!("http://{addr}/v2/{KEY}")
}

#[tokio::test]
async fn a_failing_request_traces_no_key() {
    let captured = Captured::default();
    let _guard = tracing::subscriber::set_default(captured.clone());

    let url = not_found_endpoint().await;
    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(&url)
            .build()
            .unwrap(),
    )
    .unwrap();
    let provider: RootProvider =
        RootProvider::new(RpcClient::new(BoxTransport::new(transport), false));
    let err = provider.get_block_number().await.unwrap_err();

    let out = captured.all();
    assert!(out.contains("transport error"), "{out}");
    assert!(!out.contains(KEY), "{out}");
    assert!(!err.to_string().contains(KEY), "{err}");
}

/// A local endpoint that answers every request with `error`, the JSON-RPC
/// error object, under the id 0 of a new client's first request; its URL
/// carries the key in the userinfo, the path and the query.
async fn answering_endpoint(error: &'static str) -> (String, std::net::SocketAddr) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body = format!(r#"{{"jsonrpc":"2.0","id":0,"error":{error}}}"#);
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let response = response.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                while socket.read(&mut buf).await.unwrap_or(0) > 0 {
                    if socket.write_all(response.as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (
        format!("http://user:{KEY}@{addr}/v2/{KEY}?apikey={KEY}"),
        addr,
    )
}

fn provider(url: &str) -> RootProvider {
    let transport = HftTransport::new(
        TransportConfig::builder()
            .shared_endpoint(url)
            .build()
            .unwrap(),
    )
    .unwrap();
    RootProvider::new(RpcClient::new(BoxTransport::new(transport), false))
}

/// An `eth_call` at a block hash with the next block's overrides: four
/// params.
async fn call_with_overrides(url: &str) {
    let overrides = BlockOverrides {
        number: Some(U256::from(501)),
        time: Some(1_001),
        ..BlockOverrides::default()
    };
    provider(url)
        .call(TransactionRequest::default().to(Address::ZERO))
        .block(BlockId::hash(B256::repeat_byte(7)))
        .with_block_overrides(overrides)
        .await
        .unwrap_err();
}

/// A simulation with block overrides that the endpoint refuses (`-32602`)
/// or answers without the pinned block warns with the endpoint's host and
/// port, the method and the error code only: no userinfo, path or query.
/// The same refusal of a call without overrides, and an execution answer
/// (revert data under `-32602`, a revert under code 3 that names a
/// missing block), warn of nothing.
#[tokio::test]
async fn an_override_refusal_warns_with_the_host_only() {
    let captured = Captured::default();
    let _guard = tracing::subscriber::set_default(captured.clone());

    for (error, code) in [
        (
            r#"{"code":-32602,"message":"too many arguments, want at most 3 (POST /v2/alch_secretkey123)"}"#,
            -32602,
        ),
        (r#"{"code":-32000,"message":"header not found"}"#, -32000),
    ] {
        let (url, addr) = answering_endpoint(error).await;
        call_with_overrides(&url).await;
        assert_eq!(
            captured.take_warnings(),
            [format!(
                "message=RPC endpoint refused block overrides or could not serve the pinned \
                 block endpoint={addr} method=\"eth_call\" error_code={code} "
            )]
        );
    }

    let (url, _) =
        answering_endpoint(r#"{"code":-32602,"message":"too many arguments, want at most 3"}"#)
            .await;
    provider(&url)
        .call(TransactionRequest::default().to(Address::ZERO))
        .await
        .unwrap_err();
    assert_eq!(captured.take_warnings(), Vec::<String>::new());

    for error in [
        r#"{"code":-32602,"message":"invalid argument","data":"0x08c379a0"}"#,
        r#"{"code":3,"message":"execution reverted: unknown block","data":"0x08c379a0"}"#,
    ] {
        let (url, _) = answering_endpoint(error).await;
        call_with_overrides(&url).await;
        assert_eq!(captured.take_warnings(), Vec::<String>::new(), "{error}");
    }

    let out = captured.all();
    assert!(!out.contains(KEY), "{out}");
    assert!(!out.contains("user:"), "{out}");
}
