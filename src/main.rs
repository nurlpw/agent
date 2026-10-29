mod ui;

use std::{
    io::IsTerminal,
    process::{Command as Process, ExitCode, Stdio},
    sync::Arc,
    time::Duration,
};

use clap::{Parser, Subcommand};
use nurl_agent::{
    Api, DeviceInfo, OpenOptions, RunError, StartError, Tunnel,
    api::LoginPoll,
    credentials::{self, Credentials},
    update,
};
use tokio::time::Instant;
use url::Url;

use crate::ui::{Banner, Ui};

const SERVER: &str = match option_env!("NURL_SERVER_URL") {
    Some(url) if cfg!(debug_assertions) => url,
    _ => "https://nurl.pw",
};

#[derive(Parser)]
#[command(name = "nurl", version, about, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(
        help = "Локальный адрес: `3000`, `localhost:3000` или `http://localhost:3000`. \
                Со своим именем можно не указывать, если у имени задан локальный адрес."
    )]
    target: Option<String>,

    #[arg(
        value_name = "ИМЯ",
        help = "Своё имя (Pro): `nurl 3000 myshop` откроет туннель на `myshop.<домен>` \
                и сохранит адрес между запусками."
    )]
    name_arg: Option<String>,

    #[arg(
        short,
        long,
        value_name = "ИМЯ",
        conflicts_with = "name_arg",
        help = "Своё имя флагом: `nurl --name myshop` запускает имя без адреса, \
                если у имени задан локальный адрес по умолчанию."
    )]
    name: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    #[command(about = "Войти в аккаунт: туннели получат тариф аккаунта и появятся в панели.")]
    Login,
    #[command(about = "Выйти из аккаунта на этом компьютере.")]
    Logout,
    #[command(about = "Обновить агента до последней версии.")]
    Update,
}

enum Failure {
    Start(StartError),
    Run(RunError),
    Exit(u8),
}

impl From<StartError> for Failure {
    fn from(e: StartError) -> Self {
        Self::Start(e)
    }
}

impl From<RunError> for Failure {
    fn from(e: RunError) -> Self {
        Self::Run(e)
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    update::cleanup();
    let cli = Cli::parse();
    let ui = Arc::new(Ui::new());
    let result = match cli.command {
        Some(Command::Login) => login(&ui).await,
        Some(Command::Logout) => logout(&ui).await,
        Some(Command::Update) => self_update(&ui).await,
        None => Box::pin(run(cli, Arc::clone(&ui))).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Start(e)) => {
            ui.error(&e.to_string(), e.hint().as_deref());
            ExitCode::from(e.exit_code())
        }
        Err(Failure::Run(e)) => {
            ui.error(&e.to_string(), e.hint().as_deref());
            if e.is_stopped() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(Failure::Exit(code)) => ExitCode::from(code),
    }
}

fn server_url() -> Url {
    nurl_agent::parse_server(SERVER).expect("NURL_SERVER_URL must be a valid server address")
}

async fn run(cli: Cli, ui: Arc<Ui>) -> Result<(), Failure> {
    let target = cli
        .target
        .as_deref()
        .map(nurl_agent::parse_target)
        .transpose()
        .map_err(StartError::BadTarget)?;
    let name = cli
        .name_arg
        .or(cli.name)
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty());
    if target.is_none() && name.is_none() {
        ui.error(
            "не указан локальный адрес",
            Some(
                "Примеры: nurl 3000, nurl localhost:5173, nurl 3000 myshop. \
                 Вход в аккаунт: nurl login",
            ),
        );
        return Err(Failure::Exit(2));
    }

    let server = server_url();
    let account = credentials::load(&server);
    let api = Api::from_env(&server);
    let options = OpenOptions {
        target,
        name,
        token: account.as_ref().map(|account| account.token.clone()),
        device: DeviceInfo::current(),
    };

    let probe = async {
        match &options.target {
            Some(target) => Some(nurl_agent::probe_local(target).await),
            None => None,
        }
    };
    let (opened, local) = tokio::join!(
        open_tunnel(&api, &ui, account.as_ref(), options.clone()),
        probe
    );
    let tunnel = opened?;
    let local = match local {
        Some(result) => result,
        None => nurl_agent::probe_local(&tunnel.target).await,
    };

    let update = tunnel
        .latest_version
        .as_deref()
        .filter(|latest| update::is_older(update::VERSION, latest));
    ui.banner(&Banner {
        public_url: &tunnel.public_url,
        local_url: tunnel.target.as_str().trim_end_matches('/'),
        plan: tunnel.plan.as_ref(),
        account: tunnel
            .account
            .as_ref()
            .map(|account| account.email.as_str()),
        local_unreachable: local.as_ref().err().map(String::as_str),
        update,
    });

    let events = Arc::clone(&ui);
    let result = tunnel
        .on_event(move |event| events.event(&event))
        .run(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
    if let Err(e) = result {
        if e.is_revoked() {
            forget(&server, &ui);
        }
        return Err(e.into());
    }
    ui.closed();
    Ok(())
}

async fn open_tunnel(
    api: &Api,
    ui: &Ui,
    account: Option<&Credentials>,
    options: OpenOptions,
) -> Result<Tunnel, Failure> {
    match nurl_agent::open_with(api.clone(), options.clone()).await {
        Ok(tunnel) => Ok(tunnel),
        Err(e) if e.code() == Some("agent_token_revoked") => {
            forget(api.server(), ui);
            Err(e.into())
        }
        Err(e) if e.code() == Some("name_not_registered") => {
            let (Some(name), Some(account)) = (options.name.as_deref(), account) else {
                return Err(e.into());
            };
            if !offer_registration(api, ui, account, name, options.target.as_ref()).await? {
                return Err(e.into());
            }
            Ok(nurl_agent::open_with(api.clone(), options).await?)
        }
        Err(e) => Err(e.into()),
    }
}

fn forget(server: &Url, ui: &Ui) {
    if let Err(e) = credentials::remove(server) {
        ui.warning(&format!("не удалось удалить токен из конфига: {e}"));
    }
}

async fn offer_registration(
    api: &Api,
    ui: &Ui,
    account: &Credentials,
    name: &str,
    target: Option<&Url>,
) -> Result<bool, Failure> {
    let host = format!("{name}.{}", api.server().host_str().unwrap_or_default());
    let check = api.check_name(&account.token, name).await?;
    let question = match check.status.as_str() {
        "available" => format!("Имя {host} свободно. Зарегистрировать?"),
        "yours_deleted" => format!("Имя {host} удалено, но ещё ваше. Вернуть его?"),
        _ => {
            let reason = check.message.trim_end_matches('.');
            ui.error(&format!("имя {host} не зарегистрировано: {reason}"), None);
            if !check.suggestions.is_empty() {
                ui.info(&format!("Свободны: {}", check.suggestions.join(", ")));
            }
            return Err(Failure::Exit(1));
        }
    };
    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    if !ui.confirm(&question).await {
        return Err(Failure::Exit(1));
    }
    api.register_name(&account.token, name, target.map(Url::as_str))
        .await?;
    ui.success(&format!("Имя {host} зарегистрировано"));
    Ok(true)
}

async fn login(ui: &Ui) -> Result<(), Failure> {
    let server = server_url();
    let api = Api::from_env(&server);
    let previous = credentials::load(&server);

    let start = api.start_login(&DeviceInfo::current()).await?;
    ui.login(&start.verification_url, &start.user_code);
    open_browser(&start.verification_url);

    let deadline = Instant::now() + Duration::from_secs(start.expires_in_seconds);
    let interval = Duration::from_secs(start.interval_seconds.max(1));
    loop {
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            _ = tokio::signal::ctrl_c() => return Err(Failure::Exit(130)),
        }
        match api.poll_login(&start.device_code).await {
            Ok(LoginPoll::Pending) => {}
            Ok(LoginPoll::Approved { token, user }) => {
                let credentials = Credentials {
                    token,
                    email: user.email.clone(),
                    name: user.name.clone(),
                };
                let path = credentials::save(&server, credentials).map_err(|e| {
                    ui.error(&format!("не удалось сохранить токен: {e}"), None);
                    Failure::Exit(1)
                })?;
                if let Some(previous) = previous {
                    let _ = api.logout(&previous.token).await;
                }
                ui.success(&format!(
                    "Вы вошли как {} ({}), тариф {}",
                    user.name, user.email, user.plan.name
                ));
                ui.info(&format!("Токен сохранён в {}", path.display()));
                return Ok(());
            }
            Ok(LoginPoll::Denied) => {
                ui.error("вход отклонён в браузере", None);
                return Err(Failure::Exit(1));
            }
            Err(e) if e.code() == Some("login_expired") => return Err(login_expired(ui)),
            Err(_) if Instant::now() < deadline => {}
            Err(e) => return Err(e.into()),
        }
        if Instant::now() >= deadline {
            return Err(login_expired(ui));
        }
    }
}

fn login_expired(ui: &Ui) -> Failure {
    ui.error("код входа истёк", Some("Выполните nurl login ещё раз."));
    Failure::Exit(1)
}

async fn logout(ui: &Ui) -> Result<(), Failure> {
    let server = server_url();
    let Some(account) = credentials::load(&server) else {
        ui.info("На этом компьютере вход не выполнен.");
        return Ok(());
    };
    let revoked = Api::from_env(&server).logout(&account.token).await;
    forget(&server, ui);
    match revoked {
        Ok(()) | Err(StartError::Rejected { status: 401, .. }) => {
            ui.success(&format!("Вы вышли из аккаунта {}", account.email));
        }
        Err(e) => {
            ui.success(&format!(
                "Токен {} удалён с этого компьютера",
                account.email
            ));
            ui.warning(&format!(
                "Отозвать его на сервере не удалось ({e}): это можно сделать в панели, \
                 в разделе «Сессии»."
            ));
        }
    }
    Ok(())
}

async fn self_update(ui: &Ui) -> Result<(), Failure> {
    let api = Api::from_env(&server_url());
    let info = api.version().await?;
    if !update::is_older(update::VERSION, &info.latest) {
        ui.success(&format!("У вас последняя версия: {}", update::VERSION));
        return Ok(());
    }
    if update::installed_by_winget() {
        ui.info(&format!(
            "Доступна версия {}. Агент установлен через winget, обновите его так:",
            info.latest
        ));
        ui.info("winget upgrade Nurl.Nurl");
        return Ok(());
    }
    ui.info(&format!("Обновляю {} → {}…", update::VERSION, info.latest));
    match update::install(&api, &info).await {
        Ok(path) => {
            ui.success(&format!(
                "Nurl обновлён до {}: {}",
                info.latest,
                path.display()
            ));
            Ok(())
        }
        Err(e) => {
            ui.error(
                &format!("не удалось обновить агента: {e:#}"),
                Some("Скачайте новую версию вручную: curl -fsSL <адрес сервера>/install.sh | sh"),
            );
            Err(Failure::Exit(1))
        }
    }
}

fn open_browser(url: &str) {
    let (program, args): (&str, &[&str]) = if cfg!(windows) {
        ("rundll32", &["url.dll,FileProtocolHandler"])
    } else if cfg!(target_os = "macos") {
        ("open", &[])
    } else {
        ("xdg-open", &[])
    };
    let _ = Process::new(program)
        .args(args)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("nurl").chain(args.iter().copied()))
    }

    #[test]
    fn name_is_positional_or_flag() {
        let cli = parse(&["5000"]).unwrap();
        assert_eq!(
            (cli.target.as_deref(), cli.name_arg, cli.name),
            (Some("5000"), None, None)
        );

        let cli = parse(&["5000", "myshop"]).unwrap();
        assert_eq!(
            (cli.target.as_deref(), cli.name_arg.as_deref()),
            (Some("5000"), Some("myshop"))
        );

        let cli = parse(&["127.0.0.1:5000", "--name", "myshop"]).unwrap();
        assert_eq!(
            (cli.target.as_deref(), cli.name.as_deref()),
            (Some("127.0.0.1:5000"), Some("myshop"))
        );

        let cli = parse(&["--name", "myshop"]).unwrap();
        assert_eq!((cli.target, cli.name.as_deref()), (None, Some("myshop")));

        let cli = parse(&["-n", "myshop"]).unwrap();
        assert_eq!((cli.target, cli.name.as_deref()), (None, Some("myshop")));

        assert!(parse(&["5000", "myshop", "--name", "other"]).is_err());
        assert!(parse(&["5000", "myshop", "extra"]).is_err());
    }

    #[test]
    fn built_in_server_is_valid() {
        server_url();
        assert!(parse(&["5000", "--server", "nurl.pw"]).is_err());
    }

    #[test]
    fn subcommands_still_win() {
        assert!(matches!(
            parse(&["login"]).unwrap().command,
            Some(Command::Login)
        ));
    }
}
