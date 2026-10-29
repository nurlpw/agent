use std::{
    fmt::Write as _,
    io::{self, Write},
    sync::{Mutex, PoisonError},
    time::Duration,
};

use anstyle::{Color, Effects, RgbColor, Style};
use chrono::{DateTime, Local};
use nurl_agent::{Event, Plan, RequestLog};
use qrcode::{EcLevel, QrCode};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

mod theme {
    use anstyle::RgbColor;

    pub const BG: RgbColor = RgbColor(0x1a, 0x1b, 0x26);
    pub const FG: RgbColor = RgbColor(0xc0, 0xca, 0xf5);
    pub const FG_DARK: RgbColor = RgbColor(0xa9, 0xb1, 0xd6);
    pub const COMMENT: RgbColor = RgbColor(0x56, 0x5f, 0x89);
    pub const LINE: RgbColor = RgbColor(0x3b, 0x42, 0x61);
    pub const BLUE: RgbColor = RgbColor(0x7a, 0xa2, 0xf7);
    pub const CYAN: RgbColor = RgbColor(0x7d, 0xcf, 0xff);
    pub const TEAL: RgbColor = RgbColor(0x73, 0xda, 0xca);
    pub const GREEN: RgbColor = RgbColor(0x9e, 0xce, 0x6a);
    pub const YELLOW: RgbColor = RgbColor(0xe0, 0xaf, 0x68);
    pub const ORANGE: RgbColor = RgbColor(0xff, 0x9e, 0x64);
    pub const RED: RgbColor = RgbColor(0xf7, 0x76, 0x8e);
    pub const MAGENTA: RgbColor = RgbColor(0xbb, 0x9a, 0xf7);
}

const LABEL_WIDTH: usize = 18;
const GAP: usize = 4;
const INDENT: &str = "  ";
const QUIET_ZONE: usize = 2;
const DEFAULT_TERMINAL_WIDTH: usize = 100;
const COLUMNS: [(&str, usize); 4] = [("ВРЕМЯ", 10), ("МЕТОД", 8), ("СТАТУС", 8), ("ДЛИТ.", 10)];

fn fg(color: RgbColor) -> Style {
    Style::new().fg_color(Some(Color::Rgb(color)))
}

fn bold(color: RgbColor) -> Style {
    fg(color).effects(Effects::BOLD)
}

fn link() -> Style {
    fg(theme::CYAN).effects(Effects::UNDERLINE)
}

#[derive(Default, Clone)]
struct Line(Vec<(Style, String)>);

impl Line {
    fn indented() -> Self {
        Self::default().plain(INDENT)
    }

    fn push(mut self, style: Style, text: impl Into<String>) -> Self {
        self.0.push((style, text.into()));
        self
    }

    fn plain(self, text: impl Into<String>) -> Self {
        self.push(Style::new(), text)
    }

    fn append(mut self, other: &Self) -> Self {
        self.0.extend_from_slice(&other.0);
        self
    }

    fn width(&self) -> usize {
        self.0.iter().map(|(_, text)| text.width()).sum()
    }

    fn render(&self) -> String {
        self.0.iter().fold(String::new(), |mut out, (style, text)| {
            let _ = write!(out, "{style}{text}{style:#}");
            out
        })
    }
}

pub struct Banner<'a> {
    pub public_url: &'a str,
    pub local_url: &'a str,
    pub plan: Option<&'a Plan>,
    pub account: Option<&'a str>,
    pub local_unreachable: Option<&'a str>,
    pub update: Option<&'a str>,
}

pub struct Ui {
    out: Mutex<anstream::Stdout>,
    err: Mutex<anstream::Stderr>,
    colors: bool,
}

impl Ui {
    pub fn new() -> Self {
        let out = anstream::stdout();
        let colors = out.current_choice() != anstream::ColorChoice::Never;
        Self {
            out: Mutex::new(out),
            err: Mutex::new(anstream::stderr()),
            colors,
        }
    }

    fn print(&self, lines: &[Line]) {
        let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
        write_lines(&mut *out, lines);
    }

    pub fn banner(&self, banner: &Banner) {
        let qr = qr_lines(banner.public_url, self.colors);
        let info = info_lines(banner);
        let qr_width = qr.first().map_or(0, Line::width);
        let info_width = info.iter().map(Line::width).max().unwrap_or(0);

        let mut lines = vec![Line::default()];
        if terminal_width() >= INDENT.len() + qr_width + GAP + info_width {
            let top = qr.len().saturating_sub(info.len()) / 2;
            lines.extend(qr.iter().enumerate().map(|(i, qr_line)| {
                let line = Line::indented().append(qr_line);
                match i.checked_sub(top).and_then(|j| info.get(j)) {
                    Some(info_line) => line.plain(" ".repeat(GAP)).append(info_line),
                    None => line,
                }
            }));
        } else {
            let blank = Line::default();
            lines.extend(
                info.iter()
                    .chain([&blank])
                    .chain(&qr)
                    .map(|line| Line::indented().append(line)),
            );
        }
        lines.push(Line::default());
        lines.extend(table_header());
        self.print(&lines);
    }

    pub fn event(&self, event: &Event) {
        match event {
            Event::Request(log) => self.print(&request_lines(log)),
            Event::Warning(message) => self.warning(message),
            Event::Reconnecting {
                attempt,
                delay,
                reason,
            } => self.print(&[Line::indented()
                .push(
                    fg(theme::YELLOW),
                    format!("↻ Переподключение через {} с", delay.as_secs().max(1)),
                )
                .push(
                    fg(theme::COMMENT),
                    format!(" (попытка {attempt}: {reason})"),
                )]),
            Event::Reconnected => {
                self.print(&[Line::indented().push(fg(theme::GREEN), "✓ Связь восстановлена")]);
            }
            Event::AddressChanged(url) => self.print(&[
                Line::indented()
                    .push(bold(theme::YELLOW), "▲ Адрес изменился: ")
                    .push(link(), url.as_str()),
                Line::indented().push(
                    fg(theme::FG_DARK),
                    "Сервер не дождался агента. Своё имя (nurl login, --name) не меняется.",
                ),
            ]),
        }
    }

    pub fn warning(&self, message: &str) {
        self.print(&[Line::indented().push(fg(theme::YELLOW), format!("! {message}"))]);
    }

    pub fn info(&self, message: &str) {
        self.print(&[Line::indented().push(fg(theme::FG_DARK), message)]);
    }

    pub fn success(&self, message: &str) {
        self.print(&[Line::indented().push(bold(theme::GREEN), format!("✓ {message}"))]);
    }

    pub fn login(&self, url: &str, code: &str) {
        let mut lines = vec![Line::default()];
        lines.extend(
            qr_lines(url, self.colors)
                .iter()
                .map(|line| Line::indented().append(line)),
        );
        lines.extend([
            Line::default(),
            Line::indented().push(fg(theme::FG), "Откройте ссылку и подтвердите вход:"),
            Line::indented().push(link(), url),
            Line::indented()
                .push(fg(theme::COMMENT), format!("{:<LABEL_WIDTH$}", "Код"))
                .push(bold(theme::MAGENTA), code),
            Line::default(),
            Line::indented().push(fg(theme::COMMENT), "Жду подтверждения… (Ctrl+C — отмена)"),
        ]);
        self.print(&lines);
    }

    pub async fn confirm(&self, question: &str) -> bool {
        let prompt = Line::indented()
            .push(fg(theme::FG), question)
            .push(fg(theme::COMMENT), " [Y/n] ");
        {
            let mut out = self.out.lock().unwrap_or_else(PoisonError::into_inner);
            let _ = write!(out, "{}", prompt.render());
            let _ = out.flush();
        }
        let answer = tokio::task::spawn_blocking(|| {
            let mut answer = String::new();
            io::stdin().read_line(&mut answer).map(|_| answer)
        })
        .await;
        answer.is_ok_and(|answer| {
            answer.is_ok_and(|answer| {
                matches!(
                    answer.trim().to_lowercase().as_str(),
                    "" | "y" | "yes" | "д" | "да"
                )
            })
        })
    }

    pub fn closed(&self) {
        self.print(&[
            Line::default(),
            Line::indented().push(fg(theme::COMMENT), "Туннель закрыт"),
        ]);
    }

    pub fn error(&self, message: &str, hint: Option<&str>) {
        let mut lines = vec![
            Line::default(),
            Line::indented().push(bold(theme::RED), capitalize(message)),
        ];
        if let Some(hint) = hint {
            let width = terminal_width().saturating_sub(INDENT.len()).clamp(30, 100);
            lines.extend(
                wrap(hint, width)
                    .into_iter()
                    .map(|chunk| Line::indented().push(fg(theme::RED), chunk)),
            );
        }
        let mut err = self.err.lock().unwrap_or_else(PoisonError::into_inner);
        write_lines(&mut *err, &lines);
    }
}

fn write_lines(out: &mut impl Write, lines: &[Line]) {
    for line in lines {
        let _ = writeln!(out, "{}", line.render());
    }
    let _ = out.flush();
}

fn info_lines(banner: &Banner) -> Vec<Line> {
    let row =
        |name: &str| Line::default().push(fg(theme::COMMENT), format!("{name:<LABEL_WIDTH$}"));

    let plan = match banner.plan {
        Some(plan) => {
            let line = row("Тариф").push(bold(theme::MAGENTA), plan.name.as_str());
            if plan.warning_page {
                line.push(fg(theme::COMMENT), " / ")
                    .push(fg(theme::YELLOW), "экран предупреждения")
            } else {
                line
            }
        }
        None => row("Тариф").push(fg(theme::COMMENT), "—"),
    };

    let mut lines = vec![
        Line::default().push(bold(theme::BLUE), "Nurl"),
        Line::default(),
        row("Публичный адрес").push(link(), banner.public_url),
        row("Локальный адрес").push(fg(theme::FG), banner.local_url),
        plan,
    ];
    if let Some(email) = banner.account {
        lines.push(row("Аккаунт").push(fg(theme::FG), email));
    }
    if let Some(version) = banner.update {
        lines.push(
            row("Обновление")
                .push(fg(theme::GREEN), format!("доступна {version}"))
                .push(fg(theme::COMMENT), " · nurl update"),
        );
    }
    if let Some(reason) = banner.local_unreachable {
        lines.extend([
            Line::default(),
            Line::default()
                .push(bold(theme::YELLOW), "▲ Приложение не отвечает: ")
                .push(fg(theme::YELLOW), reason),
            Line::default().push(
                fg(theme::FG_DARK),
                "Туннель уже открыт: запустите приложение, и запросы пойдут к нему.",
            ),
        ]);
    }
    lines
}

fn qr_lines(url: &str, colors: bool) -> Vec<Line> {
    let Ok(code) = QrCode::with_error_correction_level(url, EcLevel::L) else {
        return Vec::new();
    };
    let width = code.width();
    let modules = code.to_colors();
    let size = width + 2 * QUIET_ZONE;
    let inside = QUIET_ZONE..width + QUIET_ZONE;
    let dark = |x: usize, y: usize| {
        inside.contains(&x)
            && inside.contains(&y)
            && modules[(y - QUIET_ZONE) * width + (x - QUIET_ZONE)] == qrcode::Color::Dark
    };
    let ink = |x: usize, y: usize| dark(x, y) == colors;
    let style = Style::new()
        .fg_color(Some(Color::Rgb(theme::BG)))
        .bg_color(Some(Color::Rgb(theme::FG)));

    (0..size)
        .step_by(2)
        .map(|y| {
            let text: String = (0..size)
                .map(|x| {
                    let bottom = y + 1 < size && ink(x, y + 1);
                    match (ink(x, y), bottom) {
                        (true, true) => '█',
                        (true, false) => '▀',
                        (false, true) => '▄',
                        (false, false) => ' ',
                    }
                })
                .collect();
            Line::default().push(style, text)
        })
        .collect()
}

fn table_header() -> Vec<Line> {
    let header = COLUMNS
        .iter()
        .fold(Line::indented(), |line, (name, width)| {
            line.push(bold(theme::COMMENT), format!("{name:<width$}"))
        })
        .push(bold(theme::COMMENT), "ПУТЬ");
    let rule_width = terminal_width()
        .saturating_sub(INDENT.len() * 2)
        .clamp(40, 120);
    vec![
        header,
        Line::indented().push(fg(theme::LINE), "─".repeat(rule_width)),
    ]
}

fn request_lines(log: &RequestLog) -> Vec<Line> {
    let time = DateTime::<Local>::from(log.at)
        .format("%H:%M:%S")
        .to_string();
    let fixed = INDENT.len() + COLUMNS.iter().map(|(_, width)| width).sum::<usize>();
    let path_width = terminal_width().saturating_sub(fixed + 1).max(16);

    let mut lines = vec![
        Line::indented()
            .push(fg(theme::COMMENT), format!("{time:<10}"))
            .push(
                method_style(&log.method),
                format!("{:<8}", truncate(&log.method, 7)),
            )
            .push(status_style(log.status), format!("{:<8}", log.status))
            .push(
                duration_style(log.duration),
                format!("{:<10}", format_duration(log.duration)),
            )
            .push(fg(theme::FG), truncate(&log.uri, path_width)),
    ];
    if let Some(error) = &log.error {
        let width = terminal_width().saturating_sub(fixed + 3).max(16);
        lines.push(
            Line::default()
                .plain(" ".repeat(fixed))
                .push(fg(theme::RED), "↳ ")
                .push(fg(theme::RED), truncate(error, width)),
        );
    }
    lines
}

fn method_style(method: &str) -> Style {
    let color = match method {
        "GET" | "HEAD" => theme::BLUE,
        "POST" => theme::GREEN,
        "PUT" | "PATCH" => theme::ORANGE,
        "DELETE" => theme::RED,
        "WS" => theme::MAGENTA,
        _ => theme::CYAN,
    };
    bold(color)
}

fn status_style(status: u16) -> Style {
    let color = match status {
        100..=199 => theme::TEAL,
        200..=299 => theme::GREEN,
        300..=399 => theme::BLUE,
        400..=499 => theme::YELLOW,
        _ => theme::RED,
    };
    fg(color)
}

fn duration_style(duration: Duration) -> Style {
    let color = match duration.as_millis() {
        0..300 => theme::FG_DARK,
        300..1000 => theme::YELLOW,
        _ => theme::ORANGE,
    };
    fg(color)
}

fn format_duration(duration: Duration) -> String {
    let ms = duration.as_millis();
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.1} s", duration.as_secs_f64())
    }
}

fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(current) if current.width() + 1 + word.width() <= width => {
                current.push(' ');
                current.push_str(word);
            }
            _ => lines.push(word.to_owned()),
        }
    }
    lines
}

fn terminal_width() -> usize {
    terminal_size::terminal_size().map_or(DEFAULT_TERMINAL_WIDTH, |(width, _)| usize::from(width.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_is_square_and_decodable_shape() {
        let lines = qr_lines("https://i1g3ns22.nurl.pw", true);
        let width = lines[0].width();
        assert_eq!(lines.len(), width.div_ceil(2));
        assert!(lines.iter().all(|line| line.width() == width));
        let first_finder_row = &lines[1].0[0].1;
        assert!(
            first_finder_row
                .chars()
                .skip(2)
                .take(7)
                .all(|c| c == '█' || c == '▀')
        );
    }

    fn decode(lines: &[Line], ink_is_dark: bool) -> String {
        const SCALE: usize = 6;
        let rows: Vec<Vec<char>> = lines
            .iter()
            .map(|line| line.0[0].1.chars().collect())
            .collect();
        let (w, h) = (rows[0].len() * SCALE, rows.len() * 2 * SCALE);
        let mut image = rqrr::PreparedImage::prepare_from_greyscale(w, h, |x, y| {
            let c = rows[y / (2 * SCALE)][x / SCALE];
            let top = (y / SCALE).is_multiple_of(2);
            let ink = matches!((c, top), ('█', _) | ('▀', true) | ('▄', false));
            if ink == ink_is_dark { 0 } else { 255 }
        });
        let grids = image.detect_grids();
        assert_eq!(grids.len(), 1, "exactly one QR code expected");
        grids[0].decode().unwrap().1
    }

    #[test]
    fn qr_decodes_to_public_url() {
        for url in ["https://i1g3ns22.nurl.pw", "http://tfevu160.localhost:8080"] {
            assert_eq!(decode(&qr_lines(url, true), true), url);
            assert_eq!(decode(&qr_lines(url, false), false), url);
        }
    }

    #[test]
    fn truncates_by_display_width() {
        assert_eq!(truncate("/api/users", 20), "/api/users");
        assert_eq!(truncate("/очень/длинный/путь", 8), "/очень/…");
        assert_eq!(truncate("abcdef", 4).width(), 4);
    }

    #[test]
    fn wraps_by_words() {
        assert_eq!(
            wrap("один два три четыре", 9),
            ["один два", "три", "четыре"]
        );
        assert_eq!(wrap("", 9), Vec::<String>::new());
        assert_eq!(capitalize("не удалось"), "Не удалось");
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(Duration::from_millis(12)), "12 ms");
        assert_eq!(format_duration(Duration::from_millis(1530)), "1.5 s");
    }
}
