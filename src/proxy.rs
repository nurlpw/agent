use std::io;

use percent_encoding::percent_decode_str;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use url::Url;

use crate::{address::unbracket, net::connect_tcp};

const MAX_RESPONSE_HEAD: usize = 8 * 1024;
const HEAD_END: &[u8] = b"\r\n\r\n";

#[must_use]
pub fn from_env(server: &Url) -> Option<Url> {
    from_vars(server, |name| std::env::var(name).ok())
}

fn from_vars(server: &Url, var: impl Fn(&str) -> Option<String>) -> Option<Url> {
    let get = |name: &str| {
        var(name)
            .or_else(|| var(&name.to_ascii_lowercase()))
            .filter(|value| !value.trim().is_empty())
    };
    let host = unbracket(server.host_str()?);
    if get("NO_PROXY").is_some_and(|list| bypassed(host, &list)) {
        return None;
    }
    let raw = match server.scheme() {
        "https" | "wss" => get("HTTPS_PROXY"),
        _ => get("HTTP_PROXY"),
    }
    .or_else(|| get("ALL_PROXY"))?;

    let raw = raw.trim();
    let url = if raw.contains("://") {
        Url::parse(raw)
    } else {
        Url::parse(&format!("http://{raw}"))
    }
    .ok()?;
    (url.scheme() == "http" && url.host_str().is_some()).then_some(url)
}

fn bypassed(host: &str, list: &str) -> bool {
    let host = host.to_ascii_lowercase();
    list.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .any(|entry| {
            let entry = entry.trim_start_matches("*.").trim_start_matches('.');
            let entry = entry
                .split(':')
                .next()
                .unwrap_or(entry)
                .to_ascii_lowercase();
            entry == "*" || host == entry || host.ends_with(&format!(".{entry}"))
        })
}

#[must_use]
pub fn display(proxy: &Url) -> String {
    let (scheme, host) = (proxy.scheme(), proxy.host_str().unwrap_or_default());
    match proxy.port() {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("{0}")]
    Unreachable(io::Error),
    #[error("прокси ответил: {0}")]
    Refused(String),
    #[error("{0}")]
    Io(io::Error),
}

pub async fn connect(proxy: &Url, host: &str, port: u16) -> Result<TcpStream, ProxyError> {
    let proxy_host = unbracket(proxy.host_str().unwrap_or_default());
    let proxy_port = proxy.port_or_known_default().unwrap_or(80);
    let mut stream = connect_tcp(proxy_host, proxy_port)
        .await
        .map_err(ProxyError::Unreachable)?;

    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let authorization = authorization(proxy).unwrap_or_default();
    let request =
        format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n{authorization}\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(ProxyError::Io)?;

    let mut head = Vec::new();
    let mut buf = [0; 512];
    while !head
        .windows(HEAD_END.len())
        .any(|window| window == HEAD_END)
    {
        let read = stream.read(&mut buf).await.map_err(ProxyError::Io)?;
        if read == 0 {
            return Err(ProxyError::Refused("соединение закрыто".into()));
        }
        head.extend_from_slice(&buf[..read]);
        if head.len() > MAX_RESPONSE_HEAD {
            return Err(ProxyError::Refused("слишком длинный ответ".into()));
        }
    }

    let status_line = String::from_utf8_lossy(&head)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    let connected = status_line
        .split_whitespace()
        .nth(1)
        .is_some_and(|code| code.starts_with('2'));
    if connected {
        Ok(stream)
    } else {
        Err(ProxyError::Refused(status_line))
    }
}

fn authorization(proxy: &Url) -> Option<String> {
    if proxy.username().is_empty() {
        return None;
    }
    let decode = |part: &str| percent_decode_str(part).decode_utf8_lossy().into_owned();
    let user = decode(proxy.username());
    let password = decode(proxy.password().unwrap_or_default());
    let credentials = base64(format!("{user}:{password}").as_bytes());
    Some(format!("Proxy-Authorization: Basic {credentials}\r\n"))
}

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let byte = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
        let triple = (byte(0) << 16) | (byte(1) << 8) | byte(2);
        for i in 0..4 {
            if i <= chunk.len() {
                let index = (triple >> (18 - 6 * i)) & 0x3f;
                out.push(char::from(ALPHABET[index as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn resolve(server: &str, vars: &[(&str, &str)]) -> Option<String> {
        let vars: HashMap<&str, &str> = vars.iter().copied().collect();
        let server = Url::parse(server).unwrap();
        from_vars(&server, |name| vars.get(name).map(ToString::to_string)).map(String::from)
    }

    #[test]
    fn picks_proxy_by_scheme() {
        let vars = [
            ("HTTPS_PROXY", "http://127.0.0.1:10808"),
            ("HTTP_PROXY", "http://10.0.0.1:3128"),
        ];
        assert_eq!(
            resolve("https://nurl.pw", &vars).as_deref(),
            Some("http://127.0.0.1:10808/")
        );
        assert_eq!(
            resolve("http://nurl.pw", &vars).as_deref(),
            Some("http://10.0.0.1:3128/")
        );
        assert_eq!(
            resolve("https://nurl.pw", &[("https_proxy", "127.0.0.1:8888")]).as_deref(),
            Some("http://127.0.0.1:8888/")
        );
        assert_eq!(
            resolve("https://nurl.pw", &[("ALL_PROXY", "http://p:1")]).as_deref(),
            Some("http://p:1/")
        );
        assert_eq!(resolve("https://nurl.pw", &[]), None);
        assert_eq!(
            resolve(
                "https://nurl.pw",
                &[("ALL_PROXY", "socks5://127.0.0.1:1080")]
            ),
            None
        );
    }

    #[test]
    fn no_proxy_bypasses() {
        let with = |no_proxy| {
            [
                ("HTTPS_PROXY", "http://p:1"),
                ("HTTP_PROXY", "http://p:1"),
                ("NO_PROXY", no_proxy),
            ]
        };
        assert_eq!(
            resolve("http://localhost:8080", &with("localhost,127.0.0.1")),
            None
        );
        assert_eq!(resolve("https://nurl.pw", &with("nurl.pw")), None);
        assert_eq!(resolve("https://NURL.pw", &with("nurl.PW")), None);
        assert_eq!(resolve("https://api.nurl.pw", &with(".nurl.pw")), None);
        assert_eq!(resolve("https://api.nurl.pw", &with("nurl.pw")), None);
        assert_eq!(resolve("https://nurl.pw", &with("*")), None);
        assert!(resolve("https://notnurl.pw", &with("nurl.pw")).is_some());
    }

    #[test]
    fn display_hides_credentials() {
        let proxy = Url::parse("http://user:secret@proxy:3128").unwrap();
        assert_eq!(display(&proxy), "http://proxy:3128");
    }

    #[test]
    fn authorization_decodes_credentials() {
        let proxy = Url::parse("http://us%40er:p+ss@proxy:3128").unwrap();
        assert_eq!(
            authorization(&proxy).as_deref(),
            Some("Proxy-Authorization: Basic dXNAZXI6cCtzcw==\r\n")
        );
        assert_eq!(
            authorization(&Url::parse("http://proxy:3128").unwrap()),
            None
        );
    }

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"user:p@ss"), "dXNlcjpwQHNz");
    }
}
