//! Keyboard model (PRD §7.2, §7.11, TUI-001/004/005/008/009).
//!
//! Everything in the product is reachable by keyboard. Bindings are data, so
//! `config.toml` can override them and the help overlay and the hint bar are
//! generated from the same table instead of being written twice.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::BTreeMap;
use std::fmt;

/// The screens, in the order the work happens.
///
/// The product is two containers with many small databases and buckets
/// inside them, so [`Screen::Resources`] is the home screen and
/// [`Screen::Engines`] is the pair of containers. There is no separate
/// dashboard: a screen whose only job is to summarise the two screens next
/// to it earns its keystroke from neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Screen {
    /// The databases and buckets the user creates. Home.
    Resources,
    /// The PostgreSQL and MinIO containers they live in.
    Engines,
    Backups,
    Activity,
    Targets,
    Tunnels,
    /// Runtime diagnostics and recoverable configuration warnings.
    Doctor,
}

impl Screen {
    pub const ALL: [Screen; 7] = [
        Screen::Resources,
        Screen::Engines,
        Screen::Backups,
        Screen::Activity,
        Screen::Targets,
        Screen::Tunnels,
        Screen::Doctor,
    ];

    /// Digit that jumps to this screen (PRD §7.2).
    pub fn digit(self) -> char {
        match self {
            Screen::Resources => '1',
            Screen::Engines => '2',
            Screen::Backups => '3',
            Screen::Activity => '4',
            Screen::Targets => '5',
            Screen::Tunnels => '6',
            Screen::Doctor => '7',
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Screen::Resources => "Resources",
            Screen::Engines => "Engines",
            Screen::Backups => "Backups",
            Screen::Activity => "Log",
            Screen::Targets => "Targets",
            Screen::Tunnels => "Tunnels",
            Screen::Doctor => "Doctor",
        }
    }

    /// Screens the user reaches for every day, drawn before the rest.
    pub fn primary(self) -> bool {
        matches!(
            self,
            Screen::Resources | Screen::Engines | Screen::Backups | Screen::Activity
        )
    }

    pub fn from_digit(c: char) -> Option<Screen> {
        Screen::ALL.into_iter().find(|s| s.digit() == c)
    }

    pub fn next(self) -> Screen {
        let i = Self::ALL.iter().position(|s| *s == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Screen {
        let i = Self::ALL.iter().position(|s| *s == self).unwrap_or(0);
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

/// Every keyboard-triggerable action. Names double as the config keys and as
/// the command-palette entries, which is what keeps palette and keymap in sync
/// (PRD §7.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    Quit,
    Help,
    Palette,
    Goto(Screen),
    NextScreen,
    PrevScreen,
    FocusNext,
    FocusPrev,
    Down,
    Up,
    Top,
    Bottom,
    Open,
    Filter,
    Refresh,
    Add,
    Delete,
    Cancel,
    Submit,
    Test,
    RevealSecret,
    TunnelToggle,

    // -- one per CLI subcommand (PRD §7.10) ---------------------------------
    Doctor,
    Discover,
    Reset,
    TargetAddLocal,
    TargetAddSsh,
    TargetSshConfig,
    TargetList,
    TargetTest,
    TargetVerify,
    TargetForget,
    EngineEnsure,
    EngineList,
    EngineStart,
    EngineStop,
    EngineRestart,
    Logs,
    EngineRemove,
    NewDatabase,
    DbList,
    DbUrl,
    DbEnv,
    Copy,
    CopyExpanded,
    DbTest,
    DbDrop,
    DbForget,
    RotatePassword,
    Duplicate,
    BucketCreate,
    BucketList,
    BucketUrl,
    BucketEndpoint,
    BucketEnv,
    BucketCopyUrl,
    BucketCopyEnv,
    BucketTest,
    BucketDrop,
    BucketForget,
    BucketRotateKey,
    TunnelStart,
    TunnelStop,
    TunnelRestart,
    TunnelStartAll,
    TunnelStatus,
    Backup,
    BackupList,
    Restore,
    BackupVerify,
}

impl Action {
    /// Every action, so the command palette can offer the ones that have no
    /// key of their own (PRD §7.10: palette ⊇ CLI).
    pub fn all() -> Vec<Action> {
        let mut out: Vec<Action> = Screen::ALL.into_iter().map(Action::Goto).collect();
        out.extend([
            Action::Quit,
            Action::Help,
            Action::Palette,
            Action::NextScreen,
            Action::PrevScreen,
            Action::FocusNext,
            Action::FocusPrev,
            Action::Down,
            Action::Up,
            Action::Top,
            Action::Bottom,
            Action::Open,
            Action::Filter,
            Action::Refresh,
            Action::Add,
            Action::Delete,
            Action::Cancel,
            Action::Submit,
            Action::Test,
            Action::RevealSecret,
            Action::TunnelToggle,
            Action::Doctor,
            Action::Discover,
            Action::Reset,
            Action::TargetAddLocal,
            Action::TargetAddSsh,
            Action::TargetSshConfig,
            Action::TargetList,
            Action::TargetTest,
            Action::TargetVerify,
            Action::TargetForget,
            Action::EngineEnsure,
            Action::EngineList,
            Action::EngineStart,
            Action::EngineStop,
            Action::EngineRestart,
            Action::Logs,
            Action::EngineRemove,
            Action::NewDatabase,
            Action::DbList,
            Action::DbUrl,
            Action::DbEnv,
            Action::Copy,
            Action::CopyExpanded,
            Action::DbTest,
            Action::DbDrop,
            Action::DbForget,
            Action::RotatePassword,
            Action::Duplicate,
            Action::BucketCreate,
            Action::BucketList,
            Action::BucketUrl,
            Action::BucketEndpoint,
            Action::BucketEnv,
            Action::BucketCopyUrl,
            Action::BucketCopyEnv,
            Action::BucketTest,
            Action::BucketDrop,
            Action::BucketForget,
            Action::BucketRotateKey,
            Action::TunnelStart,
            Action::TunnelStop,
            Action::TunnelRestart,
            Action::TunnelStartAll,
            Action::TunnelStatus,
            Action::Backup,
            Action::BackupList,
            Action::Restore,
            Action::BackupVerify,
        ]);
        out
    }

    /// Stable identifier used in `config.toml` and by the command palette.
    /// For anything the CLI can do, this *is* the CLI subcommand path.
    pub fn name(self) -> String {
        match self {
            Action::Quit => "quit".into(),
            Action::Help => "help".into(),
            Action::Palette => "palette".into(),
            Action::Goto(s) => format!("goto.{}", s.title().to_ascii_lowercase()),
            Action::NextScreen => "screen.next".into(),
            Action::PrevScreen => "screen.prev".into(),
            Action::FocusNext => "focus.next".into(),
            Action::FocusPrev => "focus.prev".into(),
            Action::Down => "cursor.down".into(),
            Action::Up => "cursor.up".into(),
            Action::Top => "cursor.top".into(),
            Action::Bottom => "cursor.bottom".into(),
            Action::Open => "open".into(),
            Action::Filter => "filter".into(),
            Action::Refresh => "refresh".into(),
            Action::Add => "add".into(),
            Action::Delete => "delete".into(),
            Action::Cancel => "cancel".into(),
            Action::Submit => "submit".into(),
            Action::Test => "test".into(),
            Action::RevealSecret => "secret.reveal".into(),
            Action::TunnelToggle => "tunnel.toggle".into(),
            Action::Doctor => "doctor".into(),
            Action::Discover => "discover".into(),
            Action::Reset => "reset".into(),
            Action::TargetAddLocal => "target.add-local".into(),
            Action::TargetAddSsh => "target.add-ssh".into(),
            Action::TargetSshConfig => "target.ssh-config".into(),
            Action::TargetList => "target.list".into(),
            Action::TargetTest => "target.test".into(),
            Action::TargetVerify => "target.verify".into(),
            Action::TargetForget => "target.forget".into(),
            Action::EngineEnsure => "engine.ensure".into(),
            Action::EngineList => "engine.list".into(),
            Action::EngineStart => "engine.start".into(),
            Action::EngineStop => "engine.stop".into(),
            Action::EngineRestart => "engine.restart".into(),
            Action::Logs => "engine.logs".into(),
            Action::EngineRemove => "engine.rm".into(),
            Action::NewDatabase => "db.create".into(),
            Action::DbList => "db.list".into(),
            Action::DbUrl => "db.url".into(),
            Action::DbEnv => "db.env".into(),
            Action::Copy => "db.copy-url".into(),
            Action::CopyExpanded => "db.copy-env".into(),
            Action::DbTest => "db.test".into(),
            Action::DbDrop => "db.drop".into(),
            Action::DbForget => "db.forget".into(),
            Action::RotatePassword => "db.rotate-password".into(),
            Action::Duplicate => "db.duplicate".into(),
            Action::BucketCreate => "bucket.create".into(),
            Action::BucketList => "bucket.list".into(),
            Action::BucketUrl => "bucket.url".into(),
            Action::BucketEndpoint => "bucket.endpoint".into(),
            Action::BucketEnv => "bucket.env".into(),
            Action::BucketCopyUrl => "bucket.copy-url".into(),
            Action::BucketCopyEnv => "bucket.copy-env".into(),
            Action::BucketTest => "bucket.test".into(),
            Action::BucketDrop => "bucket.drop".into(),
            Action::BucketForget => "bucket.forget".into(),
            Action::BucketRotateKey => "bucket.rotate-key".into(),
            Action::TunnelStart => "tunnel.start".into(),
            Action::TunnelStop => "tunnel.stop".into(),
            Action::TunnelRestart => "tunnel.restart".into(),
            Action::TunnelStartAll => "tunnel.start-all".into(),
            Action::TunnelStatus => "tunnel.status".into(),
            Action::Backup => "backup.run".into(),
            Action::BackupList => "backup.list".into(),
            Action::Restore => "backup.restore".into(),
            Action::BackupVerify => "backup.verify".into(),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Action::Quit => "종료",
            Action::Help => "도움말",
            Action::Palette => "커맨드 팔레트",
            Action::Goto(_) => "화면 전환",
            Action::NextScreen => "다음 화면",
            Action::PrevScreen => "이전 화면",
            Action::FocusNext => "다음 패널",
            Action::FocusPrev => "이전 패널",
            Action::Down => "아래",
            Action::Up => "위",
            Action::Top => "처음",
            Action::Bottom => "끝",
            Action::Open => "상세 열기",
            Action::Filter => "필터",
            Action::Refresh => "새로 고침",
            Action::Add => "추가",
            Action::Delete => "삭제",
            Action::Cancel => "취소",
            Action::Submit => "실행",
            Action::Test => "접속 테스트",
            Action::RevealSecret => "비밀번호 표시",
            Action::TunnelToggle => "터널",
            Action::Doctor => "환경 진단",
            Action::Discover => "미관리 컨테이너 탐색",
            Action::Reset => "전체 초기화",
            Action::TargetAddLocal => "이 컴퓨터 등록",
            Action::TargetAddSsh => "SSH Target 등록",
            Action::TargetSshConfig => "ssh config 호스트 목록",
            Action::TargetList => "Target 목록",
            Action::TargetTest => "Target 점검",
            Action::TargetVerify => "호스트 키 지문 확인",
            Action::TargetForget => "Target 등록 해제",
            Action::EngineEnsure => "엔진 생성/재사용",
            Action::EngineList => "엔진 목록",
            Action::EngineStart => "엔진 시작",
            Action::EngineStop => "엔진 중지",
            Action::EngineRestart => "엔진 재시작",
            Action::Logs => "엔진 로그",
            Action::EngineRemove => "엔진 삭제",
            Action::NewDatabase => "새 DB",
            Action::DbList => "DB 목록",
            Action::DbUrl => "DB 접속 URL",
            Action::DbEnv => "DB env 블록",
            Action::Copy => "URL 복사",
            Action::CopyExpanded => "env 복사",
            Action::DbTest => "DB 접속 테스트",
            Action::DbDrop => "DB 삭제",
            Action::DbForget => "DB 등록 해제",
            Action::RotatePassword => "비밀번호 교체",
            Action::Duplicate => "복제",
            Action::BucketCreate => "새 버킷",
            Action::BucketList => "버킷 목록",
            Action::BucketUrl => "버킷 접속 문자열",
            Action::BucketEndpoint => "버킷 엔드포인트",
            Action::BucketEnv => "버킷 env 블록",
            Action::BucketCopyUrl => "버킷 URL 복사",
            Action::BucketCopyEnv => "버킷 env 복사",
            Action::BucketTest => "버킷 접근 테스트",
            Action::BucketDrop => "버킷 삭제",
            Action::BucketForget => "버킷 등록 해제",
            Action::BucketRotateKey => "액세스 키 교체",
            Action::TunnelStart => "터널 시작",
            Action::TunnelStop => "터널 중지",
            Action::TunnelRestart => "터널 재연결",
            Action::TunnelStartAll => "모든 터널 시작",
            Action::TunnelStatus => "터널 상태",
            Action::Backup => "백업",
            Action::BackupList => "백업 목록",
            Action::Restore => "복원",
            Action::BackupVerify => "백업 검증",
        }
    }
}

/// A key plus modifiers, comparable and printable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chord {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
}

impl Chord {
    pub fn plain(code: KeyCode) -> Self {
        Self {
            code,
            ctrl: false,
            alt: false,
        }
    }

    pub fn ctrl(c: char) -> Self {
        Self {
            code: KeyCode::Char(c),
            ctrl: true,
            alt: false,
        }
    }

    pub fn key(c: char) -> Self {
        Self::plain(KeyCode::Char(c))
    }

    /// Normalise a crossterm event. Shift is folded into the character itself,
    /// so `Y` and `shift+y` are the same chord.
    pub fn from_event(ev: KeyEvent) -> Self {
        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        let alt = ev.modifiers.contains(KeyModifiers::ALT);
        let code = match ev.code {
            // Ctrl chords arrive in either case depending on the terminal.
            KeyCode::Char(c) if ctrl => KeyCode::Char(c.to_ascii_lowercase()),
            other => other,
        };
        Self { code, ctrl, alt }
    }

    pub fn parse(text: &str) -> Option<Self> {
        let mut ctrl = false;
        let mut alt = false;
        let mut rest = text.trim();
        loop {
            let lower = rest.to_ascii_lowercase();
            if let Some(r) = lower.strip_prefix("ctrl+") {
                ctrl = true;
                rest = &rest[rest.len() - r.len()..];
            } else if let Some(r) = lower.strip_prefix("alt+") {
                alt = true;
                rest = &rest[rest.len() - r.len()..];
            } else {
                break;
            }
        }
        let code = match rest.to_ascii_lowercase().as_str() {
            "esc" | "escape" => KeyCode::Esc,
            "enter" | "return" => KeyCode::Enter,
            "tab" => KeyCode::Tab,
            "backtab" | "shift+tab" => KeyCode::BackTab,
            "space" => KeyCode::Char(' '),
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "pageup" => KeyCode::PageUp,
            "pagedown" => KeyCode::PageDown,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "backspace" => KeyCode::Backspace,
            "delete" | "del" => KeyCode::Delete,
            _ => {
                let mut chars = rest.chars();
                let c = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                // A bare letter keeps its case: `y` and `Y` are distinct.
                KeyCode::Char(if ctrl { c.to_ascii_lowercase() } else { c })
            }
        };
        Some(Self { code, ctrl, alt })
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            write!(f, "Ctrl+")?;
        }
        if self.alt {
            write!(f, "Alt+")?;
        }
        match self.code {
            KeyCode::Char(' ') => write!(f, "Space"),
            KeyCode::Char(c) if self.ctrl => write!(f, "{}", c.to_ascii_uppercase()),
            KeyCode::Char(c) => write!(f, "{c}"),
            KeyCode::Esc => write!(f, "Esc"),
            KeyCode::Enter => write!(f, "Enter"),
            KeyCode::Tab => write!(f, "Tab"),
            KeyCode::BackTab => write!(f, "Shift+Tab"),
            KeyCode::Up => write!(f, "↑"),
            KeyCode::Down => write!(f, "↓"),
            KeyCode::Left => write!(f, "←"),
            KeyCode::Right => write!(f, "→"),
            KeyCode::Home => write!(f, "Home"),
            KeyCode::End => write!(f, "End"),
            KeyCode::PageUp => write!(f, "PgUp"),
            KeyCode::PageDown => write!(f, "PgDn"),
            KeyCode::Backspace => write!(f, "Backspace"),
            KeyCode::Delete => write!(f, "Del"),
            other => write!(f, "{other:?}"),
        }
    }
}

/// Where a key press is interpreted. Modals capture keys so a stray `x` can
/// never delete something while a form is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    Global,
    List,
    Modal,
}

pub struct Keymap {
    bindings: Vec<(Chord, Action)>,
    /// Reverse lookup for the hint bar and the help overlay.
    by_action: BTreeMap<String, Chord>,
}

impl Keymap {
    /// Built-in bindings from PRD §7.11, with vim keys and arrows both live.
    pub fn defaults() -> Self {
        let mut bindings: Vec<(Chord, Action)> = vec![
            (Chord::key('q'), Action::Quit),
            (Chord::ctrl('c'), Action::Quit),
            (Chord::key('?'), Action::Help),
            (Chord::key(':'), Action::Palette),
            (Chord::plain(KeyCode::Tab), Action::FocusNext),
            (Chord::plain(KeyCode::BackTab), Action::FocusPrev),
            (Chord::plain(KeyCode::Right), Action::NextScreen),
            (Chord::plain(KeyCode::Left), Action::PrevScreen),
            (Chord::key('j'), Action::Down),
            (Chord::plain(KeyCode::Down), Action::Down),
            (Chord::key('k'), Action::Up),
            (Chord::plain(KeyCode::Up), Action::Up),
            (Chord::key('g'), Action::Top),
            (Chord::key('G'), Action::Bottom),
            (Chord::plain(KeyCode::Enter), Action::Open),
            (Chord::key('/'), Action::Filter),
            (Chord::key('r'), Action::Refresh),
            (Chord::key('n'), Action::NewDatabase),
            (Chord::key('a'), Action::Add),
            (Chord::key('y'), Action::Copy),
            (Chord::key('Y'), Action::CopyExpanded),
            (Chord::key('x'), Action::Delete),
            (Chord::plain(KeyCode::Esc), Action::Cancel),
            (Chord::ctrl('s'), Action::Submit),
            (Chord::ctrl('t'), Action::Test),
            (Chord::key('s'), Action::RevealSecret),
            (Chord::key('t'), Action::TunnelToggle),
            (Chord::key('c'), Action::Test),
            (Chord::key('b'), Action::Backup),
            (Chord::key('R'), Action::Restore),
            (Chord::key('p'), Action::RotatePassword),
            (Chord::key('d'), Action::Duplicate),
            (Chord::key('l'), Action::Logs),
            // The engines screen needs its two verbs on keys, or it is a
            // screen you can only drive from the palette.
            (Chord::key('e'), Action::EngineEnsure),
            (Chord::ctrl('r'), Action::EngineRestart),
        ];
        for screen in Screen::ALL {
            bindings.push((Chord::key(screen.digit()), Action::Goto(screen)));
        }
        // The first binding wins as the canonical key, so the hint bar shows
        // `q` rather than the `Ctrl+C` alias.
        let mut by_action: BTreeMap<String, Chord> = BTreeMap::new();
        for (chord, action) in &bindings {
            by_action.entry(action.name()).or_insert(*chord);
        }
        Self {
            bindings,
            by_action,
        }
    }

    /// Apply `[keymap]` overrides. An unknown action name or an unparseable key
    /// is reported rather than silently dropped (TUI-009).
    pub fn with_overrides(mut self, overrides: &BTreeMap<String, String>) -> (Self, Vec<String>) {
        let mut problems = Vec::new();
        for (action_name, key) in overrides {
            let Some(action) = self.action_by_name(action_name) else {
                problems.push(format!("알 수 없는 동작 이름: `{action_name}`"));
                continue;
            };
            let Some(chord) = Chord::parse(key) else {
                problems.push(format!(
                    "`{action_name}`의 키 `{key}`를 해석할 수 없습니다."
                ));
                continue;
            };
            self.bindings.retain(|(_, a)| *a != action);
            self.bindings.retain(|(c, _)| *c != chord);
            self.bindings.push((chord, action));
            self.by_action.insert(action.name(), chord);
        }
        (self, problems)
    }

    fn action_by_name(&self, name: &str) -> Option<Action> {
        Action::all().into_iter().find(|a| a.name() == name)
    }

    /// Resolve a key press. `Context::Modal` only exposes navigation and the
    /// modal's own submit/cancel, so single-letter destructive keys are inert
    /// while a form or confirmation is open (PRD §7.9).
    pub fn resolve(&self, context: Context, ev: KeyEvent) -> Option<Action> {
        let chord = Chord::from_event(ev);
        let action = self
            .bindings
            .iter()
            .find(|(c, _)| *c == chord)
            .map(|(_, a)| *a)?;
        match context {
            Context::Global | Context::List => Some(action),
            Context::Modal => matches!(
                action,
                Action::Cancel
                    | Action::Submit
                    | Action::Test
                    | Action::Help
                    | Action::FocusNext
                    | Action::FocusPrev
                    | Action::Up
                    | Action::Down
                    | Action::Copy
                    | Action::CopyExpanded
            )
            .then_some(action),
        }
    }

    pub fn chord_for(&self, action: Action) -> Option<Chord> {
        self.by_action.get(&action.name()).copied()
    }

    /// `key label` pairs for the bottom hint bar. Only the actions valid in the
    /// current focus are listed (TUI-005); the primary action comes first
    /// (§14.2).
    pub fn hints(&self, actions: &[Action]) -> Vec<(String, &'static str)> {
        actions
            .iter()
            .filter_map(|a| Some((self.chord_for(*a)?.to_string(), a.label())))
            .collect()
    }

    /// Full keymap for the `?` overlay (TUI-008).
    pub fn help_rows(&self) -> Vec<(String, String)> {
        let mut rows: Vec<(String, String)> = self
            .bindings
            .iter()
            .map(|(chord, action)| {
                let label = match action {
                    Action::Goto(s) => format!("{} 화면", s.title()),
                    other => other.label().to_string(),
                };
                (chord.to_string(), label)
            })
            .collect();
        rows.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
        rows.dedup();
        rows
    }

    /// Every action the command palette can run, as `name -> action`
    /// (PRD §7.10: palette names match CLI subcommands 1:1). Actions with no
    /// key of their own are listed too — the palette is how they are reached.
    pub fn palette_entries(&self) -> Vec<(String, Action)> {
        let mut entries: Vec<(String, Action)> =
            Action::all().into_iter().map(|a| (a.name(), a)).collect();
        entries.sort();
        entries.dedup();
        entries
    }
}

impl Default for Keymap {
    fn default() -> Self {
        Self::defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn digits_one_to_seven_reach_every_screen() {
        let km = Keymap::defaults();
        for screen in Screen::ALL {
            assert_eq!(
                km.resolve(Context::Global, press(screen.digit())),
                Some(Action::Goto(screen)),
                "{screen:?}"
            );
        }
        assert_eq!(Screen::from_digit('1'), Some(Screen::Resources));
        assert_eq!(Screen::from_digit('2'), Some(Screen::Engines));
        assert_eq!(Screen::from_digit('9'), None);
    }

    #[test]
    fn left_and_right_cycle_screens() {
        let km = Keymap::defaults();
        assert_eq!(
            km.resolve(
                Context::List,
                KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)
            ),
            Some(Action::NextScreen)
        );
        assert_eq!(
            km.resolve(
                Context::List,
                KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)
            ),
            Some(Action::PrevScreen)
        );
        assert_eq!(Screen::Resources.next(), Screen::Engines);
        assert_eq!(Screen::Doctor.next(), Screen::Resources);
        assert_eq!(Screen::Resources.prev(), Screen::Doctor);
    }

    #[test]
    fn vim_keys_and_arrows_are_both_bound() {
        let km = Keymap::defaults();
        assert_eq!(km.resolve(Context::List, press('j')), Some(Action::Down));
        assert_eq!(
            km.resolve(
                Context::List,
                KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)
            ),
            Some(Action::Down)
        );
        assert_eq!(km.resolve(Context::List, press('k')), Some(Action::Up));
        assert_eq!(
            km.resolve(
                Context::List,
                KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)
            ),
            Some(Action::Up)
        );
    }

    #[test]
    fn case_distinguishes_copy_from_expanded_copy() {
        let km = Keymap::defaults();
        assert_eq!(km.resolve(Context::List, press('y')), Some(Action::Copy));
        assert_eq!(
            km.resolve(Context::List, press('Y')),
            Some(Action::CopyExpanded)
        );
    }

    #[test]
    fn modal_context_swallows_destructive_single_letter_keys() {
        let km = Keymap::defaults();
        assert_eq!(km.resolve(Context::List, press('x')), Some(Action::Delete));
        assert_eq!(km.resolve(Context::Modal, press('x')), None);
        assert_eq!(km.resolve(Context::Modal, press('n')), None);
        assert_eq!(km.resolve(Context::Modal, press('y')), Some(Action::Copy));
        assert_eq!(
            km.resolve(Context::Modal, press('Y')),
            Some(Action::CopyExpanded)
        );
        assert_eq!(
            km.resolve(
                Context::Modal,
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
            ),
            Some(Action::Cancel)
        );
        assert_eq!(
            km.resolve(
                Context::Modal,
                KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)
            ),
            Some(Action::Submit)
        );
    }

    #[test]
    fn enter_alone_never_triggers_a_destructive_action() {
        let km = Keymap::defaults();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(km.resolve(Context::List, enter), Some(Action::Open));
        assert_eq!(km.resolve(Context::Modal, enter), None);
    }

    #[test]
    fn chords_round_trip_through_parse_and_display() {
        for text in ["q", "Y", "Ctrl+S", "Esc", "Tab", "↑"] {
            if let Some(chord) = Chord::parse(text) {
                let shown = chord.to_string();
                assert_eq!(
                    Chord::parse(&shown),
                    Some(chord),
                    "{text} rendered as {shown}"
                );
            }
        }
        assert_eq!(Chord::parse("ctrl+s"), Some(Chord::ctrl('s')));
        assert_eq!(Chord::parse("CTRL+S"), Some(Chord::ctrl('s')));
        assert_eq!(Chord::parse("nope!"), None);
    }

    #[test]
    fn overrides_replace_a_binding_and_report_bad_entries() {
        let overrides = BTreeMap::from([
            ("quit".to_string(), "ctrl+q".to_string()),
            ("nonexistent".to_string(), "z".to_string()),
            ("refresh".to_string(), "not a key".to_string()),
        ]);
        let (km, problems) = Keymap::defaults().with_overrides(&overrides);

        assert_eq!(
            km.resolve(
                Context::Global,
                KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)
            ),
            Some(Action::Quit)
        );
        assert_eq!(
            km.resolve(Context::Global, press('q')),
            None,
            "old key freed"
        );
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("nonexistent")));
        assert!(problems.iter().any(|p| p.contains("not a key")));
    }

    #[test]
    fn hints_render_key_and_label_in_the_requested_order() {
        let km = Keymap::defaults();
        let hints = km.hints(&[Action::NewDatabase, Action::TunnelToggle, Action::Quit]);
        assert_eq!(
            hints,
            vec![
                ("n".to_string(), "새 DB"),
                ("t".to_string(), "터널"),
                ("q".to_string(), "종료"),
            ]
        );
    }

    #[test]
    fn the_palette_lists_every_action_bound_or_not() {
        let km = Keymap::defaults();
        let names: Vec<String> = km.palette_entries().into_iter().map(|(n, _)| n).collect();
        for expected in [
            "db.create",
            "db.copy-url",
            "tunnel.toggle",
            "backup.run",
            "quit",
            // No key of their own: the palette is how these are reached.
            "engine.rm",
            "bucket.rotate-key",
            "target.add-ssh",
            "reset",
        ] {
            assert!(names.contains(&expected.to_string()), "missing {expected}");
        }
        // Help lists the *bound* keys, so it is a subset of the palette.
        assert!(km.help_rows().len() <= names.len());
        assert!(km.help_rows().len() >= 20);
    }
}
