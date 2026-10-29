use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Instant, SystemTime},
};

use anyhow::{Context, anyhow};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{
    self, Bytes, Message,
    client::IntoClientRequest,
    http::{HeaderName, HeaderValue},
    protocol::WebSocketConfig,
};
use url::Url;

use crate::{
    error::{RunError, ShutdownReason},
    net::{ConnectError, Socket, connect_ws},
    protocol::{self, Frame},
    tunnel::{Event, EventHandler, RequestLog},
};

const WS_BUFFER: usize = 256;

type Headers = Vec<(String, String)>;

enum WsData {
    Text(String),
    Binary(Bytes),
}

struct LocalResponse {
    status: u16,
    headers: Headers,
    body: Vec<u8>,
}

struct WsRejection {
    status: u16,
    message: String,
    detail: Option<String>,
}

type Sessions = HashMap<u64, mpsc::Sender<WsData>>;

pub(crate) struct Forwarder {
    http: reqwest::Client,
    target: Url,
    public_origin: String,
    target_origin: String,
    max_body_bytes: usize,
    outbound: mpsc::Sender<Message>,
    ws_sessions: Mutex<Sessions>,
    on_event: EventHandler,
}

impl Forwarder {
    pub(crate) fn new(
        http: reqwest::Client,
        target: Url,
        public_url: &str,
        max_body_bytes: usize,
        outbound: mpsc::Sender<Message>,
        on_event: EventHandler,
    ) -> Self {
        Self {
            http,
            public_origin: origin_of(public_url),
            target_origin: target.origin().ascii_serialization(),
            target,
            max_body_bytes,
            outbound,
            ws_sessions: Mutex::default(),
            on_event,
        }
    }

    pub(crate) fn close_sessions(&self) {
        self.sessions().clear();
    }

    fn sessions(&self) -> MutexGuard<'_, Sessions> {
        self.ws_sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn dispatch(self: &Arc<Self>, data: &[u8]) -> Option<RunError> {
        let Some((frame, body)) = protocol::decode(data) else {
            self.emit(Event::Warning("malformed frame from server".into()));
            return None;
        };
        match frame {
            Frame::Request {
                id,
                method,
                uri,
                headers,
            } => {
                let this = Arc::clone(self);
                let body = body.to_vec();
                tokio::spawn(async move { this.handle_http(id, method, uri, headers, body).await });
            }
            Frame::WsOpen { id, uri, headers } => {
                let this = Arc::clone(self);
                tokio::spawn(async move { this.handle_ws(id, uri, headers).await });
            }
            Frame::WsMessage { id, binary } => self.deliver(id, binary, body),
            Frame::WsClose { id } => {
                self.sessions().remove(&id);
            }
            Frame::Shutdown { reason, message } => {
                return Some(RunError::Shutdown {
                    reason: ShutdownReason::from_frame(&reason),
                    message,
                });
            }
            Frame::Response { .. }
            | Frame::Error { .. }
            | Frame::WsOpened { .. }
            | Frame::WsRejected { .. } => {
                self.emit(Event::Warning("server sent an agent-only frame".into()));
            }
        }
        None
    }

    fn deliver(&self, id: u64, binary: bool, body: &[u8]) {
        let message = if binary {
            WsData::Binary(Bytes::copy_from_slice(body))
        } else if let Ok(text) = std::str::from_utf8(body) {
            WsData::Text(text.to_owned())
        } else {
            self.sessions().remove(&id);
            return;
        };
        let mut sessions = self.sessions();
        if let Some(tx) = sessions.get(&id)
            && tx.try_send(message).is_err()
        {
            sessions.remove(&id);
        }
    }

    fn emit(&self, event: Event) {
        (self.on_event)(event);
    }

    fn log(&self, method: &str, uri: &str, status: u16, started: Started, error: Option<String>) {
        self.emit(Event::Request(RequestLog {
            at: started.at,
            method: method.to_owned(),
            uri: uri.to_owned(),
            status,
            duration: started.instant.elapsed(),
            error,
        }));
    }

    async fn send(&self, frame: &Frame, body: &[u8]) {
        let encoded = protocol::encode(frame, body);
        let _ = self.outbound.send(Message::Binary(encoded.into())).await;
    }

    async fn handle_http(
        &self,
        id: u64,
        method: String,
        uri: String,
        headers: Headers,
        body: Vec<u8>,
    ) {
        let started = Started::now();
        match self.forward_http(&method, &uri, headers, body).await {
            Ok(response) => {
                let upstream_ms = u64::try_from(started.instant.elapsed().as_millis()).ok();
                self.log(&method, &uri, response.status, started, None);
                let frame = Frame::Response {
                    id,
                    status: response.status,
                    headers: response.headers,
                    upstream_ms,
                };
                self.send(&frame, &response.body).await;
            }
            Err(e) => {
                self.log(&method, &uri, 502, started, Some(format!("{e:#}")));
                let frame = Frame::Error {
                    id,
                    message: e.to_string(),
                };
                self.send(&frame, &[]).await;
            }
        }
    }

    async fn forward_http(
        &self,
        method: &str,
        uri: &str,
        headers: Headers,
        body: Vec<u8>,
    ) -> anyhow::Result<LocalResponse> {
        let url = local_url(&self.target, uri);
        let method = reqwest::Method::from_bytes(method.as_bytes())?;

        let request = self.local_headers(headers).into_iter().fold(
            self.http.request(method, url).body(body),
            |request, (name, value)| request.header(name, value),
        );
        let response = request
            .send()
            .await
            .with_context(|| format!("local app at {} is unreachable", self.target_origin))?;

        let too_large = || {
            anyhow!(
                "response is larger than the tunnel limit of {} bytes",
                self.max_body_bytes
            )
        };
        if response
            .content_length()
            .is_some_and(|len| usize::try_from(len).map_or(true, |len| len > self.max_body_bytes))
        {
            return Err(too_large());
        }

        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
            })
            .collect();

        let mut body = Vec::new();
        let mut chunks = response.bytes_stream();
        while let Some(chunk) = chunks.next().await {
            body.extend_from_slice(&chunk?);
            if body.len() > self.max_body_bytes {
                return Err(too_large());
            }
        }
        Ok(LocalResponse {
            status,
            headers,
            body,
        })
    }

    async fn handle_ws(&self, id: u64, uri: String, headers: Headers) {
        let started = Started::now();
        let (socket, protocol) = match self.connect_local_ws(&uri, headers).await {
            Ok(connected) => connected,
            Err(rejection) => {
                self.log("WS", &uri, rejection.status, started, rejection.detail);
                let frame = Frame::WsRejected {
                    id,
                    status: rejection.status,
                    message: rejection.message,
                };
                self.send(&frame, &[]).await;
                return;
            }
        };

        let (tx, mut from_server) = mpsc::channel(WS_BUFFER);
        self.sessions().insert(id, tx);
        self.send(&Frame::WsOpened { id, protocol }, &[]).await;
        self.log("WS", &uri, 101, started, None);

        let (mut sink, mut stream) = socket.split();
        loop {
            tokio::select! {
                message = stream.next() => {
                    let (binary, data) = match message {
                        Some(Ok(Message::Text(text))) => (false, Bytes::from(text)),
                        Some(Ok(Message::Binary(bytes))) => (true, bytes),
                        Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => continue,
                        Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                    };
                    self.send(&Frame::WsMessage { id, binary }, &data).await;
                }
                data = from_server.recv() => {
                    let message = match data {
                        Some(WsData::Text(text)) => Message::Text(text.into()),
                        Some(WsData::Binary(bytes)) => Message::Binary(bytes),
                        None => break,
                    };
                    if sink.send(message).await.is_err() {
                        break;
                    }
                }
            }
        }

        let _ = sink.send(Message::Close(None)).await;
        self.sessions().remove(&id);
        self.send(&Frame::WsClose { id }, &[]).await;
    }

    async fn connect_local_ws(
        &self,
        uri: &str,
        headers: Headers,
    ) -> Result<(Socket, Option<String>), WsRejection> {
        let unreachable = |e: &dyn fmt::Display| {
            let message = format!("local app at {} is unreachable", self.target_origin);
            WsRejection {
                status: 502,
                detail: Some(format!("{message}: {e}")),
                message,
            }
        };

        let mut url = local_url(&self.target, uri);
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .expect("ws and wss are valid schemes");

        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|e| unreachable(&e))?;
        for (name, value) in self.local_headers(headers) {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(&value),
            ) {
                request.headers_mut().append(name, value);
            }
        }

        let config = WebSocketConfig::default()
            .max_message_size(Some(self.max_body_bytes))
            .max_frame_size(Some(self.max_body_bytes));
        match connect_ws(request, Some(config), None).await {
            Ok((socket, response)) => {
                let protocol = response
                    .headers()
                    .get("sec-websocket-protocol")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                Ok((socket, protocol))
            }
            Err(ConnectError::Ws(tungstenite::Error::Http(response))) => {
                let status = response.status().as_u16();
                Err(WsRejection {
                    status,
                    message: format!("local app refused the WebSocket with status {status}"),
                    detail: None,
                })
            }
            Err(e) => Err(unreachable(&e)),
        }
    }

    fn local_headers(&self, headers: Headers) -> Headers {
        headers
            .into_iter()
            .filter(|(name, _)| name != "host")
            .map(|(name, value)| {
                if name == "origin" && value == self.public_origin {
                    (name, self.target_origin.clone())
                } else {
                    (name, value)
                }
            })
            .collect()
    }
}

#[derive(Clone, Copy)]
struct Started {
    at: SystemTime,
    instant: Instant,
}

impl Started {
    fn now() -> Self {
        Self {
            at: SystemTime::now(),
            instant: Instant::now(),
        }
    }
}

fn local_url(target: &Url, uri: &str) -> Url {
    let (path, query) = match uri.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (uri, None),
    };
    let mut url = target.clone();
    url.set_path(path);
    url.set_query(query);
    url
}

fn origin_of(url: &str) -> String {
    Url::parse(url).map_or_else(|_| url.to_owned(), |url| url.origin().ascii_serialization())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_target;

    #[test]
    fn local_url_keeps_path_and_query() {
        let target = parse_target("3000").unwrap();
        let url = |uri| local_url(&target, uri).to_string();
        assert_eq!(url("/api/x?a=1&b=2"), "http://localhost:3000/api/x?a=1&b=2");
        assert_eq!(url("/"), "http://localhost:3000/");
        assert_eq!(url("/?"), "http://localhost:3000/?");
        assert_eq!(url("/a%20b?q=%2F"), "http://localhost:3000/a%20b?q=%2F");
    }

    #[test]
    fn local_url_never_leaves_target() {
        let target = parse_target("127.0.0.1:3000").unwrap();
        for uri in [
            "/http://evil.example/x",
            "/https://evil.example/x",
            r"/\/evil.example/x",
            r"/\\evil.example/x",
            "//evil.example/x",
            "///evil.example/x",
            "/ftp:x",
            "/@evil.example/x",
            "/..//evil.example/x",
        ] {
            let url = local_url(&target, uri);
            assert_eq!(
                url.origin(),
                target.origin(),
                "{uri} must stay on the local app, got {url}"
            );
        }
    }

    #[test]
    fn origin_has_no_trailing_slash() {
        assert_eq!(
            origin_of("http://1gh2t3n8.localhost:8080"),
            "http://1gh2t3n8.localhost:8080"
        );
        assert_eq!(
            origin_of("https://1gh2t3n8.nurl.pw/"),
            "https://1gh2t3n8.nurl.pw"
        );
    }
}
