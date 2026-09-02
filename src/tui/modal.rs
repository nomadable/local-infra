//! Overlays: the command palette, the help sheet, result boxes and — the one
//! that matters — destructive confirmation (PRD §7.9, §7.10, TUI-008).
//!
//! The safety property lives in [`Confirm::armed`]: until the typed text
//! equals the resource's own name, nothing can execute. `Enter` is not wired
//! to confirmation at all, in any modal.

use crate::core::model::{BackupRecord, EngineInstance, Target, TunnelSession};
use crate::core::plan::Plan;
use crate::tui::data::Resource;
use crate::tui::form::Form;
use crate::tui::keymap::{Action, Keymap};
use crate::tui::rows::plan_lines;
use crate::tui::theme::Theme;
use ratatui::text::{Line, Span};

/// What a confirmed modal will actually do. Carrying the payload means the
/// action cannot drift between the plan the user read and the call that runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Remove the database/bucket and its dedicated account (PRD §7.9 row 2).
    DropResource(Resource),
    /// Unregister only; the server keeps everything (DB-007).
    ForgetResource(Resource),
    ForgetTarget(Target),
    /// Remove the shared container, optionally with its volume (rows 3 and 4).
    RemoveEngine {
        engine: EngineInstance,
        volume: bool,
    },
    StopTunnel(TunnelSession),
    RestoreBackup {
        record: BackupRecord,
        resource: Resource,
        overwrite: bool,
    },
    /// Register an SSH target with the fingerprint the user just approved
    /// (TAR-005: no path stores a target without explicit approval).
    AddSshTarget {
        spec: crate::core::target::SshSpec,
        fingerprint: String,
    },
    /// Destroy every managed engine and wipe registration (`linf reset`).
    Reset,
    /// Quit while work is in flight (requirement 10).
    Quit,
}

impl Intent {
    /// The word that must be typed out, when the blast radius warrants it.
    /// Databases, buckets and volumes do (PRD §7.9); reversible bookkeeping
    /// does not.
    pub fn required_name(&self) -> Option<String> {
        match self {
            Intent::DropResource(r) => Some(r.name().to_string()),
            Intent::RemoveEngine { engine, volume } => Some(if *volume {
                engine.volume_name.clone()
            } else {
                engine.container_name.clone()
            }),
            Intent::Reset => Some("reset".to_string()),
            Intent::ForgetResource(_)
            | Intent::ForgetTarget(_)
            | Intent::StopTunnel(_)
            | Intent::RestoreBackup { .. }
            | Intent::AddSshTarget { .. }
            | Intent::Quit => None,
        }
    }

    pub fn title(&self) -> String {
        match self {
            Intent::DropResource(r) => match r.kind() {
                crate::core::model::ResourceKind::Database => "DB 삭제".to_string(),
                crate::core::model::ResourceKind::Bucket => "버킷 삭제".to_string(),
            },
            Intent::ForgetResource(_) => "앱 등록 해제".to_string(),
            Intent::ForgetTarget(_) => "Target 등록 해제".to_string(),
            Intent::RemoveEngine { volume: true, .. } => "볼륨 삭제".to_string(),
            Intent::RemoveEngine { volume: false, .. } => "엔진 컨테이너 삭제".to_string(),
            Intent::StopTunnel(_) => "터널 중지".to_string(),
            Intent::RestoreBackup { .. } => "백업 복원".to_string(),
            Intent::AddSshTarget { .. } => "SSH 호스트 키 확인".to_string(),
            Intent::Reset => "전체 초기화".to_string(),
            Intent::Quit => "종료".to_string(),
        }
    }

    /// The label on the affirmative button.
    pub fn verb(&self) -> &'static str {
        match self {
            Intent::DropResource(_) | Intent::RemoveEngine { .. } | Intent::Reset => "삭제",
            Intent::ForgetResource(_) | Intent::ForgetTarget(_) => "등록 해제",
            Intent::StopTunnel(_) => "중지",
            Intent::RestoreBackup { .. } => "복원",
            Intent::AddSshTarget { .. } => "승인하고 저장",
            Intent::Quit => "종료",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmFocus {
    Cancel,
    Confirm,
}

/// A plan, its warnings, and a typed-name gate (PRD §7.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirm {
    pub plan: Plan,
    pub intent: Intent,
    pub required_name: Option<String>,
    pub typed: String,
    /// Defaults to cancel: the destructive button is never one keypress away.
    pub focus: ConfirmFocus,
}

impl Confirm {
    pub fn new(plan: Plan, intent: Intent) -> Self {
        let required_name = intent.required_name();
        Self {
            plan,
            intent,
            required_name,
            typed: String::new(),
            focus: ConfirmFocus::Cancel,
        }
    }

    pub fn needs_typing(&self) -> bool {
        self.required_name.is_some()
    }

    /// True only when the gate is satisfied. Everything destructive checks
    /// this, and nothing else unlocks it.
    pub fn armed(&self) -> bool {
        match &self.required_name {
            None => true,
            Some(expected) => &self.typed == expected,
        }
    }

    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            ConfirmFocus::Cancel => ConfirmFocus::Confirm,
            ConfirmFocus::Confirm => ConfirmFocus::Cancel,
        };
    }

    pub fn type_char(&mut self, c: char) -> bool {
        if !self.needs_typing() || c.is_control() {
            return false;
        }
        self.typed.push(c);
        true
    }

    pub fn backspace(&mut self) -> bool {
        self.needs_typing() && self.typed.pop().is_some()
    }

    pub fn title(&self) -> String {
        self.intent.title()
    }

    pub fn lines(&self, theme: &Theme) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if self.plan.is_destructive() {
            lines.push(Line::from(Span::styled(
                "! 이 작업은 되돌릴 수 없습니다.".to_string(),
                theme.danger(),
            )));
            lines.push(Line::raw(String::new()));
        }
        lines.extend(plan_lines(&self.plan, theme));

        if let Some(expected) = &self.required_name {
            lines.push(Line::raw(String::new()));
            lines.push(Line::from(Span::styled(
                format!("삭제하려면 이름을 입력하세요: {expected}"),
                theme.normal(),
            )));
            lines.push(Line::from(vec![
                Span::styled("> ".to_string(), theme.accent()),
                Span::styled(
                    format!(
                        "[ {} ]",
                        crate::tui::rows::pad(&format!("{}_", self.typed), 38)
                    ),
                    theme.selected(),
                ),
                Span::styled(
                    if self.armed() {
                        "  ✓ 일치".to_string()
                    } else {
                        "  대기 중".to_string()
                    },
                    if self.armed() {
                        theme.ok()
                    } else {
                        theme.muted()
                    },
                ),
            ]));
        }

        lines.push(Line::raw(String::new()));
        let confirm_style = if self.focus == ConfirmFocus::Confirm {
            if self.armed() {
                theme.danger().patch(theme.selected())
            } else {
                theme.selected().patch(theme.muted())
            }
        } else if self.armed() {
            theme.danger()
        } else {
            theme.muted()
        };
        lines.push(Line::from(vec![
            Span::styled(
                "  [ 취소 ]  ".to_string(),
                if self.focus == ConfirmFocus::Cancel {
                    theme.selected()
                } else {
                    theme.normal()
                },
            ),
            Span::styled(format!("[ {} ]", self.intent.verb()), confirm_style),
        ]));
        lines
    }
}

/// `:` palette — every bound action by name, filtered as you type (PRD §7.10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Palette {
    pub query: String,
    pub entries: Vec<(String, Action)>,
    pub selected: usize,
}

impl Palette {
    pub fn new(keymap: &Keymap) -> Self {
        Self {
            query: String::new(),
            entries: keymap.palette_entries(),
            selected: 0,
        }
    }

    pub fn type_char(&mut self, c: char) -> bool {
        if c.is_control() {
            return false;
        }
        self.query.push(c);
        self.selected = 0;
        true
    }

    pub fn backspace(&mut self) -> bool {
        let popped = self.query.pop().is_some();
        if popped {
            self.selected = 0;
        }
        popped
    }

    pub fn matches(&self) -> Vec<usize> {
        filter(&self.entries, &self.query)
    }

    pub fn move_by(&mut self, delta: isize) {
        let count = self.matches().len();
        if count == 0 {
            self.selected = 0;
            return;
        }
        let next = self.selected as isize + delta;
        self.selected = next.rem_euclid(count as isize) as usize;
    }

    pub fn selection(&self) -> Option<Action> {
        let matches = self.matches();
        matches
            .get(self.selected)
            .and_then(|i| self.entries.get(*i))
            .map(|(_, action)| *action)
    }

    pub fn lines(&self, keymap: &Keymap, theme: &Theme) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(vec![
            Span::styled(": ".to_string(), theme.accent()),
            Span::styled(format!("{}_", self.query), theme.normal()),
        ])];
        let matches = self.matches();
        if matches.is_empty() {
            lines.push(Line::from(Span::styled(
                "  일치하는 명령이 없습니다.".to_string(),
                theme.muted(),
            )));
        }
        for (row, index) in matches.iter().enumerate() {
            let (name, action) = &self.entries[*index];
            let key = keymap
                .chord_for(*action)
                .map(|c| c.to_string())
                .unwrap_or_default();
            let style = if row == self.selected {
                theme.selected()
            } else {
                theme.normal()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {name:<24}"), style),
                Span::styled(crate::tui::rows::pad(action.label(), 20), theme.muted()),
                Span::styled(key, theme.key()),
            ]));
        }
        lines
    }
}

/// Case-insensitive substring match against the command name *and* its label,
/// so `터널` finds `tunnel.toggle` too. Pure, so the filter is testable.
pub fn filter(entries: &[(String, Action)], query: &str) -> Vec<usize> {
    let needle = query.trim().to_ascii_lowercase();
    entries
        .iter()
        .enumerate()
        .filter(|(_, (name, action))| {
            needle.is_empty()
                || name.to_ascii_lowercase().contains(&needle)
                || action.label().to_ascii_lowercase().contains(&needle)
        })
        .map(|(i, _)| i)
        .collect()
}

/// A result or failure box. Errors arrive as the three-part diagnostic every
/// surface renders identically (PRD §10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub title: String,
    pub body: Vec<String>,
    pub danger: bool,
    pub scroll: u16,
    copy_url: Option<String>,
    copy_env: Option<String>,
    /// Set after `y`/`Y` so the modal itself confirms the copy.
    pub status: Option<String>,
}

impl Message {
    pub fn note(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into().lines().map(str::to_string).collect(),
            danger: false,
            scroll: 0,
            copy_url: None,
            copy_env: None,
            status: None,
        }
    }

    pub fn with_copy(mut self, url: impl Into<String>, env: impl Into<String>) -> Self {
        self.copy_url = Some(url.into());
        self.copy_env = Some(env.into());
        self
    }

    pub fn can_copy(&self) -> bool {
        self.copy_url.is_some() || self.copy_env.is_some()
    }

    pub fn copy_url(&self) -> Option<&str> {
        self.copy_url.as_deref()
    }

    pub fn copy_env(&self) -> Option<&str> {
        self.copy_env.as_deref()
    }

    pub fn failure(diagnostic: &crate::core::Diagnostic) -> Self {
        let mut body = vec![
            format!("무엇     {}", diagnostic.what),
            format!("원인     {}", diagnostic.cause),
            format!("다음     {}", diagnostic.next),
        ];
        if let Some(command) = &diagnostic.command {
            body.push(String::new());
            body.push(format!("명령     {command}"));
        }
        if let Some(output) = &diagnostic.output {
            body.push(String::new());
            for line in output.lines() {
                body.push(format!("  {line}"));
            }
        }
        Self {
            title: "오류".to_string(),
            body,
            danger: true,
            scroll: 0,
            copy_url: None,
            copy_env: None,
            status: None,
        }
    }

    pub fn lines(&self, theme: &Theme) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = self
            .body
            .iter()
            .map(|line| {
                Line::from(Span::styled(
                    line.clone(),
                    if self.danger && line.starts_with("무엇") {
                        theme.danger()
                    } else {
                        theme.normal()
                    },
                ))
            })
            .collect();
        if let Some(status) = &self.status {
            lines.push(Line::raw(String::new()));
            lines.push(Line::from(Span::styled(status.clone(), theme.ok())));
        }
        lines
    }
}

/// Everything that can sit on top of a screen. Only one at a time: nested
/// modals in a terminal are how users lose track of what `Esc` cancels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    Help {
        scroll: u16,
    },
    Palette(Palette),
    Form(Box<Form>),
    /// The SSH target form (PRD §7.4).
    SshForm(Box<crate::tui::ssh_form::SshForm>),
    Confirm(Box<Confirm>),
    Message(Message),
    /// Full-screen list's inspect overlay. Replaces the old split detail pane.
    Detail {
        title: String,
        scroll: u16,
    },
}

impl Modal {
    /// Modals that accept typing must see printable keys before the keymap
    /// does, or a bucket named `query` would trip `q`.
    pub fn takes_text(&self) -> bool {
        match self {
            Modal::Palette(_) | Modal::SshForm(_) => true,
            Modal::Form(form) => form.editable(form.focus),
            Modal::Confirm(confirm) => confirm.needs_typing(),
            Modal::Help { .. } | Modal::Message(_) | Modal::Detail { .. } => false,
        }
    }

    pub fn title(&self) -> String {
        match self {
            Modal::Help { .. } => "도움말".to_string(),
            Modal::Palette(_) => "커맨드 팔레트".to_string(),
            Modal::Form(form) => form.title(),
            Modal::SshForm(form) => form.title(),
            Modal::Confirm(confirm) => confirm.title(),
            Modal::Message(message) => message.title.clone(),
            Modal::Detail { title, .. } => title.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::plan::StepKind;
    use crate::tui::data::fixture;
    use crate::tui::rows::render_lines;

    fn drop_confirm() -> Confirm {
        let local = fixture::local_target();
        let resource = fixture::database(&local, "letsbid_dev", None);
        let plan = Plan::new("삭제")
            .step(StepKind::Destroy, "DB letsbid_dev 삭제")
            .warn("백업 기록이 없습니다.");
        Confirm::new(plan, Intent::DropResource(resource))
    }

    #[test]
    fn a_delete_modal_opens_disarmed_and_focused_on_cancel() {
        let confirm = drop_confirm();
        assert_eq!(confirm.focus, ConfirmFocus::Cancel);
        assert!(confirm.needs_typing());
        assert!(!confirm.armed());
    }

    #[test]
    fn only_the_exact_name_arms_the_delete_modal() {
        let mut confirm = drop_confirm();
        for c in "letsbid_de".chars() {
            confirm.type_char(c);
        }
        assert!(!confirm.armed(), "a prefix must not arm it");

        confirm.type_char('v');
        assert!(confirm.armed());

        confirm.type_char('x');
        assert!(!confirm.armed(), "a superstring must not arm it");

        confirm.backspace();
        assert!(confirm.armed());
    }

    #[test]
    fn a_volume_delete_asks_for_the_volume_name_not_the_container_name() {
        let local = fixture::local_target();
        let engine =
            fixture::engine_instance(&local, crate::core::model::EngineKind::Postgres, 5432);
        let volume = Intent::RemoveEngine {
            engine: engine.clone(),
            volume: true,
        };
        assert_eq!(volume.required_name(), Some(engine.volume_name.clone()));
        assert_eq!(volume.title(), "볼륨 삭제");

        let container = Intent::RemoveEngine {
            engine: engine.clone(),
            volume: false,
        };
        assert_eq!(container.required_name(), Some(engine.container_name));
        assert_eq!(container.title(), "엔진 컨테이너 삭제");
    }

    #[test]
    fn reversible_bookkeeping_needs_no_typed_name() {
        let local = fixture::local_target();
        let resource = fixture::database(&local, "letsbid_dev", None);
        let confirm = Confirm::new(Plan::new("해제"), Intent::ForgetResource(resource));
        assert!(!confirm.needs_typing());
        assert!(confirm.armed());
        assert_eq!(confirm.focus, ConfirmFocus::Cancel);
    }

    #[test]
    fn the_modal_shows_the_plan_its_warnings_and_the_typed_gate() {
        let confirm = drop_confirm();
        let text = render_lines(&confirm.lines(&Theme::plain()));
        assert!(text.contains("되돌릴 수 없습니다"));
        assert!(text.contains("DB letsbid_dev 삭제"));
        assert!(text.contains("백업 기록이 없습니다"));
        assert!(text.contains("삭제하려면 이름을 입력하세요: letsbid_dev"));
        assert!(text.contains("대기 중"));
        assert!(text.contains("[ 취소 ]"));
        assert!(text.contains("[ 삭제 ]"));
    }

    #[test]
    fn the_gate_indicator_flips_once_the_name_matches() {
        let mut confirm = drop_confirm();
        for c in "letsbid_dev".chars() {
            confirm.type_char(c);
        }
        let text = render_lines(&confirm.lines(&Theme::plain()));
        assert!(text.contains("✓ 일치"));
    }

    #[test]
    fn the_palette_filters_by_command_name_and_by_label() {
        let keymap = Keymap::defaults();
        let mut palette = Palette::new(&keymap);
        let all = palette.matches().len();
        assert!(all > 10);

        for c in "tunnel".chars() {
            palette.type_char(c);
        }
        let names: Vec<&str> = palette
            .matches()
            .iter()
            .map(|i| palette.entries[*i].0.as_str())
            .collect();
        assert!(!names.is_empty());
        assert!(names.iter().all(|n| n.contains("tunnel")));
        assert!(names.len() < all);
    }

    #[test]
    fn the_palette_finds_commands_by_their_korean_label_too() {
        let keymap = Keymap::defaults();
        let mut palette = Palette::new(&keymap);
        for c in "백업 목록".chars() {
            palette.type_char(c);
        }
        assert_eq!(palette.selection(), Some(Action::BackupList));
    }

    #[test]
    fn palette_selection_wraps_and_survives_a_narrowing_query() {
        let keymap = Keymap::defaults();
        let mut palette = Palette::new(&keymap);
        palette.move_by(-1);
        assert_eq!(palette.selected, palette.matches().len() - 1);

        for c in "goto".chars() {
            palette.type_char(c);
        }
        assert_eq!(palette.selected, 0, "typing resets the cursor");
        assert!(palette.selection().is_some());
    }

    #[test]
    fn an_impossible_query_selects_nothing_instead_of_the_first_entry() {
        let keymap = Keymap::defaults();
        let mut palette = Palette::new(&keymap);
        for c in "zzzz".chars() {
            palette.type_char(c);
        }
        assert!(palette.matches().is_empty());
        assert_eq!(palette.selection(), None);
        let text = render_lines(&palette.lines(&keymap, &Theme::plain()));
        assert!(text.contains("일치하는 명령이 없습니다"));
    }

    #[test]
    fn a_failure_box_carries_all_three_diagnostic_parts() {
        let diagnostic =
            crate::core::Diagnostic::new("실패했습니다", "원인입니다", "이렇게 하세요")
                .with_command("docker ps")
                .with_output("permission denied");
        let message = Message::failure(&diagnostic);
        assert!(message.danger);
        let text = render_lines(&message.lines(&Theme::plain()));
        assert!(text.contains("실패했습니다"));
        assert!(text.contains("원인입니다"));
        assert!(text.contains("이렇게 하세요"));
        assert!(text.contains("docker ps"));
        assert!(text.contains("permission denied"));
    }

    #[test]
    fn a_copy_status_line_is_drawn_on_the_result_box() {
        let mut message = Message::note("완료", "url here").with_copy("secret-url", "ENV=1");
        assert!(message.can_copy());
        message.status = Some("URL을(를) 복사했습니다".into());
        let text = render_lines(&message.lines(&Theme::plain()));
        assert!(text.contains("URL을(를) 복사했습니다"));
    }

    #[test]
    fn only_typing_modals_claim_printable_keys() {
        let keymap = Keymap::defaults();
        assert!(Modal::Palette(Palette::new(&keymap)).takes_text());
        assert!(Modal::Confirm(Box::new(drop_confirm())).takes_text());
        assert!(!Modal::Help { scroll: 0 }.takes_text());
        assert!(!Modal::Message(Message::note("t", "b")).takes_text());

        let local = fixture::local_target();
        let resource = fixture::database(&local, "letsbid_dev", None);
        let forget = Confirm::new(Plan::new("해제"), Intent::ForgetResource(resource));
        assert!(
            !Modal::Confirm(Box::new(forget)).takes_text(),
            "a modal with no gate must not swallow keys"
        );
    }

    #[test]
    fn modal_titles_name_the_service_being_created() {
        let form = Form::new(
            vec![fixture::local_target()],
            crate::core::model::EngineKind::Minio,
        );
        assert_eq!(Modal::Form(Box::new(form)).title(), "새 리소스 · 버킷");
    }
}
