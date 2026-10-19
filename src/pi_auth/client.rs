//! An http client for the MCP transport that sends a server's Pi access token with every request,
//! refreshing it after a 401, which means the server did not handle the request.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::stream::BoxStream;
use rmcp::model::ClientJsonRpcMessage;
use rmcp::transport::streamable_http_client::{
    SseError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};
use sse_stream::Sse;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue, StatusCode};

use super::PiAuth;

type Error = StreamableHttpError<reqwest::Error>;

#[derive(Clone)]
pub struct PiAuthClient {
    http: reqwest::Client,
    auth: Arc<PiAuth>,
}

impl PiAuthClient {
    /// Built like rmcp's own client: no idle pooling, no redirects that would replay headers.
    pub fn new(auth: Arc<PiAuth>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .pool_max_idle_per_host(0)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("a client without TLS options builds"),
            auth,
        }
    }

    /// Sends with the current token, and once more with a new one if the server rejects it.
    async fn authorized<T, F>(&self, send: impl Fn(Option<String>) -> F) -> Result<T, Error>
    where
        F: Future<Output = Result<T, Error>>,
    {
        let token = self.auth.token().await;

        match send(token.clone()).await {
            Err(error) if unauthorized(&error) => {
                if self.auth.refresh(token.as_deref()).await {
                    send(self.auth.token().await).await
                } else {
                    Err(error)
                }
            }
            result => result,
        }
    }
}

fn unauthorized(error: &Error) -> bool {
    match error {
        StreamableHttpError::AuthRequired(_) => true,
        StreamableHttpError::Client(error) => error.status() == Some(StatusCode::UNAUTHORIZED),
        _ => false,
    }
}

impl StreamableHttpClient for PiAuthClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, Error> {
        self.authorized(|token| {
            self.http.post_message(
                uri.clone(),
                message.clone(),
                session_id.clone(),
                token,
                custom_headers.clone(),
            )
        })
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, Error> {
        self.authorized(|token| {
            self.http.post_message_with_max_sse_event_size(
                uri.clone(),
                message.clone(),
                session_id.clone(),
                token,
                custom_headers.clone(),
                max_sse_event_size,
            )
        })
        .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), Error> {
        self.authorized(|token| {
            self.http.delete_session(
                uri.clone(),
                session_id.clone(),
                token,
                custom_headers.clone(),
            )
        })
        .await
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, Error> {
        self.authorized(|token| {
            self.http.get_stream(
                uri.clone(),
                session_id.clone(),
                last_event_id.clone(),
                token,
                custom_headers.clone(),
            )
        })
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        _auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, Error> {
        self.authorized(|token| {
            self.http.get_stream_with_max_sse_event_size(
                uri.clone(),
                session_id.clone(),
                last_event_id.clone(),
                token,
                custom_headers.clone(),
                max_sse_event_size,
            )
        })
        .await
    }
}
