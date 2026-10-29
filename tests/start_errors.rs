use nurl_agent::{StartError, open_with_proxy, parse_target, probe_local};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use url::Url;

fn target() -> Url {
    parse_target("3000").unwrap()
}

async fn open(server: &Url, target: Url) -> Result<nurl_agent::Tunnel, StartError> {
    open_with_proxy(server, target, None).await
}

async fn dead_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

async fn fake_server(status_line: &'static str, body: &'static str) -> Url {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 {status_line}\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    Url::parse(&format!("http://127.0.0.1:{port}")).unwrap()
}

#[tokio::test]
async fn unknown_host_is_not_found() {
    let server = Url::parse("https://nurl-test.invalid").unwrap();
    let error = open(&server, target()).await.err().unwrap();
    assert!(
        matches!(&error, StartError::ServerNotFound { host } if host == "nurl-test.invalid"),
        "{error:?}"
    );
    assert!(error.hint().unwrap().contains("интернет"));
}

#[tokio::test]
async fn closed_port_is_refused() {
    let server = Url::parse(&format!("http://127.0.0.1:{}", dead_port().await)).unwrap();
    let error = open(&server, target()).await.err().unwrap();
    assert!(
        matches!(error, StartError::ServerRefused { .. }),
        "{error:?}"
    );
}

#[tokio::test]
async fn some_other_website_is_not_nurl() {
    let server = fake_server("404 Not Found", "<h1>Not Found</h1>").await;
    let error = open(&server, target()).await.err().unwrap();
    assert!(matches!(error, StartError::NotNurl { .. }), "{error:?}");

    let server = fake_server("200 OK", "<html>welcome</html>").await;
    let error = open(&server, target()).await.err().unwrap();
    assert!(matches!(error, StartError::NotNurl { .. }), "{error:?}");
}

#[tokio::test]
async fn server_error_message_is_passed_through() {
    let server = fake_server("400 Bad Request", r#"{"error":"target: Validation error"}"#).await;
    let error = open(&server, target()).await.err().unwrap();
    assert!(
        matches!(&error, StartError::Rejected { status: 400, code: None, message } if message == "target: Validation error"),
        "{error:?}"
    );
}

#[tokio::test]
async fn coded_refusal_is_explained() {
    let body =
        r#"{"error":"the Open plan allows 4 open tunnels per IP address","code":"tunnel_limit"}"#;
    let server = fake_server("429 Too Many Requests", body).await;
    let error = open(&server, target()).await.err().unwrap();
    assert_eq!(error.code(), Some("tunnel_limit"));
    assert_eq!(
        error.to_string(),
        "с этого IP уже открыто максимальное число туннелей"
    );
    assert!(error.hint().unwrap().contains("На Pro лимит выше"));
}

#[tokio::test]
async fn probe_reports_missing_local_app() {
    let dead = parse_target(&dead_port().await.to_string()).unwrap();
    assert_eq!(
        probe_local(&dead).await,
        Err("на этом порту ничего не запущено".into())
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let alive = parse_target(&format!(
        "127.0.0.1:{}",
        listener.local_addr().unwrap().port()
    ))
    .unwrap();
    assert_eq!(probe_local(&alive).await, Ok(()));
}

#[tokio::test]
async fn proxy_not_running() {
    let server = Url::parse("https://nurl.pw").unwrap();
    let proxy = Url::parse(&format!("http://127.0.0.1:{}", dead_port().await)).unwrap();
    let error = open_with_proxy(&server, target(), Some(proxy))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&error, StartError::ProxyDown { host, .. } if host == "nurl.pw"),
        "{error:?}"
    );
    assert!(error.hint().unwrap().contains("NO_PROXY"));
}

#[tokio::test]
async fn proxy_cannot_reach_server() {
    let proxy = fake_server("502 Bad Gateway", "upstream unreachable").await;

    let https = Url::parse("https://nurl.pw").unwrap();
    let error = open_with_proxy(&https, target(), Some(proxy.clone()))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(error, StartError::ProxyFailed { .. }),
        "https via CONNECT: {error:?}"
    );

    let http = Url::parse("http://nurl.pw").unwrap();
    let error = open_with_proxy(&http, target(), Some(proxy))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(error, StartError::ProxyFailed { .. }),
        "plain http via proxy: {error:?}"
    );
}
