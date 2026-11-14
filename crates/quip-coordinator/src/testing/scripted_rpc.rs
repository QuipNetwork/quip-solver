//! A scripted JSON-RPC server over HTTP, for tests that run the real binary
//! against a chain they control. It answers each request from a handler and
//! records the method names, so a test can show that a command only read.

use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A JSON-RPC endpoint on a loopback port, stopped when dropped.
pub struct ScriptedRpc {
    url: String,
    methods: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl ScriptedRpc {
    /// Listen on a free loopback port. `handler` maps a method name and its
    /// params to the JSON `result` of the response.
    ///
    /// # Panics
    /// Panics when no loopback port can be bound.
    pub async fn start(handler: impl Fn(&str, &Value) -> Value + Send + Sync + 'static) -> Self {
        #[expect(
            clippy::expect_used,
            reason = "test double; a port that cannot bind is a test failure"
        )]
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the scripted RPC listener");
        #[expect(
            clippy::expect_used,
            reason = "test double; a bound listener has an address"
        )]
        let addr = listener.local_addr().expect("scripted RPC address");
        let methods = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(handler);
        let task = tokio::spawn({
            let methods = Arc::clone(&methods);
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let (handler, methods) = (Arc::clone(&handler), Arc::clone(&methods));
                    drop(tokio::spawn(async move {
                        serve_one(stream, handler.as_ref(), &methods).await;
                    }));
                }
            }
        });
        Self {
            url: format!("http://{addr}"),
            methods,
            task,
        }
    }

    /// The endpoint to give the command under test.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Every method the server has answered, in order.
    ///
    /// # Panics
    /// Panics if a request handler poisoned the record.
    #[must_use]
    pub fn methods(&self) -> Vec<String> {
        #[expect(
            clippy::unwrap_used,
            reason = "test double; Mutex poison is a test failure"
        )]
        self.methods.lock().unwrap().clone()
    }
}

impl Drop for ScriptedRpc {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Answer one HTTP request, then close the connection.
async fn serve_one(
    mut stream: TcpStream,
    handler: &(dyn Fn(&str, &Value) -> Value + Send + Sync),
    methods: &Mutex<Vec<String>>,
) {
    let Some(body) = read_body(&mut stream).await else {
        return;
    };
    let Ok(request) = serde_json::from_slice::<Value>(&body) else {
        return;
    };
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    if let Ok(mut seen) = methods.lock() {
        seen.push(method.to_owned());
    }
    let reply = json!({
        "jsonrpc": "2.0",
        "id": request.get("id").cloned().unwrap_or(Value::Null),
        "result": handler(method, &params),
    })
    .to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
        reply.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// The body of one HTTP request: the headers up to the blank line, then
/// `content-length` bytes.
async fn read_body(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(chunk.get(..n)?);
    };
    let head = String::from_utf8_lossy(buf.get(..header_end)?).to_ascii_lowercase();
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))?
        .trim()
        .parse()
        .ok()?;
    while buf.len() < header_end + length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(chunk.get(..n)?);
    }
    buf.get(header_end..header_end + length).map(<[u8]>::to_vec)
}
