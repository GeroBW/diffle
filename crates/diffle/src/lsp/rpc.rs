//! JSON-RPC 2.0 with `Content-Length` framing (the LSP base protocol), driven by async-lsp's main
//! loop. Payloads stay JSON values; no LSP semantics.
use std::collections::HashMap;
use std::fmt;
use std::ops::ControlFlow;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_lsp::router::Router;
use async_lsp::{AnyNotification, AnyRequest, ErrorCode, LspService, MainLoop, ResponseError, ServerSocket};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tower_service::Service;

/// How long a request waits for its answer unless the caller says otherwise.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// What async-lsp answers a request whose main loop ended before the reply came.
const STOPPED: &str = "forwarding service stopped";

/// A server's error reply, carrying the code so callers can tell a refusal from an outage.
#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Why a request got no result: the server refused it, the connection is gone, or time ran out.
#[derive(Debug, Clone)]
pub enum RequestError {
    Rpc(RpcError),
    Closed(String),
    Timeout(String),
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rpc(e) => f.write_str(&e.message),
            Self::Closed(reason) | Self::Timeout(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for RequestError {}

type Answer = Result<Value, RequestError>;
type MessageHandler = Arc<dyn Fn(&str, &Value) + Send + Sync>;
type RequestHandler = Arc<dyn Fn(Value) -> BoxFuture<'static, Result<Value, RpcError>> + Send + Sync>;

struct Shared {
    handlers: Mutex<Vec<MessageHandler>>,
    requests: Mutex<HashMap<String, RequestHandler>>,
    /// Why the connection ended, once it has.
    closed: watch::Sender<Option<String>>,
}

impl Shared {
    fn observe(&self, method: &str, params: &Value) {
        let handlers: Vec<MessageHandler> = self.handlers.lock().unwrap().clone();
        for handler in handlers {
            handler(method, params);
        }
    }

    fn close(&self, reason: String) {
        self.closed.send_if_modified(|closed| closed.is_none() && closed.replace(reason).is_none());
    }

    fn reason(&self) -> Option<String> {
        self.closed.borrow().clone()
    }
}

/// Ends the main loop from our side.
struct Stop;

/// One end of a JSON-RPC connection. Cheap to clone; every clone is the same connection.
#[derive(Clone)]
pub struct Connection {
    server: ServerSocket,
    shared: Arc<Shared>,
}

impl Connection {
    pub fn new(
        reader: impl AsyncRead + Unpin + Send + 'static,
        writer: impl AsyncWrite + Unpin + Send + 'static,
    ) -> Self {
        let shared = Arc::new(Shared {
            handlers: Mutex::new(Vec::new()),
            requests: Mutex::new(HashMap::new()),
            closed: watch::channel(None).0,
        });
        let (main, server) = MainLoop::new_client(|_| {
            let mut router = Router::new(shared.clone());
            router
                .unhandled_notification(|shared, notif| {
                    shared.observe(&notif.method, &notif.params);
                    ControlFlow::Continue(())
                })
                .unhandled_request(|shared, req| {
                    shared.observe(&req.method, &req.params);
                    let handler = shared.requests.lock().unwrap().get(&req.method).cloned();
                    async move {
                        let Some(handler) = handler else { return Ok(Value::Null) };
                        let code = |e: RpcError| ResponseError::new(ErrorCode(e.code as i32), e.message);
                        handler(req.params).await.map_err(code)
                    }
                })
                .event::<Stop>(|_, _| ControlFlow::Break(Ok(())));
            router
        });
        let ended = shared.clone();
        tokio::spawn(async move {
            let reason = match main.run_buffered(reader.compat(), writer.compat_write()).await {
                Ok(()) => "connection closed".to_owned(),
                Err(async_lsp::Error::Eof) => "the language server closed its output".to_owned(),
                Err(e) => format!("language server connection failed: {e}"),
            };
            ended.close(reason);
        });
        Self { server, shared }
    }

    /// Sends a request and waits for its result. `None` waits 30 seconds; a zero timeout waits forever.
    pub async fn request(&self, method: &str, params: Value, timeout: Option<Duration>) -> Answer {
        if let Some(reason) = self.shared.reason() {
            return Err(RequestError::Closed(reason));
        }
        // async-lsp numbers the request itself; its message types are built through serde alone.
        let req: AnyRequest =
            serde_json::from_value(json!({ "id": 0, "method": method, "params": params })).expect("requests build");
        let call = self.server.clone().call(req);
        let answer = async {
            match call.await {
                Ok(result) => Ok(result),
                Err(e) if e.message == STOPPED => {
                    let mut closed = self.shared.closed.subscribe();
                    let reason = closed.wait_for(Option::is_some).await.ok().and_then(|r| r.clone());
                    Err(RequestError::Closed(reason.unwrap_or_else(|| "connection closed".into())))
                }
                Err(e) => Err(RequestError::Rpc(RpcError::new(e.code.0.into(), e.message))),
            }
        };
        let timeout = timeout.unwrap_or(DEFAULT_TIMEOUT);
        if timeout.is_zero() {
            return answer.await;
        }
        tokio::time::timeout(timeout, answer).await.unwrap_or_else(|_| {
            Err(RequestError::Timeout(format!("{method} timed out after {} ms", timeout.as_millis())))
        })
    }

    pub fn notify(&self, method: &str, params: Value) {
        if self.shared.reason().is_some() {
            return;
        }
        let notif: AnyNotification =
            serde_json::from_value(json!({ "method": method, "params": params })).expect("notifications build");
        let _ = LspService::notify(&mut self.server.clone(), notif);
    }

    /// Observes every message the server sends: notifications and requests alike.
    pub fn on_message(&self, f: impl Fn(&str, &Value) + Send + Sync + 'static) {
        self.shared.handlers.lock().unwrap().push(Arc::new(f));
    }

    /// Handles a server request and answers with its result, or its error. Unhandled ones get `null`.
    pub fn on_request(
        &self,
        method: &str,
        f: impl Fn(Value) -> BoxFuture<'static, Result<Value, RpcError>> + Send + Sync + 'static,
    ) {
        self.shared.requests.lock().unwrap().insert(method.to_owned(), Arc::new(f));
    }

    /// Fails every pending request with `reason` and ignores further traffic.
    pub fn dispose(&self, reason: String) {
        self.shared.close(reason);
        let _ = self.server.emit(Stop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn answers_asynchronous_server_requests_and_returns_handler_errors_without_losing_the_connection() {
        let (a, b) = tokio::io::duplex(4096);
        let (a_read, a_write) = tokio::io::split(a);
        let (b_read, b_write) = tokio::io::split(b);
        let client = Connection::new(a_read, a_write);
        let server = Connection::new(b_read, b_write);
        client.on_request("workspace/configuration", |_| {
            async {
                tokio::task::yield_now().await;
                Ok(json!([{ "hover": true }, null]))
            }
            .boxed()
        });
        client.on_request("fail", |_| async { Err(RpcError::new(-32602, "bad parameters")) }.boxed());
        let config = server.request("workspace/configuration", json!({}), None).await.unwrap();
        assert_eq!(config, json!([{ "hover": true }, null]));
        match server.request("fail", json!({}), None).await {
            Err(RequestError::Rpc(e)) => assert_eq!((e.code, e.message.as_str()), (-32602, "bad parameters")),
            other => panic!("{other:?}"),
        }
        assert_eq!(server.request("window/workDoneProgress/create", json!({}), None).await.unwrap(), Value::Null);
        client.dispose("done".into());
        assert!(matches!(client.request("x", json!({}), None).await, Err(RequestError::Closed(r)) if r == "done"));
        server.dispose("done".into());
    }

    #[tokio::test]
    async fn times_out_unanswered_requests_and_closes_on_a_frame_without_length() {
        let (a, b) = tokio::io::duplex(4096);
        let (a_read, a_write) = tokio::io::split(a);
        let (mut b_read, mut b_write) = tokio::io::split(b);
        let client = Connection::new(a_read, a_write);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        client.on_message(move |method, _| record.lock().unwrap().push(method.to_owned()));
        let body = br#"{"jsonrpc":"2.0","method":"hello","params":{}}"#;
        b_write.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes()).await.unwrap();
        b_write.write_all(body).await.unwrap();
        let err = client.request("slow", json!(null), Some(Duration::from_millis(50))).await.unwrap_err();
        assert_eq!(err.to_string(), "slow timed out after 50 ms");
        assert_eq!(*seen.lock().unwrap(), ["hello"]);
        let mut sent = vec![0; 16];
        assert!(b_read.read(&mut sent).await.unwrap() > 0);
        let pending = client.request("pending", json!(null), Some(Duration::ZERO));
        b_write.write_all(b"X-Junk: 1\r\n\r\n").await.unwrap();
        match pending.await {
            Err(RequestError::Closed(reason)) => {
                assert_eq!(reason, "language server connection failed: protocol error: Missing content-length")
            }
            other => panic!("{other:?}"),
        }
    }
}
