use url::{Host, Url};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AddressError {
    #[error("адрес не указан")]
    Empty,
    #[error("порт «{0}» должен быть числом от 1 до 65535")]
    BadPort(String),
    #[error("протокол «{0}» не поддерживается, нужен http или https")]
    BadScheme(String),
    #[error("в адресе нет хоста")]
    NoHost,
    #[error("укажите только адрес и порт, без пути и параметров: «{0}»")]
    HasPath(String),
    #[error("адрес не должен содержать логин и пароль")]
    HasCredentials,
    #[error("не получается разобрать адрес «{0}»")]
    Invalid(String),
}

pub fn parse_target(raw: &str) -> Result<Url, AddressError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(AddressError::Empty);
    }
    let port = raw.strip_prefix(':').unwrap_or(raw).trim_end_matches('/');
    if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
        check_port(port)?;
        return Ok(Url::parse(&format!("http://localhost:{port}")).expect("valid URL"));
    }

    let mut url = parse_with_default_scheme(raw, "http")?;
    let unspecified = match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_unspecified(),
        Some(Host::Ipv6(ip)) => ip.is_unspecified(),
        _ => false,
    };
    if unspecified {
        url.set_host(Some("localhost"))
            .expect("localhost is a valid host");
    }
    Ok(url)
}

pub fn parse_server(raw: &str) -> Result<Url, AddressError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(AddressError::Empty);
    }
    let host = raw.split(['/', ':']).next().unwrap_or_default();
    let local = matches!(host, "localhost" | "127.0.0.1") || host.starts_with('[');
    let scheme = if local { "http" } else { "https" };
    parse_with_default_scheme(raw, scheme)
}

pub(crate) fn unbracket(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host)
}

fn parse_with_default_scheme(raw: &str, scheme: &str) -> Result<Url, AddressError> {
    let with_scheme = match raw.split_once("://") {
        Some((given, _)) => {
            let given = given.to_ascii_lowercase();
            if given != "http" && given != "https" {
                return Err(AddressError::BadScheme(given));
            }
            raw.to_owned()
        }
        None => format!("{scheme}://{raw}"),
    };

    if let Some(port) = explicit_port(&with_scheme) {
        check_port(port)?;
    }

    let url = Url::parse(&with_scheme).map_err(|e| match e {
        url::ParseError::EmptyHost => AddressError::NoHost,
        _ => AddressError::Invalid(raw.to_owned()),
    })?;
    if url.host_str().is_none_or(str::is_empty) {
        return Err(AddressError::NoHost);
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(AddressError::HasCredentials);
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(AddressError::HasPath(raw.to_owned()));
    }
    Ok(url)
}

fn explicit_port(url: &str) -> Option<&str> {
    let authority = url.split_once("://")?.1.split(['/', '?', '#']).next()?;
    let host_end = authority.rfind(']').map_or(0, |i| i + 1);
    let (_, port) = authority[host_end..].rsplit_once(':')?;
    Some(port)
}

fn check_port(port: &str) -> Result<(), AddressError> {
    match port.parse::<u16>() {
        Ok(1..) => Ok(()),
        _ => Err(AddressError::BadPort(port.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(raw: &str) -> String {
        parse_target(raw).unwrap().to_string()
    }

    #[test]
    fn target_forms_people_type() {
        assert_eq!(target("3000"), "http://localhost:3000/");
        assert_eq!(target(":3000"), "http://localhost:3000/");
        assert_eq!(target("localhost:5173"), "http://localhost:5173/");
        assert_eq!(target("localhost"), "http://localhost/");
        assert_eq!(target("127.0.0.1:8000"), "http://127.0.0.1:8000/");
        assert_eq!(target("[::1]:8000"), "http://[::1]:8000/");
        assert_eq!(target("myapp.test:8080"), "http://myapp.test:8080/");
        assert_eq!(target("http://localhost:3000"), "http://localhost:3000/");
        assert_eq!(target("HTTP://localhost:3000/"), "http://localhost:3000/");
        assert_eq!(target("https://localhost:8443"), "https://localhost:8443/");
        assert_eq!(target("  localhost:3000/  "), "http://localhost:3000/");
    }

    #[test]
    fn unspecified_address_means_this_machine() {
        assert_eq!(target("0.0.0.0:3000"), "http://localhost:3000/");
        assert_eq!(target("http://0.0.0.0:3000/"), "http://localhost:3000/");
        assert_eq!(target("[::]:3000"), "http://localhost:3000/");
    }

    #[test]
    fn target_errors() {
        use AddressError::*;
        assert_eq!(parse_target(""), Err(Empty));
        assert_eq!(parse_target("   "), Err(Empty));
        assert_eq!(parse_target("0"), Err(BadPort("0".into())));
        assert_eq!(parse_target("70000"), Err(BadPort("70000".into())));
        assert_eq!(
            parse_target("localhost:99999"),
            Err(BadPort("99999".into()))
        );
        assert_eq!(parse_target("localhost:abc"), Err(BadPort("abc".into())));
        assert_eq!(
            parse_target("ftp://localhost"),
            Err(BadScheme("ftp".into()))
        );
        assert_eq!(
            parse_target("htp://localhost:3000"),
            Err(BadScheme("htp".into()))
        );
        assert_eq!(parse_target("http://"), Err(NoHost));
        assert_eq!(parse_target("http://:3000"), Err(NoHost));
        assert_eq!(
            parse_target("localhost:3000/app"),
            Err(HasPath("localhost:3000/app".into()))
        );
        assert_eq!(
            parse_target("localhost:3000?x=1"),
            Err(HasPath("localhost:3000?x=1".into()))
        );
        assert_eq!(parse_target("user:pw@localhost:3000"), Err(HasCredentials));
        assert!(matches!(parse_target("local host:3000"), Err(Invalid(_))));
    }

    #[test]
    fn server_scheme_defaults() {
        let server = |raw| parse_server(raw).unwrap().to_string();
        assert_eq!(server("nurl.pw"), "https://nurl.pw/");
        assert_eq!(server("localhost:8080"), "http://localhost:8080/");
        assert_eq!(server("127.0.0.1:8080"), "http://127.0.0.1:8080/");
        assert_eq!(server("http://nurl.pw"), "http://nurl.pw/");
        assert_eq!(server("https://nurl.pw/"), "https://nurl.pw/");
        assert_eq!(
            parse_server("nurl.pw/api"),
            Err(AddressError::HasPath("nurl.pw/api".into()))
        );
    }
}
