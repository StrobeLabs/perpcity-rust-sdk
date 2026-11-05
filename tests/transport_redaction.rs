//! Nothing the transport traces holds an endpoint's key.
//!
//! Its own binary, so no other test registers the transport's callsites
//! before the capturing subscriber is installed: `tracing` caches a
//! callsite's interest, and one first hit with no subscriber stays silent.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use alloy::providers::{Provider, RootProvider};
use alloy::rpc::client::RpcClient;
use alloy::transports::BoxTransport;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

use perpcity_sdk::transport::config::TransportConfig;
use perpcity_sdk::transport::provider::HftTransport;

const KEY: &str = "alch_secretkey123";

/// Every span and event field recorded at every level, as text.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<String>>);

impl Visit for Captured {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let mut out = self.0.lock().unwrap();
        let _ = write!(out, "{}={value:?} ", field.name());
    }
}

impl Subscriber for Captured {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, span: &Attributes<'_>) -> Id {
        span.record(&mut self.clone());
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, values: &Record<'_>) {
        values.record(&mut self.clone());
    }
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        event.record(&mut self.clone());
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

    let out = captured.0.lock().unwrap().clone();
    assert!(out.contains("transport error"), "{out}");
    assert!(!out.contains(KEY), "{out}");
    assert!(!err.to_string().contains(KEY), "{err}");
}
