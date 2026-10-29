use std::{error::Error as StdError, io, iter};

use serde::Deserialize;
use url::Url;

use crate::{address::AddressError, proxy};

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("неверный адрес приложения: {0}")]
    BadTarget(AddressError),
    #[error("не удалось найти сервер {host}")]
    ServerNotFound { host: String },
    #[error("сервер {server} не принимает подключения")]
    ServerRefused { server: String },
    #[error("сервер {server} не ответил вовремя")]
    ServerTimeout { server: String },
    #[error("не удалось установить защищённое соединение с {server}")]
    Tls { server: String, detail: String },
    #[error("не удалось подключиться к серверу {server}: {detail}")]
    Network { server: String, detail: String },
    #[error("прокси {proxy} не отвечает")]
    ProxyDown { proxy: String, host: String },
    #[error("через прокси {proxy} не удалось подключиться к серверу {server}")]
    ProxyFailed {
        proxy: String,
        server: String,
        host: String,
        detail: String,
    },
    #[error("по адресу {server} отвечает не сервер Nurl")]
    NotNurl { server: String, detail: String },
    #[error("{}", rejected_text(.code.as_deref(), .message))]
    Rejected {
        status: u16,
        code: Option<String>,
        message: String,
    },
}

fn rejected_text(code: Option<&str>, message: &str) -> String {
    match code {
        Some("agent_token_revoked") => "вход агента отозван".into(),
        Some("sign_in_required") => "свои имена доступны только после входа в аккаунт".into(),
        Some("pro_required") => "эта возможность доступна на тарифе Pro".into(),
        Some("name_not_registered") => "имя не зарегистрировано в вашем аккаунте".into(),
        Some("name_taken") => "имя занято другим пользователем".into(),
        Some("tunnel_limit") => "с этого IP уже открыто максимальное число туннелей".into(),
        Some("account_frozen" | "account_blocked") => message.into(),
        _ => format!("сервер отказал: {message}"),
    }
}

impl StartError {
    #[must_use]
    pub fn hint(&self) -> Option<String> {
        let hint = match self {
            Self::BadTarget(_) => {
                "Примеры: nurl 3000, nurl localhost:5173, nurl http://127.0.0.1:8000".into()
            }
            Self::ServerNotFound { .. } | Self::Network { .. } => {
                "Проверьте подключение к интернету.".into()
            }
            Self::ServerRefused { .. } => {
                "Сервер Nurl недоступен. Попробуйте ещё раз через несколько минут.".into()
            }
            Self::ServerTimeout { .. } => {
                "Похоже на медленное или нестабильное подключение. Попробуйте ещё раз.".into()
            }
            Self::Tls { detail, .. } => format!(
                "Проверьте дату и время на компьютере и не перехватывает ли соединение \
                 антивирус или прокси. Подробности: {detail}"
            ),
            Self::ProxyDown { host, .. } => format!(
                "Агент берёт прокси из переменных HTTPS_PROXY / HTTP_PROXY / ALL_PROXY. \
                 Запустите VPN или прокси, либо подключайтесь напрямую: добавьте {host} \
                 в переменную NO_PROXY."
            ),
            Self::ProxyFailed { host, detail, .. } => format!(
                "Проверьте, что у прокси есть доступ в интернет. Чтобы подключаться напрямую, \
                 добавьте {host} в переменную NO_PROXY. Подробности: {detail}"
            ),
            Self::NotNurl { detail, .. } => format!(
                "Обновите агента: возможно, версии агента и сервера не совпадают. \
                 Подробности: {detail}"
            ),
            Self::Rejected { code, .. } => return rejected_hint(code.as_deref()).map(Into::into),
        };
        Some(hint)
    }

    #[must_use]
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Rejected { code, .. } => code.as_deref(),
            _ => None,
        }
    }

    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            Self::BadTarget(_) => 2,
            _ => 1,
        }
    }

    pub(crate) fn from_http(server: &Url, proxy: Option<&Url>, error: &reqwest::Error) -> Self {
        if error.is_timeout() {
            return Self::ServerTimeout {
                server: display_server(server),
            };
        }
        if error.is_decode() {
            return Self::not_nurl(server, "сервер прислал ответ в неожиданном формате");
        }
        classify_connect(server, proxy, error)
    }

    pub(crate) fn from_refusal(
        server: &Url,
        proxy: Option<&Url>,
        status: u16,
        body: &[u8],
    ) -> Self {
        #[derive(Deserialize)]
        struct ErrorBody {
            error: String,
            #[serde(default)]
            code: Option<String>,
        }

        match (serde_json::from_slice::<ErrorBody>(body), proxy) {
            (Ok(body), _) if status != 404 || body.code.is_some() => Self::Rejected {
                status,
                code: body.code,
                message: body.error,
            },
            (Err(_), Some(proxy)) if matches!(status, 502..=504) => Self::ProxyFailed {
                proxy: proxy::display(proxy),
                server: display_server(server),
                host: host_of(server),
                detail: format!("прокси ответил HTTP {status}"),
            },
            _ => Self::not_nurl(
                server,
                format!("на запрос открытия туннеля пришёл ответ HTTP {status}"),
            ),
        }
    }

    pub(crate) fn from_proxy(server: &Url, proxy_url: &Url, error: proxy::ProxyError) -> Self {
        match error {
            proxy::ProxyError::Unreachable(_) => Self::ProxyDown {
                proxy: proxy::display(proxy_url),
                host: host_of(server),
            },
            error => Self::ProxyFailed {
                proxy: proxy::display(proxy_url),
                server: display_server(server),
                host: host_of(server),
                detail: error.to_string(),
            },
        }
    }

    pub(crate) fn not_nurl(server: &Url, detail: impl Into<String>) -> Self {
        Self::NotNurl {
            server: display_server(server),
            detail: detail.into(),
        }
    }
}

fn rejected_hint(code: Option<&str>) -> Option<&'static str> {
    let hint = match code? {
        "agent_token_revoked" => "Выполните nurl login, чтобы войти снова.",
        "sign_in_required" => "Выполните nurl login.",
        "pro_required" => "Подключите Pro на сайте Nurl.",
        "name_not_registered" => {
            "Зарегистрируйте имя в панели Nurl или запустите nurl в терминале: \
             агент предложит это сделать."
        }
        "tunnel_limit" => {
            "Остановите один из запущенных туннелей и попробуйте снова. На Pro лимит выше."
        }
        "account_frozen" | "account_blocked" => "Напишите в поддержку на сайте Nurl.",
        _ => return None,
    };
    Some(hint)
}

pub(crate) fn classify_connect(
    server: &Url,
    proxy: Option<&Url>,
    error: &(dyn StdError + 'static),
) -> StartError {
    let server_name = display_server(server);
    let text = chain_text(error);

    if let Some(proxy) = proxy {
        return match io_kind(error) {
            Some(io::ErrorKind::ConnectionRefused) => StartError::ProxyDown {
                proxy: proxy::display(proxy),
                host: host_of(server),
            },
            Some(io::ErrorKind::TimedOut) => StartError::ServerTimeout {
                server: server_name,
            },
            _ => StartError::ProxyFailed {
                proxy: proxy::display(proxy),
                server: server_name,
                host: host_of(server),
                detail: text,
            },
        };
    }

    let lower = text.to_lowercase();
    let mentions = |needles: &[&str]| needles.iter().any(|needle| lower.contains(needle));
    if mentions(&["dns error", "failed to lookup", "no such host"]) {
        return StartError::ServerNotFound {
            host: host_of(server),
        };
    }
    if mentions(&["certificate", "tls", "ssl"]) {
        return StartError::Tls {
            server: server_name,
            detail: text,
        };
    }
    match io_kind(error) {
        Some(io::ErrorKind::ConnectionRefused) => StartError::ServerRefused {
            server: server_name,
        },
        Some(io::ErrorKind::TimedOut) => StartError::ServerTimeout {
            server: server_name,
        },
        _ => StartError::Network {
            server: server_name,
            detail: text,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownReason {
    Stopped,
    Revoked,
    Replaced,
    NameDeleted,
    PlanExpired,
    Frozen,
    Blocked,
    Deleted,
    Closed,
}

impl ShutdownReason {
    pub(crate) fn from_frame(reason: &str) -> Self {
        match reason {
            "stopped" => Self::Stopped,
            "revoked" => Self::Revoked,
            "replaced" => Self::Replaced,
            "name_deleted" => Self::NameDeleted,
            "plan_expired" => Self::PlanExpired,
            "frozen" => Self::Frozen,
            "blocked" => Self::Blocked,
            "deleted" => Self::Deleted,
            _ => Self::Closed,
        }
    }

    pub(crate) fn from_code(code: &str) -> Self {
        match code {
            "tunnel_stopped" => Self::Stopped,
            "tunnel_replaced" => Self::Replaced,
            "name_deleted" => Self::NameDeleted,
            "agent_token_revoked" => Self::Revoked,
            "plan_expired" => Self::PlanExpired,
            "account_frozen" => Self::Frozen,
            "account_blocked" => Self::Blocked,
            "account_deleted" => Self::Deleted,
            _ => Self::Closed,
        }
    }

    const fn message(self) -> &'static str {
        match self {
            Self::Stopped => "Туннель остановлен из панели Nurl.",
            Self::Replaced => "Это имя занял агент, запущенный позже.",
            Self::NameDeleted => "Имя удалено в панели Nurl.",
            Self::Revoked => "Вход этого агента отозван в панели Nurl.",
            Self::PlanExpired => "Подписка Pro закончилась. Продлите её в панели Nurl.",
            Self::Frozen => "Аккаунт заморожен: туннели не работают, пока его не разморозят.",
            Self::Blocked => "Аккаунт заблокирован. Подробности — в поддержке Nurl.",
            Self::Deleted => "Аккаунт удалён.",
            Self::Closed => "Сервер закрыл туннель.",
        }
    }

    const fn hint(self) -> Option<&'static str> {
        match self {
            Self::Revoked => Some("Выполните nurl login, чтобы войти снова."),
            Self::Replaced => Some("Остановите другой агент, если хотите вернуть имя сюда."),
            Self::NameDeleted => Some("Зарегистрируйте имя снова или запустите nurl без --name."),
            Self::PlanExpired => Some("Продлите Pro в панели или запустите nurl без --name."),
            Self::Frozen | Self::Blocked => Some("Напишите в поддержку на сайте Nurl."),
            Self::Stopped | Self::Deleted | Self::Closed => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("{message}")]
    Shutdown {
        reason: ShutdownReason,
        message: String,
    },
    #[error("{0}")]
    Reopen(StartError),
}

impl RunError {
    pub(crate) fn shutdown(reason: ShutdownReason) -> Self {
        Self::Shutdown {
            reason,
            message: reason.message().into(),
        }
    }

    #[must_use]
    pub fn hint(&self) -> Option<String> {
        match self {
            Self::Shutdown { reason, .. } => reason.hint().map(Into::into),
            Self::Reopen(e) => e.hint(),
        }
    }

    #[must_use]
    pub fn is_revoked(&self) -> bool {
        match self {
            Self::Shutdown { reason, .. } => *reason == ShutdownReason::Revoked,
            Self::Reopen(e) => e.code() == Some("agent_token_revoked"),
        }
    }

    #[must_use]
    pub fn is_stopped(&self) -> bool {
        matches!(
            self,
            Self::Shutdown {
                reason: ShutdownReason::Stopped,
                ..
            }
        )
    }
}

pub(crate) fn display_server(server: &Url) -> String {
    server.as_str().trim_end_matches('/').to_owned()
}

fn host_of(server: &Url) -> String {
    server.host_str().unwrap_or_default().to_owned()
}

fn sources<'a>(
    error: &'a (dyn StdError + 'static),
) -> impl Iterator<Item = &'a (dyn StdError + 'static)> {
    iter::successors(Some(error), |&e| e.source())
}

fn io_kind(error: &(dyn StdError + 'static)) -> Option<io::ErrorKind> {
    sources(error)
        .find_map(|e| e.downcast_ref::<io::Error>())
        .map(io::Error::kind)
}

pub(crate) fn chain_text(error: &(dyn StdError + 'static)) -> String {
    let mut parts: Vec<String> = Vec::new();
    for text in sources(error).map(ToString::to_string) {
        if !parts.iter().any(|part| part.contains(&text)) {
            parts.push(text);
        }
    }
    parts.join(": ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("{message}")]
    struct Wrapper {
        message: String,
        #[source]
        source: io::Error,
    }

    impl Wrapper {
        fn new(message: &str, source: impl Into<io::Error>) -> Self {
            Self {
                message: message.into(),
                source: source.into(),
            }
        }
    }

    fn server() -> Url {
        Url::parse("https://nurl.pw").unwrap()
    }

    #[test]
    fn refused_and_timeout_by_io_kind() {
        let refused = Wrapper::new("client error (Connect)", io::ErrorKind::ConnectionRefused);
        assert!(matches!(
            classify_connect(&server(), None, &refused),
            StartError::ServerRefused { .. }
        ));

        let timeout = Wrapper::new("connect", io::ErrorKind::TimedOut);
        assert!(matches!(
            classify_connect(&server(), None, &timeout),
            StartError::ServerTimeout { .. }
        ));
    }

    #[test]
    fn dns_failure_names_the_host() {
        let dns = Wrapper::new(
            "dns error",
            io::Error::other("No such host is known. (os error 11001)"),
        );
        let error = classify_connect(&server(), None, &dns);
        assert!(matches!(&error, StartError::ServerNotFound { host } if host == "nurl.pw"));
        assert_eq!(error.to_string(), "не удалось найти сервер nurl.pw");
        assert!(error.hint().unwrap().contains("интернет"));
    }

    #[test]
    fn proxy_errors() {
        let proxy = Url::parse("http://user:pw@127.0.0.1:10808").unwrap();
        let refused = Wrapper::new("connect", io::ErrorKind::ConnectionRefused);
        let error = classify_connect(&server(), Some(&proxy), &refused);
        assert!(
            matches!(&error, StartError::ProxyDown { proxy, .. } if proxy == "http://127.0.0.1:10808")
        );
        assert!(error.hint().unwrap().contains("NO_PROXY"));

        let eof = Wrapper::new("tls handshake eof", io::Error::other("eof"));
        let error = classify_connect(&server(), Some(&proxy), &eof);
        assert!(matches!(error, StartError::ProxyFailed { .. }), "{error:?}");
    }

    #[test]
    fn chain_text_skips_repeats() {
        let error = Wrapper::new("IO error: boom", io::Error::other("boom"));
        assert_eq!(chain_text(&error), "IO error: boom");
    }

    #[test]
    fn exit_codes() {
        assert_eq!(StartError::BadTarget(AddressError::Empty).exit_code(), 2);
        let refused = StartError::ServerRefused { server: "x".into() };
        assert_eq!(refused.exit_code(), 1);
    }

    #[test]
    fn coded_refusals_are_nurl_even_on_404() {
        let body = br#"{"error":"name x is not registered","code":"name_not_registered"}"#;
        let error = StartError::from_refusal(&server(), None, 404, body);
        assert_eq!(error.code(), Some("name_not_registered"));

        let error = StartError::from_refusal(&server(), None, 404, br#"{"error":"nope"}"#);
        assert!(matches!(error, StartError::NotNurl { .. }));
    }

    #[test]
    fn shutdown_reasons() {
        assert_eq!(
            ShutdownReason::from_frame("revoked"),
            ShutdownReason::Revoked
        );
        assert_eq!(
            ShutdownReason::from_code("account_frozen"),
            ShutdownReason::Frozen
        );
        assert_eq!(
            ShutdownReason::from_code("whatever"),
            ShutdownReason::Closed
        );
        assert!(RunError::shutdown(ShutdownReason::Stopped).is_stopped());
        assert!(RunError::shutdown(ShutdownReason::Revoked).is_revoked());
    }
}
