//! The transport seam between the chain client and a JSON-RPC endpoint.
//!
//! The client builds requests and decodes responses. It never opens a socket
//! itself. That split lets the same client run on a native WebSocket, on an
//! HTTP endpoint, or on a browser WebSocket under WebAssembly, where the
//! native jsonrpsee client does not build.

use super::ChainError;
use async_trait::async_trait;

/// A boxed stream, so the trait stays object safe.
pub type BoxStream<'a, T> = std::pin::Pin<Box<dyn futures::Stream<Item = T> + Send + 'a>>;

/// Issues JSON-RPC requests and subscriptions against one endpoint.
#[async_trait]
pub trait RpcTransport: Send + Sync {
    /// Issue one JSON-RPC method call against `url`.
    ///
    /// # Errors
    /// Returns [`ChainError::Unavailable`] when the endpoint cannot be reached
    /// or the node reports an error.
    async fn request(
        &self,
        url: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ChainError>;

    /// Open a JSON-RPC subscription against `url`.
    ///
    /// The stream yields one decoded notification per item. It ends when the
    /// server closes the subscription.
    ///
    /// # Errors
    /// Returns [`ChainError::Unavailable`] when the subscription cannot start.
    async fn subscribe(
        &self,
        url: &str,
        sub: &str,
        params: serde_json::Value,
        unsub: &str,
    ) -> Result<BoxStream<'static, Result<serde_json::Value, ChainError>>, ChainError>;
}
