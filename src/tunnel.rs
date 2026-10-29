use std::{
    future::Future,
    pin::{Pin, pin},
    sync::Arc,
    time::{Duration, SystemTime},
};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest, http::HeaderValue};
use url::Url;

use crate::{
    api::{self, Account, Api},
    device::DeviceInfo,
    error::{self, RunError, ShutdownReason, StartError},
    forwarder::Forwarder,
    net::{ConnectError, Socket, connect_ws},
    proxy,
};

const OUTBOUND_BUFFER: usize = 64;
const CLOSE_GRACE: Duration = Duration::from_secs(2);
const RECONNECT_FIRST: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
const SERVER_SILENCE: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Deserialize)]
pub struct Plan {
    pub name: String,
    pub warning_page: bool,
}

#[derive(Debug, Clone)]
pub enum Event {
    Request(RequestLog),
    Warning(String),
    Reconnecting {
        attempt: u32,
        delay: Duration,
        reason: String,
    },
    Reconnected,
    AddressChanged(String),
}

#[derive(Debug, Clone)]
pub struct RequestLog {
    pub at: SystemTime,
    pub method: String,
    pub uri: String,
    pub status: u16,
    pub duration: Duration,
    pub error: Option<String>,
}

pub(crate) type EventHandler = Arc<dyn Fn(Event) + Send + Sync>;

#[derive(Debug, Clone)]
pub struct OpenOptions {
    pub target: Option<Url>,
    pub name: Option<String>,
    pub token: Option<String>,
    pub device: DeviceInfo,
}

impl OpenOptions {
    #[must_use]
    pub fn anonymous(target: Url) -> Self {
        Self {
            target: Some(target),
            name: None,
            token: None,
            device: DeviceInfo::current(),
        }
    }
}

pub struct Tunnel {
    pub public_url: String,
    pub target: Url,
    pub plan: Option<Plan>,
    pub account: Option<Account>,
    pub latest_version: Option<String>,
    socket: Option<Socket>,
    api: Api,
    options: OpenOptions,
    reconnect_token: String,
    ws_url: String,
    http: reqwest::Client,
    max_body_bytes: usize,
    silence_limit: Duration,
    on_event: EventHandler,
}

pub async fn open(server: &Url, target: Url) -> Result<Tunnel, StartError> {
    open_with_proxy(server, target, proxy::from_env(server)).await
}

pub async fn open_with_proxy(
    server: &Url,
    target: Url,
    proxy: Option<Url>,
) -> Result<Tunnel, StartError> {
    open_with(Api::new(server, proxy), OpenOptions::anonymous(target)).await
}

pub async fn open_with(api: Api, options: OpenOptions) -> Result<Tunnel, StartError> {
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("HTTP client always builds");

    let open = request_open(&api, &options).await?;
    let target = match &options.target {
        Some(target) => target.clone(),
        None => open
            .target
            .as_deref()
            .and_then(|target| Url::parse(target).ok())
            .ok_or_else(|| {
                StartError::not_nurl(api.server(), "сервер не прислал локальный адрес туннеля")
            })?,
    };
    let socket = connect_tunnel(&api, &open.ws_url, &open.token)
        .await
        .map_err(ConnectFailure::into_start)?;

    Ok(Tunnel {
        public_url: open.public_url,
        target,
        plan: open.plan,
        account: open.account,
        latest_version: open.latest_agent_version,
        socket: Some(socket),
        api,
        options,
        reconnect_token: open.token,
        ws_url: open.ws_url,
        http,
        max_body_bytes: open.max_body_bytes,
        silence_limit: SERVER_SILENCE,
        on_event: Arc::new(|_| {}),
    })
}

async fn request_open(api: &Api, options: &OpenOptions) -> Result<api::OpenResponse, StartError> {
    let body = api::OpenRequest {
        target: options.target.as_ref().map(Url::as_str),
        name: options.name.as_deref(),
        device: &options.device,
    };
    api.open(&body, options.token.as_deref()).await
}

enum ConnectFailure {
    Refused(StartError),
    Network(StartError),
}

impl ConnectFailure {
    fn into_start(self) -> StartError {
        match self {
            Self::Refused(e) | Self::Network(e) => e,
        }
    }
}

async fn connect_tunnel(api: &Api, ws_url: &str, token: &str) -> Result<Socket, ConnectFailure> {
    let server = api.server();
    let proxy = api.proxy();
    let refused = |detail: String| ConnectFailure::Refused(StartError::not_nurl(server, detail));

    let mut request = ws_url
        .into_client_request()
        .map_err(|e| refused(format!("неверный адрес сокета {ws_url}: {e}")))?;
    let authorization = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| refused("неверный токен туннеля".into()))?;
    request.headers_mut().insert("authorization", authorization);
    let parsed = Url::parse(ws_url).map_err(|e| refused(e.to_string()))?;

    match connect_ws(request, None, proxy).await {
        Ok((socket, _)) => Ok(socket),
        Err(ConnectError::Ws(tungstenite::Error::Http(response))) => {
            let body = response.body().as_deref().unwrap_or_default();
            let status = response.status().as_u16();
            Err(ConnectFailure::Refused(StartError::from_refusal(
                server, proxy, status, body,
            )))
        }
        Err(ConnectError::Ws(e)) => Err(ConnectFailure::Network(error::classify_connect(
            &parsed, None, &e,
        ))),
        Err(ConnectError::Io(e)) => Err(ConnectFailure::Network(error::classify_connect(
            &parsed, None, &e,
        ))),
        Err(ConnectError::Proxy(e)) => {
            let proxy = proxy.expect("a proxy error implies a proxy");
            Err(ConnectFailure::Network(StartError::from_proxy(
                &parsed, proxy, e,
            )))
        }
    }
}

enum SocketEnd {
    Stopped,
    Closed(RunError),
    Lost(String),
}

type Shutdown<'a> = Pin<&'a mut (dyn Future<Output = ()> + Send)>;

impl Tunnel {
    #[must_use]
    pub fn on_event(mut self, handler: impl Fn(Event) + Send + Sync + 'static) -> Self {
        self.on_event = Arc::new(handler);
        self
    }

    pub async fn run(mut self, shutdown: impl Future<Output = ()> + Send) -> Result<(), RunError> {
        let mut shutdown = pin!(shutdown);
        let mut socket = self.socket.take().expect("socket is set until run");
        loop {
            match self.serve(socket, shutdown.as_mut()).await {
                SocketEnd::Stopped => return Ok(()),
                SocketEnd::Closed(error) => return Err(error),
                SocketEnd::Lost(reason) => match self.reconnect(reason, shutdown.as_mut()).await? {
                    Some(next) => socket = next,
                    None => return Ok(()),
                },
            }
        }
    }

    fn emit(&self, event: Event) {
        (self.on_event)(event);
    }

    async fn reconnect(
        &mut self,
        mut reason: String,
        mut shutdown: Shutdown<'_>,
    ) -> Result<Option<Socket>, RunError> {
        let mut delay = RECONNECT_FIRST;
        let mut attempt = 0;
        loop {
            attempt += 1;
            self.emit(Event::Reconnecting {
                attempt,
                delay,
                reason: reason.clone(),
            });
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = shutdown.as_mut() => return Ok(None),
            }
            delay = (delay * 2).min(RECONNECT_MAX);

            let socket = match connect_tunnel(&self.api, &self.ws_url, &self.reconnect_token).await
            {
                Ok(socket) => Some(socket),
                Err(ConnectFailure::Network(e)) => {
                    reason = e.to_string();
                    None
                }
                Err(ConnectFailure::Refused(e)) => match e.code() {
                    Some("tunnel_expired") | None => match self.reopen().await {
                        Ok(Some(socket)) => Some(socket),
                        Ok(None) => {
                            reason = "сервер не принял переподключение".into();
                            None
                        }
                        Err(e @ StartError::Rejected { .. }) => return Err(RunError::Reopen(e)),
                        Err(e) => {
                            reason = e.to_string();
                            None
                        }
                    },
                    Some(code) => return Err(RunError::shutdown(ShutdownReason::from_code(code))),
                },
            };
            if let Some(socket) = socket {
                self.emit(Event::Reconnected);
                return Ok(Some(socket));
            }
        }
    }

    async fn reopen(&mut self) -> Result<Option<Socket>, StartError> {
        let open = request_open(&self.api, &self.options).await?;
        if open.public_url != self.public_url {
            self.public_url.clone_from(&open.public_url);
            self.emit(Event::AddressChanged(open.public_url));
        }
        self.reconnect_token = open.token;
        self.ws_url = open.ws_url;
        self.max_body_bytes = open.max_body_bytes;
        match connect_tunnel(&self.api, &self.ws_url, &self.reconnect_token).await {
            Ok(socket) => Ok(Some(socket)),
            Err(ConnectFailure::Refused(e)) => Err(e),
            Err(ConnectFailure::Network(_)) => Ok(None),
        }
    }

    async fn serve(&self, socket: Socket, mut shutdown: Shutdown<'_>) -> SocketEnd {
        let (mut sink, mut stream) = socket.split();

        let (tx, mut rx) = mpsc::channel::<Message>(OUTBOUND_BUFFER);
        let writer = tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                let closing = matches!(message, Message::Close(_));
                if sink.send(message).await.is_err() || closing {
                    break;
                }
            }
        });

        let forwarder = Arc::new(Forwarder::new(
            self.http.clone(),
            self.target.clone(),
            &self.public_url,
            self.max_body_bytes,
            tx.clone(),
            Arc::clone(&self.on_event),
        ));
        let mut guard = ServeGuard {
            writer: Some(writer),
            forwarder: Arc::clone(&forwarder),
        };

        let end = loop {
            tokio::select! {
                () = shutdown.as_mut() => {
                    let _ = tx.send(Message::Close(None)).await;
                    break SocketEnd::Stopped;
                }
                message = tokio::time::timeout(self.silence_limit, stream.next()) => match message {
                    Err(_) => {
                        break SocketEnd::Lost(format!(
                            "сервер молчит {} с",
                            self.silence_limit.as_secs()
                        ));
                    }
                    Ok(Some(Ok(Message::Binary(data)))) => {
                        if let Some(error) = forwarder.dispatch(&data) {
                            break SocketEnd::Closed(error);
                        }
                    }
                    Ok(Some(Ok(Message::Close(_))) | None) => {
                        break SocketEnd::Lost("сервер закрыл соединение".into());
                    }
                    Ok(Some(Err(e))) => break SocketEnd::Lost(error::chain_text(&e)),
                    Ok(Some(Ok(_))) => {}
                },
            }
        };

        forwarder.close_sessions();
        drop(tx);
        drop(forwarder);
        if let Some(writer) = guard.writer.take()
            && matches!(end, SocketEnd::Stopped)
        {
            let _ = tokio::time::timeout(CLOSE_GRACE, writer).await;
        }
        end
    }
}

struct ServeGuard {
    writer: Option<JoinHandle<()>>,
    forwarder: Arc<Forwarder>,
}

impl Drop for ServeGuard {
    fn drop(&mut self) {
        if let Some(writer) = &self.writer {
            writer.abort();
        }
        self.forwarder.close_sessions();
    }
}

#[cfg(test)]
mod tests {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::*;
    use crate::parse_target;

    async fn silent_server() -> (Url, mpsc::UnboundedReceiver<()>) {
        let ws = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_url = format!("ws://{}/ws", ws.local_addr().unwrap());
        let (accepted, sockets) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = ws.accept().await {
                held.push(tokio_tungstenite::accept_async(stream).await.unwrap());
                let _ = accepted.send(());
            }
        });

        let api = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = Url::parse(&format!("http://{}", api.local_addr().unwrap())).unwrap();
        let body = serde_json::json!({
            "public_url": "http://abc.localhost",
            "ws_url": ws_url,
            "token": "t",
            "max_body_bytes": 1024,
        })
        .to_string();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = api.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        (server, sockets)
    }

    #[tokio::test]
    async fn silent_server_triggers_reconnect() {
        let (server, mut sockets) = silent_server().await;
        let mut tunnel = open_with(
            Api::new(&server, None),
            OpenOptions::anonymous(parse_target("3000").unwrap()),
        )
        .await
        .unwrap();
        tunnel.silence_limit = Duration::from_millis(200);
        sockets.recv().await.unwrap();

        let (events, mut reasons) = mpsc::unbounded_channel();
        let tunnel = tunnel.on_event(move |event| {
            if let Event::Reconnecting { reason, .. } = event {
                let _ = events.send(reason);
            }
        });
        let run = tokio::spawn(tunnel.run(std::future::pending()));

        let reason = tokio::time::timeout(Duration::from_secs(5), reasons.recv())
            .await
            .expect("agent must notice the silent server")
            .unwrap();
        assert!(reason.contains("молчит"), "{reason}");
        tokio::time::timeout(Duration::from_secs(5), sockets.recv())
            .await
            .expect("agent must reconnect after the silence")
            .unwrap();
        run.abort();
    }
}
