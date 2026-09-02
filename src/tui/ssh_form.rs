//! The SSH target form (PRD §7.4).
//!
//! Text fields only: name, host, port, user, identity, docker. Submitting
//! does *not* register anything — it scans the host keys and hands them to a
//! confirmation modal, because a target may only be stored with a fingerprint
//! the user approved on screen (TAR-005).

use crate::core::model::AuthType;
use crate::core::target::SshSpec;
use crate::tui::chrome;
use crate::tui::form::{text_window, Widgets};
use crate::tui::rows::pad;
use crate::tui::theme::Theme;
use ratatui::text::{Line, Span};

/// The SSH popup is 72 columns; the form sits two cells inside its border.
const FORM_COLS: u16 = 68;
const LABEL: u16 = 14;
/// Interior of a text box, at most. The leftover is for a verdict that this
/// form does not currently print — it keeps the box from eating the row.
const VALUE: u16 = 28;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Host,
    Port,
    User,
    Identity,
    Docker,
}

impl Field {
    pub const ALL: [Field; 6] = [
        Field::Name,
        Field::Host,
        Field::Port,
        Field::User,
        Field::Identity,
        Field::Docker,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Field::Name => "표시 이름",
            Field::Host => "호스트",
            Field::Port => "SSH 포트",
            Field::User => "사용자명",
            Field::Identity => "개인키 경로",
            Field::Docker => "docker 명령",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Field::Name => "TUI와 CLI에서 이 Target을 부르는 이름",
            Field::Host => "호스트명, IP 또는 Tailscale 주소",
            Field::Port => "기본 22",
            Field::User => "비우면 ssh 기본 사용자",
            Field::Identity => "비우면 ssh-agent 사용",
            Field::Docker => "원격에서 실행할 docker 경로",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshForm {
    pub focus: Field,
    name: String,
    host: String,
    port: String,
    user: String,
    identity: String,
    docker: String,
    /// Set while the host-key scan is in flight, so the footer can say so.
    pub scanning: bool,
}

impl Default for SshForm {
    fn default() -> Self {
        Self::new()
    }
}

impl SshForm {
    pub fn new() -> Self {
        Self {
            focus: Field::Name,
            name: String::new(),
            host: String::new(),
            port: "22".into(),
            user: String::new(),
            identity: String::new(),
            docker: "docker".into(),
            scanning: false,
        }
    }

    pub fn title(&self) -> String {
        "새 Target · SSH".to_string()
    }

    fn value(&self, field: Field) -> &str {
        match field {
            Field::Name => &self.name,
            Field::Host => &self.host,
            Field::Port => &self.port,
            Field::User => &self.user,
            Field::Identity => &self.identity,
            Field::Docker => &self.docker,
        }
    }

    fn value_mut(&mut self, field: Field) -> &mut String {
        match field {
            Field::Name => &mut self.name,
            Field::Host => &mut self.host,
            Field::Port => &mut self.port,
            Field::User => &mut self.user,
            Field::Identity => &mut self.identity,
            Field::Docker => &mut self.docker,
        }
    }

    pub fn type_char(&mut self, c: char) -> bool {
        if c.is_control() {
            return false;
        }
        // A port is a number; refusing the keystroke beats a parse error at
        // submit time.
        if self.focus == Field::Port && !c.is_ascii_digit() {
            return false;
        }
        self.value_mut(self.focus).push(c);
        true
    }

    pub fn backspace(&mut self) -> bool {
        self.value_mut(self.focus).pop().is_some()
    }

    pub fn next_field(&mut self) {
        let i = Field::ALL
            .iter()
            .position(|f| *f == self.focus)
            .unwrap_or(0);
        self.focus = Field::ALL[(i + 1) % Field::ALL.len()];
    }

    pub fn prev_field(&mut self) {
        let i = Field::ALL
            .iter()
            .position(|f| *f == self.focus)
            .unwrap_or(0);
        self.focus = Field::ALL[(i + Field::ALL.len() - 1) % Field::ALL.len()];
    }

    /// `Err` with the first thing the user must fix, so the footer can say it.
    pub fn check(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("표시 이름을 입력하세요.".into());
        }
        if self.host.trim().is_empty() {
            return Err("호스트를 입력하세요.".into());
        }
        match self.port.parse::<u16>() {
            Ok(0) | Err(_) => return Err("SSH 포트는 1–65535 사이 숫자입니다.".into()),
            Ok(_) => {}
        }
        if self.docker.trim().is_empty() {
            return Err("docker 명령을 입력하세요.".into());
        }
        Ok(())
    }

    pub fn is_valid(&self) -> bool {
        self.check().is_ok()
    }

    pub fn port(&self) -> u16 {
        self.port.parse().unwrap_or(22)
    }

    pub fn host(&self) -> String {
        self.host.trim().to_string()
    }

    /// The spec `core` will register, once a fingerprint is approved.
    pub fn spec(&self) -> SshSpec {
        let identity = self.identity.trim();
        SshSpec {
            display_name: self.name.trim().to_string(),
            host: self.host(),
            port: self.port(),
            username: some_if_set(&self.user),
            auth: if identity.is_empty() {
                AuthType::Agent
            } else {
                AuthType::Key
            },
            identity_path: some_if_set(identity),
            docker_command: self.docker.trim().to_string(),
        }
    }

    pub fn lines(&self, theme: &Theme) -> Vec<Line<'static>> {
        let g = Widgets::of(theme);
        let mut lines = vec![
            Line::from(Span::styled(
                "SSH로 접근하는 원격 Docker를 Target으로 등록합니다.".to_string(),
                theme.normal(),
            )),
            Line::from(Span::styled(
                "저장 전에 호스트 키 지문을 직접 확인하게 됩니다.".to_string(),
                theme.muted(),
            )),
            Line::raw(String::new()),
            chrome::titled_rule("접속 정보", FORM_COLS, theme),
        ];

        for field in Field::ALL {
            let focused = field == self.focus;
            let bar = if focused { g.bar_on } else { g.bar };
            let frame = if focused {
                theme.accent()
            } else {
                theme.muted()
            };
            // Placeholders live in the unfocused empty box so the user can
            // still see the default; once the field has the keyboard the
            // real (possibly empty) value and the cursor take over.
            let (shown, placeholder) = match (field, self.value(field).is_empty()) {
                (Field::User, true) if !focused => ("(ssh 기본값)".to_string(), true),
                (Field::Identity, true) if !focused => ("(ssh-agent)".to_string(), true),
                _ => {
                    let mut value = self.value(field).to_string();
                    if focused {
                        value.push('_');
                    }
                    (value, false)
                }
            };
            let interior = FORM_COLS.saturating_sub(2 + LABEL + 4).clamp(8, VALUE);
            lines.push(Line::from(vec![
                Span::styled(g.lead(focused), theme.accent()),
                Span::styled(pad(field.label(), LABEL as usize), theme.muted()),
                Span::styled(format!("{bar} "), frame),
                Span::styled(
                    text_window(&shown, interior as usize),
                    if placeholder {
                        theme.muted()
                    } else {
                        theme.normal()
                    },
                ),
                Span::styled(format!(" {bar}"), frame),
            ]));
            if focused {
                // Sit under the box, not under the label: the hint is about
                // the value, not the field name.
                lines.push(Line::from(Span::styled(
                    format!("{}{}", " ".repeat((2 + LABEL) as usize), field.hint()),
                    theme.muted(),
                )));
            }
        }

        lines.push(Line::raw(String::new()));
        match self.check() {
            Err(problem) => lines.push(Line::from(Span::styled(problem, theme.warn()))),
            Ok(()) if self.scanning => lines.push(Line::from(Span::styled(
                format!("호스트 키를 조회하는 중입니다{}", theme.ellipsis()),
                theme.accent(),
            ))),
            Ok(()) => lines.push(Line::from(Span::styled(
                format!(
                    "Ctrl+S — {}:{} 호스트 키를 조회한 뒤 승인 화면을 엽니다.",
                    self.host(),
                    self.port()
                ),
                theme.ok(),
            ))),
        }
        lines
    }
}

fn some_if_set(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::rows::render_lines;

    fn typed(form: &mut SshForm, field: Field, text: &str) {
        form.focus = field;
        for c in text.chars() {
            form.type_char(c);
        }
    }

    fn unicode() -> Theme {
        Theme {
            color: false,
            unicode: true,
            reduced_motion: true,
        }
    }

    /// The drawn line for `field`, counting the extra hint row under the
    /// focused field so the index still matches what the user sees.
    fn row_of(form: &SshForm, text: &str, field: Field) -> String {
        let mut i = 4usize; // intro, intro, blank, titled rule
        for f in Field::ALL {
            if f == field {
                return text
                    .lines()
                    .nth(i)
                    .unwrap_or_else(|| panic!("no line for {field:?}:\n{text}"))
                    .to_string();
            }
            i += 1;
            if f == form.focus {
                i += 1;
            }
        }
        panic!("{field:?} is not a field of the SSH form");
    }

    #[test]
    fn a_fresh_form_states_what_is_missing_instead_of_looking_ready() {
        let form = SshForm::new();
        assert!(!form.is_valid());
        assert_eq!(form.check().unwrap_err(), "표시 이름을 입력하세요.");
        let text = render_lines(&form.lines(&Theme::plain()));
        assert!(text.contains("표시 이름"));
        assert!(text.contains("호스트"));
        assert!(text.contains("접속 정보"));
    }

    #[test]
    fn agent_auth_is_the_default_and_a_key_path_switches_it() {
        let mut form = SshForm::new();
        typed(&mut form, Field::Name, "dev-vps");
        typed(&mut form, Field::Host, "vps.ts.net");
        assert!(form.is_valid());

        let spec = form.spec();
        assert_eq!(spec.display_name, "dev-vps");
        assert_eq!(spec.host, "vps.ts.net");
        assert_eq!(spec.port, 22);
        assert_eq!(spec.auth, AuthType::Agent);
        assert_eq!(spec.identity_path, None);
        assert_eq!(spec.username, None);
        assert_eq!(spec.docker_command, "docker");

        typed(&mut form, Field::Identity, "~/.ssh/id_ed25519");
        let spec = form.spec();
        assert_eq!(spec.auth, AuthType::Key);
        assert_eq!(spec.identity_path.as_deref(), Some("~/.ssh/id_ed25519"));
    }

    #[test]
    fn the_port_field_refuses_anything_but_digits() {
        let mut form = SshForm::new();
        form.focus = Field::Port;
        form.backspace();
        form.backspace();
        assert!(!form.type_char('x'));
        assert!(form.type_char('2'));
        assert!(form.type_char('2'));
        assert_eq!(form.port(), 22);
    }

    #[test]
    fn port_zero_is_rejected_rather_than_sent_to_ssh() {
        let mut form = SshForm::new();
        typed(&mut form, Field::Name, "n");
        typed(&mut form, Field::Host, "h");
        form.focus = Field::Port;
        form.backspace();
        form.backspace();
        form.type_char('0');
        assert!(!form.is_valid());
    }

    #[test]
    fn tab_walks_every_field_and_wraps() {
        let mut form = SshForm::new();
        for expected in Field::ALL.into_iter().skip(1) {
            form.next_field();
            assert_eq!(form.focus, expected);
        }
        form.next_field();
        assert_eq!(form.focus, Field::Name);
        form.prev_field();
        assert_eq!(form.focus, Field::Docker);
    }

    #[test]
    fn the_focused_field_is_marked_without_relying_on_colour() {
        let mut form = SshForm::new();
        typed(&mut form, Field::Name, "dev-vps");
        form.focus = Field::Host;
        typed(&mut form, Field::Host, "vps");

        let text = render_lines(&form.lines(&Theme::plain()));
        let focused = row_of(&form, &text, Field::Host);
        assert!(focused.starts_with("> "), "no gutter mark: {focused:?}");
        assert!(focused.contains("vps_"), "no cursor: {focused:?}");

        let quiet = row_of(&form, &text, Field::Name);
        assert!(quiet.starts_with("  "), "{quiet:?}");
        assert!(!quiet.contains("dev-vps_"), "{quiet:?}");
    }

    #[test]
    fn a_utf8_terminal_gets_the_heavy_bar_on_the_focused_field() {
        let mut form = SshForm::new();
        typed(&mut form, Field::Name, "dev-vps");
        let text = render_lines(&form.lines(&unicode()));
        let focused = row_of(&form, &text, Field::Name);
        assert!(focused.contains('┃'), "{focused:?}");
        assert!(focused.contains('▸'), "{focused:?}");
        let quiet = row_of(&form, &text, Field::Host);
        assert!(quiet.contains('│'), "{quiet:?}");
        assert!(!quiet.contains('┃'), "{quiet:?}");
    }

    #[test]
    fn the_ascii_fallback_draws_nothing_a_non_utf8_terminal_cannot_print() {
        let mut form = SshForm::new();
        typed(&mut form, Field::Name, "dev-vps");
        typed(&mut form, Field::Host, "vps.ts.net");
        let text = render_lines(&form.lines(&Theme::plain()));
        for banned in ['─', '│', '┃', '●', '○', '▾', '▸', '…'] {
            assert!(
                !text.contains(banned),
                "{banned:?} survived the ASCII fallback:\n{text}"
            );
        }
        assert!(text.contains("접속 정보"), "{text}");
        assert!(text.contains("표시 이름"), "{text}");
    }

    #[test]
    fn empty_optional_fields_show_their_default_until_they_are_focused() {
        let mut form = SshForm::new();
        let text = render_lines(&form.lines(&Theme::plain()));
        assert!(row_of(&form, &text, Field::User).contains("(ssh 기본값)"));
        assert!(row_of(&form, &text, Field::Identity).contains("(ssh-agent)"));

        form.focus = Field::User;
        let text = render_lines(&form.lines(&Theme::plain()));
        let row = row_of(&form, &text, Field::User);
        assert!(!row.contains("(ssh 기본값)"), "{row:?}");
        assert!(row.contains('_'), "{row:?}");
    }
}
