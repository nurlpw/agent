use std::{io, time::Duration};

use futures_util::future::select_ok;
use tokio::net::{TcpStream, lookup_host};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{
        self,
        error::UrlError,
        handshake::client::{Request, Response},
        protocol::WebSocketConfig,
    },
};
use url::Url;

use crate::{address::unbracket, proxy};

pub(crate) type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, thiserror::Error)]
pub(crate) enum ConnectError {
    #[error(transparent)]
    Io(io::Error),
    #[error(transparent)]
    Proxy(proxy::ProxyError),
    #[error(transparent)]
    Ws(tungstenite::Error),
}

pub(crate) async fn connect_ws(
    request: Request,
    config: Option<WebSocketConfig>,
    proxy: Option<&Url>,
) -> Result<(Socket, Response), ConnectError> {
    let uri = request.uri();
    let host = uri
        .host()
        .map(|host| unbracket(host).to_owned())
        .ok_or(ConnectError::Ws(tungstenite::Error::Url(
            UrlError::NoHostName,
        )))?;
    let default_port = if uri.scheme_str() == Some("wss") {
        443
    } else {
        80
    };
    let port = uri.port_u16().unwrap_or(default_port);

    let connect = async {
        match proxy {
            Some(proxy) => proxy::connect(proxy, &host, port)
                .await
                .map_err(ConnectError::Proxy),
            None => connect_tcp(&host, port).await.map_err(ConnectError::Io),
        }
    };
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, connect)
        .await
        .map_err(|_| ConnectError::Io(io::ErrorKind::TimedOut.into()))??;
    tokio_tungstenite::client_async_tls_with_config(request, stream, config, None)
        .await
        .map_err(ConnectError::Ws)
}

pub(crate) async fn connect_tcp(host: &str, port: u16) -> io::Result<TcpStream> {
    let addrs: Vec<_> = lookup_host((host, port)).await?.collect();
    if addrs.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{host} has no addresses"),
        ));
    }
    let attempts = addrs
        .into_iter()
        .map(|addr| Box::pin(TcpStream::connect(addr)));
    select_ok(attempts).await.map(|(stream, _)| stream)
}

pub async fn probe_local(target: &Url) -> Result<(), String> {
    let host = unbracket(target.host_str().unwrap_or_default());
    let port = target.port_or_known_default().unwrap_or(80);
    match tokio::time::timeout(PROBE_TIMEOUT, connect_tcp(host, port)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) if e.kind() == io::ErrorKind::ConnectionRefused => {
            Err("на этом порту ничего не запущено".into())
        }
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!(
            "не ответило за {} секунды",
            PROBE_TIMEOUT.as_secs()
        )),
    }
}
