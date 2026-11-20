//! A local JSON-RPC endpoint over HTTP whose answers a test scripts, so
//! the real [`HftTransport`](super::provider::HftTransport) can be driven through
//! answers that change from one attempt to the next.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// What the endpoint does with one request.
pub(crate) enum Answer {
    /// A JSON-RPC result.
    Result(Value),
    /// A JSON-RPC error with this code and message.
    Error(i64, String),
    /// An HTTP error status with an empty body.
    Status(u16),
    /// Nothing, until the caller times out.
    Silence,
    /// A JSON-RPC result under the call's id written as a string: the
    /// number `0` as `"0"`.
    ResultUnderTextId(Value),
    /// This JSON as the whole response body, in place of the request's
    /// own answers: one error object for a whole batch, an answer under
    /// another id.
    Body(Value),
}

/// The answer to a request, from its method and params.
pub(crate) type Script = dyn Fn(&str, &Value) -> Answer + Send + Sync;

/// Start an endpoint that answers every request by `script`, and return
/// its URL. One connection's requests are answered in order, and a batch's
/// calls each by `script`; a status, silence or body for any call answers
/// the whole batch.
pub(crate) async fn endpoint(
    script: impl Fn(&str, &Value) -> Answer + Send + Sync + 'static,
) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let script: Arc<Script> = Arc::new(script);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(serve(socket, Arc::clone(&script)));
        }
    });
    url
}

async fn serve(mut socket: TcpStream, script: Arc<Script>) {
    let mut buffer = Vec::new();
    loop {
        let Some(request) = read_request(&mut socket, &mut buffer).await else {
            return;
        };
        let (status, body) = match &request {
            Value::Array(calls) => match calls
                .iter()
                .map(|call| answer(script.as_ref(), call))
                .collect()
            {
                Ok(bodies) => (200, Value::Array(bodies).to_string()),
                Err(whole) => whole,
            },
            call => match answer(script.as_ref(), call) {
                Ok(body) => (200, body.to_string()),
                Err(whole) => whole,
            },
        };
        if status == 0 {
            tokio::time::sleep(Duration::from_secs(30)).await;
            return;
        }
        let response = format!(
            "HTTP/1.1 {status} Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        if socket.write_all(response.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// The JSON-RPC answer to one call, or the HTTP status (`0` for silence)
/// and body that answer its whole request.
fn answer(script: &Script, call: &Value) -> Result<Value, (u16, String)> {
    let id = call["id"].clone();
    let method = call["method"].as_str().unwrap_or_default();
    match script(method, &call["params"]) {
        Answer::Result(result) => Ok(json!({ "jsonrpc": "2.0", "id": id, "result": result })),
        Answer::Error(code, message) => {
            Ok(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
        }
        Answer::ResultUnderTextId(result) => {
            let id = match id {
                Value::Number(number) => Value::String(number.to_string()),
                id => id,
            };
            Ok(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        }
        Answer::Status(status) => Err((status, String::new())),
        Answer::Silence => Err((0, String::new())),
        Answer::Body(body) => Err((200, body.to_string())),
    }
}

/// The next request's JSON body, or `None` when the connection closes.
async fn read_request(socket: &mut TcpStream, buffer: &mut Vec<u8>) -> Option<Value> {
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
            let length: usize = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            let start = end + 4;
            if buffer.len() >= start + length {
                let body: Vec<u8> = buffer.drain(..start + length).skip(start).collect();
                return serde_json::from_slice(&body).ok();
            }
        }
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}
