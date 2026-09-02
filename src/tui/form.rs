//! The single create form with a live execution-plan preview (PRD §7.5).
//!
//! One form serves both services. Its first field is the resource kind;
//! changing it reshapes the fields below it and the plan. PostgreSQL asks for
//! a database, a role and a locale, MinIO for a bucket, an access key and a
//! region. The field list, the derived names and the per-field validation are
//! pure functions of the form state, so all of it is testable without Docker.

use crate::core::engine::EngineSpec;
use crate::core::error::Result;
use crate::core::model::{EngineKind, Target};
use crate::core::plan::Plan;
use crate::core::{bucket, database, minio, util};
use crate::tui::chrome;
use crate::tui::rows::{pad, plan_lines};
use crate::tui::theme::Theme;
use ratatui::text::{Line, Span};

/// The glyphs a form widget is drawn from, layered on [`chrome::Glyphs`].
///
/// `chrome` draws shells and rules; a form also has to say *this field has
/// the keyboard* and *this option is the chosen one*. Colour cannot carry
/// that on its own — `NO_COLOR` terminals exist (PRD §12.4) — so every state
/// here has a shape as well.
///
/// Every glyph is one column wide in both sets, which is what lets
/// [`Form::layout_hits`] place click targets without knowing the theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Widgets {
    /// Bar around the focused field, heavier than [`Widgets::bar`] wherever
    /// the terminal can draw the difference.
    pub bar_on: &'static str,
    pub bar: &'static str,
    pub radio_on: &'static str,
    pub radio_off: &'static str,
    /// Marks a closed dropdown, so it does not read as a text box.
    pub caret: &'static str,
    /// A passed verdict, next to the field it belongs to.
    pub ok: &'static str,
    /// Left-gutter focus mark. On an ASCII terminal it is the *only* shape
    /// left, because there `bar_on` and `bar` are the same character.
    pub gutter: &'static str,
}

impl Widgets {
    pub fn of(theme: &Theme) -> Self {
        // The gutter mark comes from `chrome`, so a focused form row and a
        // selected table row point with the same character.
        let gutter = chrome::Glyphs::of(theme).cursor;
        if theme.unicode {
            Self {
                bar_on: "┃",
                bar: "│",
                radio_on: "●",
                radio_off: "○",
                caret: "▾",
                ok: "✓",
                gutter,
            }
        } else {
            Self {
                bar_on: "|",
                bar: "|",
                radio_on: "*",
                radio_off: "o",
                caret: "v",
                ok: "ok",
                gutter,
            }
        }
    }

    fn bar(self, focused: bool) -> &'static str {
        if focused {
            self.bar_on
        } else {
            self.bar
        }
    }

    fn radio(self, on: bool) -> String {
        format!("({})", if on { self.radio_on } else { self.radio_off })
    }

    /// A checkbox is not a radio button: these answers are independent of
    /// each other, and the brackets say so. Already ASCII in both sets.
    fn check(self, on: bool) -> String {
        if on { "[x]" } else { "[ ]" }.to_string()
    }

    /// Two columns whether or not the row has focus, so nothing shifts
    /// sideways as focus moves through the form.
    pub(crate) fn lead(self, focused: bool) -> String {
        if focused {
            format!("{} ", self.gutter)
        } else {
            "  ".to_string()
        }
    }
}

/// The form's own column budget, kept under the 80-column popup it sits in
/// so a row can gain an inline verdict without wrapping. A wrapped row would
/// move every row below it off the line [`Form::layout_hits`] promised.
const FORM_COLS: u16 = 74;
/// Width of the focus gutter.
const LEAD: u16 = 2;
/// Label column, wide enough for `인코딩/로케일` (13 columns) and a gap.
const LABEL: u16 = 15;
/// Mark plus trailing space: four columns in every theme, which is what
/// keeps the option geometry independent of the glyph set.
const MARK: u16 = 4;
/// Columns between two options on one row.
const GAP: u16 = 2;
/// Interior of a text box, at most.
const VALUE: u16 = 28;
/// Columns held back for `✓ 사용 가능`, so a verdict never has to wrap.
const VERDICT: u16 = 13;
/// First line the field rows occupy: the tab strip, a blank, the section
/// rule. [`Form::lines`] asserts against this rather than trusting it.
const FIELDS_LINE: u16 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// 종류 — the field that reshapes every other one.
    Kind,
    Target,
    Engine,
    Project,
    /// DB명 / 버킷명.
    Name,
    /// 사용자명 / 액세스 키.
    Principal,
    /// 비밀번호 / 시크릿 키. Always generated (PRD §11.2).
    Secret,
    /// 인코딩/로케일 / 리전.
    Options,
    Tunnel,
}

impl Field {
    /// PRD §7.5 order, top to bottom, identical for both services.
    pub const ALL: [Field; 9] = [
        Field::Kind,
        Field::Target,
        Field::Engine,
        Field::Project,
        Field::Name,
        Field::Principal,
        Field::Secret,
        Field::Options,
        Field::Tunnel,
    ];

    pub fn label(self, engine: EngineKind) -> &'static str {
        match (self, engine) {
            (Field::Kind, _) => "종류",
            (Field::Target, _) => "Target",
            (Field::Engine, _) => "엔진",
            (Field::Project, _) => "프로젝트명",
            (Field::Name, EngineKind::Postgres) => "DB명",
            (Field::Name, EngineKind::Minio) => "버킷명",
            (Field::Principal, EngineKind::Postgres) => "사용자명",
            (Field::Principal, EngineKind::Minio) => "액세스 키",
            (Field::Secret, EngineKind::Postgres) => "비밀번호",
            (Field::Secret, EngineKind::Minio) => "시크릿 키",
            (Field::Options, EngineKind::Postgres) => "인코딩/로케일",
            (Field::Options, EngineKind::Minio) => "리전",
            (Field::Tunnel, _) => "터널 자동시작",
        }
    }
}

/// Result of validating one field. `None` means "nothing to validate yet".
pub type FieldCheck = Option<Result<(), String>>;

/// Major versions offered per service. One container is shared per major, so
/// this is the only version choice the product exposes (ENG-002).
const POSTGRES_MAJORS: [&str; 3] = ["17", "16", "15"];
const MINIO_MAJORS: [&str; 1] = ["latest"];
/// `(encoding, locale)` presets. PostgreSQL locale names are not free text in
/// practice: a typo produces an `initdb` failure minutes later.
const LOCALES: [(&str, &str); 3] = [("UTF8", "C"), ("UTF8", "C.UTF-8"), ("UTF8", "en_US.UTF-8")];
const REGIONS: [&str; 4] = ["us-east-1", "us-west-2", "eu-central-1", "ap-northeast-2"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    pub targets: Vec<Target>,
    pub target_index: usize,
    pub engine: EngineKind,
    pub want_postgres: bool,
    pub want_minio: bool,
    pub major_index: usize,
    pub project: String,
    pub name: String,
    pub principal: String,
    bucket_name: String,
    /// MinIO only: the access key is generated unless the user asks to type
    /// one. PostgreSQL role names are always typed.
    pub auto_principal: bool,
    pub locale_index: usize,
    pub region_index: usize,
    pub focus: Field,
    /// Highlighted option inside the current question (checkbox / radio).
    pub option_cursor: usize,
    /// Set once the user edits by hand; derivation from the project stops then.
    name_touched: bool,
    principal_touched: bool,
    /// Generated once, so it does not churn on every keystroke.
    generated_access_key: String,
    pub plan: Option<Plan>,
    pub plan_error: Option<String>,
    plan_epoch: u64,
    plan_shown_epoch: u64,
    /// Streaming step log while `Ctrl+S` runs (PRD §7.5 last bullets).
    pub steps: Vec<String>,
    pub running: bool,
    /// True until the user has created their first resource.
    pub first_run: bool,
}

impl Form {
    pub fn new(targets: Vec<Target>, engine: EngineKind) -> Self {
        let mut form = Self {
            targets,
            target_index: 0,
            engine,
            want_postgres: engine == EngineKind::Postgres,
            want_minio: engine == EngineKind::Minio,
            major_index: 0,
            project: String::new(),
            name: String::new(),
            principal: String::new(),
            bucket_name: String::new(),
            auto_principal: true,
            locale_index: 0,
            region_index: 0,
            focus: Field::Kind,
            option_cursor: 0,
            name_touched: false,
            principal_touched: false,
            generated_access_key: minio::generate_access_key(),
            plan: None,
            plan_error: None,
            plan_epoch: 0,
            plan_shown_epoch: 0,
            steps: Vec::new(),
            running: false,
            first_run: false,
        };
        if !form.want_postgres && !form.want_minio {
            form.want_postgres = true;
        }
        form.derive_names();
        form
    }

    pub fn target(&self) -> Option<&Target> {
        self.targets.get(self.target_index)
    }

    fn majors(&self) -> &'static [&'static str] {
        match self.engine {
            EngineKind::Postgres => &POSTGRES_MAJORS,
            EngineKind::Minio => &MINIO_MAJORS,
        }
    }

    pub fn major_version(&self) -> &'static str {
        let majors = self.majors();
        majors[self.major_index.min(majors.len() - 1)]
    }

    pub fn encoding(&self) -> &'static str {
        LOCALES[self.locale_index.min(LOCALES.len() - 1)].0
    }

    pub fn locale(&self) -> &'static str {
        LOCALES[self.locale_index.min(LOCALES.len() - 1)].1
    }

    pub fn region(&self) -> &'static str {
        REGIONS[self.region_index.min(REGIONS.len() - 1)]
    }

    /// Which fields accept typing *right now*. The MinIO access key only does
    /// so once the user has switched it off auto-generation.
    pub fn editable(&self, field: Field) -> bool {
        match field {
            Field::Project | Field::Name => true,
            Field::Principal => match self.engine {
                EngineKind::Postgres => true,
                EngineKind::Minio => !self.auto_principal,
            },
            _ => false,
        }
    }

    // -- navigation ---------------------------------------------------------

    /// Fields the user can land on. Kind lives in the top tab strip; secret and
    /// tunnel are notes, not rows. A lone target/engine is not a choice.
    fn focusable(&self) -> Vec<Field> {
        let mut fields = vec![Field::Kind];
        if self.targets.len() != 1 {
            fields.push(Field::Target);
        }
        if self.majors().len() > 1 {
            fields.push(Field::Engine);
        }
        fields.extend([
            Field::Project,
            Field::Name,
            Field::Principal,
            Field::Options,
        ]);
        fields
    }

    pub fn next_field(&mut self) {
        let fields = self.focusable();
        let i = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
        self.focus = fields[(i + 1) % fields.len()];
    }

    pub fn prev_field(&mut self) {
        let fields = self.focusable();
        let i = fields.iter().position(|f| *f == self.focus).unwrap_or(0);
        self.focus = fields[(i + fields.len() - 1) % fields.len()];
    }

    /// Cycle the current radio/checkbox group.
    pub fn cycle(&mut self, forward: bool) -> bool {
        let step = |index: usize, len: usize| -> usize {
            if len == 0 {
                return 0;
            }
            if forward {
                (index + 1) % len
            } else {
                (index + len - 1) % len
            }
        };
        match self.focus {
            Field::Kind => {
                let next = if self.want_postgres && !self.want_minio {
                    EngineKind::Minio
                } else {
                    EngineKind::Postgres
                };
                self.set_engine(next);
                true
            }
            Field::Target => {
                if self.targets.len() < 2 {
                    return false;
                }
                self.target_index = step(self.target_index, self.targets.len());
                self.invalidate_plan();
                true
            }
            Field::Engine => {
                if self.majors().len() < 2 {
                    return false;
                }
                self.major_index = step(self.major_index, self.majors().len());
                self.invalidate_plan();
                true
            }
            Field::Principal if self.engine == EngineKind::Minio => {
                self.auto_principal = !self.auto_principal;
                self.principal_touched = false;
                self.derive_names();
                self.invalidate_plan();
                true
            }
            Field::Options => {
                match self.engine {
                    EngineKind::Postgres => {
                        self.locale_index = step(self.locale_index, LOCALES.len())
                    }
                    EngineKind::Minio => self.region_index = step(self.region_index, REGIONS.len()),
                }
                self.invalidate_plan();
                true
            }
            _ => false,
        }
    }

    pub fn select_index(&mut self, field: Field, index: usize) -> bool {
        self.focus = field;
        self.option_cursor = index;
        match field {
            Field::Kind => self.toggle(),
            Field::Target if index < self.targets.len() && index != self.target_index => {
                self.target_index = index;
                self.invalidate_plan();
                true
            }
            Field::Engine if index < self.majors().len() && index != self.major_index => {
                self.major_index = index;
                self.invalidate_plan();
                true
            }
            Field::Options => match self.engine {
                EngineKind::Postgres if index < LOCALES.len() && index != self.locale_index => {
                    self.locale_index = index;
                    self.invalidate_plan();
                    true
                }
                EngineKind::Minio if index < REGIONS.len() && index != self.region_index => {
                    self.region_index = index;
                    self.invalidate_plan();
                    true
                }
                _ => false,
            },
            Field::Principal if self.engine == EngineKind::Minio => {
                let auto = index == 0;
                if auto == self.auto_principal {
                    return false;
                }
                self.auto_principal = auto;
                self.principal_touched = false;
                self.derive_names();
                self.invalidate_plan();
                true
            }
            _ => false,
        }
    }

    pub fn click_field(&mut self, field: Field) -> bool {
        if self.focus == field {
            false
        } else {
            self.focus = field;
            self.option_cursor = 0;
            true
        }
    }

    pub fn toggle(&mut self) -> bool {
        if self.focus != Field::Kind {
            return self.cycle(true);
        }
        let idx = self.option_cursor.min(1);
        match idx {
            0 => {
                if self.want_postgres && !self.want_minio {
                    return false;
                }
                self.want_postgres = !self.want_postgres;
            }
            _ => {
                if self.want_minio && !self.want_postgres {
                    return false;
                }
                self.want_minio = !self.want_minio;
            }
        }
        self.engine = if self.want_postgres {
            EngineKind::Postgres
        } else {
            EngineKind::Minio
        };
        self.derive_names();
        self.invalidate_plan();
        true
    }

    pub fn move_option(&mut self, forward: bool) -> bool {
        match self.focus {
            Field::Kind => {
                self.option_cursor = if forward { 1 } else { 0 };
                false
            }
            Field::Target | Field::Engine | Field::Options => self.cycle(forward),
            _ => false,
        }
    }

    pub fn set_engine(&mut self, engine: EngineKind) {
        self.want_postgres = engine == EngineKind::Postgres;
        self.want_minio = engine == EngineKind::Minio;
        if self.engine == engine {
            return;
        }
        self.engine = engine;
        self.major_index = 0;
        self.auto_principal = true;
        self.principal_touched = false;
        self.derive_names();
        self.invalidate_plan();
    }

    pub fn type_char(&mut self, c: char) -> bool {
        if !self.editable(self.focus) || c.is_control() {
            return false;
        }
        match self.focus {
            Field::Project => {
                self.project.push(c);
                self.derive_names();
            }
            Field::Name => {
                self.name.push(c);
                self.name_touched = true;
            }
            Field::Principal => {
                self.principal.push(c);
                self.principal_touched = true;
            }
            _ => return false,
        }
        self.invalidate_plan();
        true
    }

    pub fn backspace(&mut self) -> bool {
        if !self.editable(self.focus) {
            return false;
        }
        let popped = match self.focus {
            Field::Project => {
                let popped = self.project.pop().is_some();
                if popped {
                    self.derive_names();
                }
                popped
            }
            Field::Name => {
                self.name_touched = true;
                self.name.pop().is_some()
            }
            Field::Principal => {
                self.principal_touched = true;
                self.principal.pop().is_some()
            }
            _ => false,
        };
        if popped {
            self.invalidate_plan();
        }
        popped
    }

    /// `Letsbid` → `letsbid_dev` / `letsbid_user` for PostgreSQL, and the
    /// bucket-legal `letsbid-dev` plus a generated key for MinIO.
    fn derive_names(&mut self) {
        if self.project.trim().is_empty() {
            if !self.name_touched {
                self.name.clear();
            }
            if !self.principal_touched {
                self.principal.clear();
            }
            self.bucket_name.clear();
            if self.want_minio && !self.want_postgres && self.auto_principal {
                self.principal = self.generated_access_key.clone();
            }
            return;
        }
        if self.want_postgres {
            let (db, user) = util::suggest_names(&self.project);
            if !self.name_touched {
                self.name = db;
            }
            if !self.principal_touched {
                self.principal = user;
            }
        }
        if self.want_minio {
            let spec = bucket::CreateSpec::for_project(&self.project);
            self.bucket_name = spec.bucket_name.clone();
            if !self.want_postgres {
                if !self.name_touched {
                    self.name = spec.bucket_name;
                }
                if !self.principal_touched {
                    self.principal = self.generated_access_key.clone();
                }
            }
        }
    }

    /// Marks the preview stale and returns the epoch a fresh plan must quote.
    pub fn invalidate_plan(&mut self) -> u64 {
        self.plan_epoch += 1;
        self.plan_epoch
    }

    pub fn epoch(&self) -> u64 {
        self.plan_epoch
    }

    pub fn accept_plan(&mut self, epoch: u64, plan: Result<Plan>) {
        if epoch < self.plan_shown_epoch {
            return;
        }
        self.plan_shown_epoch = epoch;
        match plan {
            Ok(plan) => {
                self.plan = Some(plan);
                self.plan_error = None;
            }
            Err(e) => {
                self.plan = None;
                self.plan_error = Some(e.as_diagnostic().what);
            }
        }
    }

    pub fn plan_stale(&self) -> bool {
        self.plan_shown_epoch != self.plan_epoch
    }

    // -- validation ---------------------------------------------------------

    /// Per-field verdicts in field order. `Ctrl+S` is refused while any is an
    /// error, which is what keeps the preview honest.
    pub fn checks(&self) -> Vec<(Field, FieldCheck)> {
        Field::ALL
            .iter()
            .map(|field| (*field, self.check(*field)))
            .collect()
    }

    pub fn check(&self, field: Field) -> FieldCheck {
        match field {
            Field::Target => Some(if self.target().is_some() {
                Ok(())
            } else {
                Err("등록된 Target이 없습니다. Targets 화면에서 먼저 추가하세요.".to_string())
            }),
            Field::Project => Some(if self.project.trim().is_empty() {
                Err("프로젝트명을 입력하세요.".to_string())
            } else {
                Ok(())
            }),
            Field::Name => {
                if self.name.is_empty() {
                    return None;
                }
                Some(
                    match self.engine {
                        EngineKind::Postgres => util::validate_pg_identifier("DB명", &self.name),
                        EngineKind::Minio => minio::validate_bucket_name(&self.name),
                    }
                    .map_err(|e| e.as_diagnostic().what),
                )
            }
            Field::Principal => {
                if self.principal.is_empty() {
                    return None;
                }
                Some(
                    match self.engine {
                        EngineKind::Postgres => {
                            util::validate_pg_identifier("사용자명", &self.principal)
                        }
                        EngineKind::Minio => minio::validate_access_key(&self.principal),
                    }
                    .map_err(|e| e.as_diagnostic().what),
                )
            }
            Field::Kind | Field::Engine | Field::Secret | Field::Options | Field::Tunnel => None,
        }
    }

    pub fn is_valid(&self) -> bool {
        !self.name.is_empty()
            && !self.principal.is_empty()
            && self
                .checks()
                .iter()
                .all(|(_, check)| !matches!(check, Some(Err(_))))
    }

    /// First blocking problem, for the form's plan area.
    pub fn first_error(&self) -> Option<String> {
        for (_, check) in self.checks() {
            if let Some(Err(message)) = check {
                return Some(message);
            }
        }
        if self.name.is_empty() || self.principal.is_empty() {
            return Some("이름을 입력하면 실행 계획이 표시됩니다.".to_string());
        }
        None
    }

    // -- specs --------------------------------------------------------------

    pub fn engine_spec(&self) -> EngineSpec {
        EngineSpec::new(self.engine, self.major_version())
    }

    pub fn database_spec(&self) -> database::CreateSpec {
        database::CreateSpec {
            project_name: self.project.trim().to_string(),
            database_name: self.name.clone(),
            username: self.principal.clone(),
            password: None,
            encoding: self.encoding().to_string(),
            locale: self.locale().to_string(),
            preferred_local_tunnel_port: None,
        }
    }

    pub fn bucket_spec(&self) -> bucket::CreateSpec {
        let bucket_name = if self.want_postgres && self.want_minio {
            self.bucket_name.clone()
        } else {
            self.name.clone()
        };
        let access_key = if self.want_postgres && self.want_minio {
            self.generated_access_key.clone()
        } else {
            self.principal.clone()
        };
        bucket::CreateSpec {
            project_name: self.project.trim().to_string(),
            bucket_name,
            access_key: Some(access_key),
            secret_key: None,
            region: self.region().to_string(),
            preferred_local_tunnel_port: None,
        }
    }

    pub fn title(&self) -> String {
        let kind = self.kinds_label();
        if self.first_run {
            format!("첫 리소스 · {kind}")
        } else {
            format!("새 리소스 · {kind}")
        }
    }

    fn kinds_label(&self) -> String {
        match (self.want_postgres, self.want_minio) {
            (true, true) => "데이터베이스 + 버킷".into(),
            (true, false) => "데이터베이스".into(),
            (false, true) => "버킷".into(),
            (false, false) => "리소스".into(),
        }
    }

    fn kind_word(&self) -> &'static str {
        match self.engine {
            EngineKind::Postgres => "데이터베이스",
            EngineKind::Minio => "버킷",
        }
    }

    // -- rendering ----------------------------------------------------------

    fn value(&self, field: Field) -> String {
        match field {
            Field::Kind => self.kind_word().to_string(),
            Field::Target => self
                .target()
                .map(|t| t.display_name.clone())
                .unwrap_or_else(|| "없음".to_string()),
            Field::Engine => format!("{} {}", self.engine.as_str(), self.major_version()),
            Field::Project => self.project.clone(),
            Field::Name => self.name.clone(),
            Field::Principal => self.principal.clone(),
            Field::Secret => "자동 생성".to_string(),
            Field::Options => match self.engine {
                EngineKind::Postgres => format!("{} / {}", self.encoding(), self.locale()),
                EngineKind::Minio => self.region().to_string(),
            },
            Field::Tunnel => match self.target() {
                Some(t) if t.is_remote() => "생성 후 `t`로 시작".to_string(),
                Some(_) => "해당 없음 (local)".to_string(),
                None => "-".to_string(),
            },
        }
    }

    /// The alternatives `field` offers, in the order [`Form::select_index`]
    /// counts them. Empty where the answer is typed rather than chosen.
    fn choice_labels(&self, field: Field) -> Vec<String> {
        match field {
            // The service belongs in the answer: `데이터베이스` alone does
            // not say which image will show up in `docker ps`.
            Field::Kind => vec!["데이터베이스 (Postgres)".into(), "버킷 (MinIO)".into()],
            Field::Target => self
                .targets
                .iter()
                .map(|t| t.display_name.clone())
                .collect(),
            Field::Engine => self.majors().iter().map(|m| (*m).to_string()).collect(),
            Field::Options => match self.engine {
                EngineKind::Postgres => LOCALES
                    .iter()
                    .map(|(_, locale)| (*locale).to_string())
                    .collect(),
                EngineKind::Minio => REGIONS.iter().map(|r| (*r).to_string()).collect(),
            },
            Field::Principal if self.engine == EngineKind::Minio => {
                vec!["자동".into(), "직접".into()]
            }
            _ => Vec::new(),
        }
    }

    fn choice_index(&self, field: Field) -> usize {
        match field {
            Field::Kind => usize::from(self.engine != EngineKind::Postgres),
            Field::Target => self.target_index,
            Field::Engine => self.major_index,
            Field::Options => match self.engine {
                EngineKind::Postgres => self.locale_index,
                EngineKind::Minio => self.region_index,
            },
            Field::Principal if self.engine == EngineKind::Minio => {
                usize::from(!self.auto_principal)
            }
            _ => 0,
        }
    }

    /// Is option `i` of `field` the current answer? `Kind` is the one field
    /// whose two answers are independent, so it is asked twice rather than
    /// indexed.
    fn option_on(&self, field: Field, i: usize) -> bool {
        match field {
            Field::Kind if i == 0 => self.want_postgres,
            Field::Kind => self.want_minio,
            _ => i == self.choice_index(field),
        }
    }

    /// The option the next `Space` acts on.
    fn option_at(&self, field: Field) -> usize {
        if field == Field::Kind {
            self.option_cursor.min(1)
        } else {
            self.choice_index(field)
        }
    }

    /// How `field` draws its answer.
    fn cell(&self, field: Field) -> Cell {
        let labels = self.choice_labels(field);
        if labels.len() >= 2 {
            // Three alternatives are worth a row each; a longer list would
            // wrap, and a wrapped row puts its options where
            // [`Form::layout_hits`] says they are not.
            let fits = option_columns(LEAD + LABEL, &labels)
                .last()
                .is_some_and(|(x, width)| x + width <= FORM_COLS);
            return if labels.len() <= 3 && fits {
                Cell::Choice
            } else {
                Cell::Closed
            };
        }
        if self.editable(field) {
            Cell::Text
        } else {
            Cell::Note
        }
    }

    /// The clickable shape of the form: one row per question, in the order
    /// [`Form::lines`] draws them.
    ///
    /// Both `lines` and [`Form::layout_hits`] are built from this, so the
    /// rectangle a click is tested against is the one the row was drawn
    /// from. The two used to be computed separately, and disagreed.
    fn geometry(&self) -> Vec<Row> {
        self.focusable()
            .into_iter()
            .enumerate()
            .map(|(i, field)| Row {
                line: FIELDS_LINE + i as u16,
                options: if self.cell(field) == Cell::Choice {
                    option_columns(LEAD + LABEL, &self.choice_labels(field))
                } else {
                    Vec::new()
                },
                field,
            })
            .collect()
    }

    pub fn lines(&self, theme: &Theme) -> Vec<Line<'static>> {
        let geometry = self.geometry();
        let mut lines = Vec::with_capacity(geometry.len() + 10);
        lines.push(self.question_tabs(theme));
        lines.push(Line::raw(String::new()));
        lines.push(chrome::titled_rule("무엇을 만들까요?", FORM_COLS, theme));
        for row in &geometry {
            debug_assert_eq!(
                lines.len() as u16,
                row.line,
                "{:?} drifted away from its click target",
                row.field
            );
            lines.push(self.field_row(row, theme));
        }

        lines.push(Line::raw(String::new()));
        lines.push(chrome::titled_rule("요약", FORM_COLS, theme));
        lines.push(Line::from(Span::styled(
            self.summary_text(),
            theme.normal(),
        )));

        lines.push(Line::raw(String::new()));
        let plan_title = if self.plan_stale() {
            format!("실행 계획 {}", theme.ellipsis())
        } else {
            "실행 계획".to_string()
        };
        lines.push(chrome::titled_rule(&plan_title, FORM_COLS, theme));
        match (&self.plan, &self.plan_error, self.first_error()) {
            (None, _, Some(problem)) => {
                lines.push(Line::from(Span::styled(problem, theme.muted())))
            }
            (Some(plan), _, _) => lines.extend(plan_lines(plan, theme)),
            (None, Some(error), None) => {
                lines.push(Line::from(Span::styled(error.clone(), theme.danger())))
            }
            (None, None, None) => lines.push(Line::from(Span::styled(
                format!("계획을 계산하는 중{}", theme.ellipsis()),
                theme.muted(),
            ))),
        }
        if !self.steps.is_empty() {
            lines.push(Line::raw(String::new()));
            for step in &self.steps {
                lines.push(Line::from(Span::styled(step.clone(), theme.ok())));
            }
        }
        lines
    }

    /// The strip that names the questions, with the focused one lit. It is
    /// the only thing that says how many answers the form wants before the
    /// eye has walked the rows.
    fn question_tabs(&self, theme: &Theme) -> Line<'static> {
        let separator = format!(" {} ", chrome::Glyphs::of(theme).horizontal);
        let mut spans = Vec::new();
        for (i, field) in self.focusable().into_iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(separator.clone(), theme.muted()));
            }
            let label = match field {
                Field::Kind => "종류",
                Field::Target => "Target",
                Field::Engine => "엔진",
                Field::Project => "이름",
                Field::Options => "옵션",
                other => other.label(self.engine),
            };
            let on = field == self.focus;
            spans.push(Span::styled(
                label.to_string(),
                if on {
                    theme.accent().add_modifier(ratatui::style::Modifier::BOLD)
                } else {
                    theme.muted()
                },
            ));
        }
        Line::from(spans)
    }

    /// Gutter, label, answer, verdict, on exactly one line. The focused
    /// field is the one with the heavy bar, the gutter mark and the cursor.
    fn field_row(&self, row: &Row, theme: &Theme) -> Line<'static> {
        let g = Widgets::of(theme);
        let focused = row.field == self.focus;
        let mut spans = Vec::new();
        let mut used = 0u16;
        push(&mut spans, &mut used, g.lead(focused), theme.accent());
        push(
            &mut spans,
            &mut used,
            pad(row.field.label(self.engine), LABEL as usize),
            theme.muted(),
        );
        match self.cell(row.field) {
            Cell::Choice => {
                self.push_options(&mut spans, &mut used, row, theme);
                // MinIO's access key is a choice *and* a value: `자동`
                // derives one, `직접` opens a box to type it into.
                if row.field == Field::Principal {
                    push(&mut spans, &mut used, "  ".to_string(), theme.normal());
                    self.push_value(&mut spans, &mut used, row.field, focused, theme);
                }
            }
            Cell::Closed => self.push_closed(&mut spans, &mut used, row.field, focused, theme),
            Cell::Text | Cell::Note => {
                self.push_value(&mut spans, &mut used, row.field, focused, theme)
            }
        }
        self.push_verdict(&mut spans, used, row.field, theme);
        Line::from(spans)
    }

    /// Every alternative on the row, the chosen one marked. Each option is
    /// drawn from the column `geometry` measured it at, so a click cannot
    /// land between two options the row never separated.
    fn push_options(
        &self,
        spans: &mut Vec<Span<'static>>,
        used: &mut u16,
        row: &Row,
        theme: &Theme,
    ) {
        let g = Widgets::of(theme);
        let labels = self.choice_labels(row.field);
        let focused = row.field == self.focus;
        let cursor = self.option_at(row.field);
        for (i, (x, _)) in row.options.iter().enumerate() {
            if *x > *used {
                let gap = (*x - *used) as usize;
                push(spans, used, " ".repeat(gap), theme.normal());
            }
            let on = self.option_on(row.field, i);
            let mark = if row.field == Field::Kind {
                g.check(on)
            } else {
                g.radio(on)
            };
            // Reversed rather than coloured: the cursor has to be visible on
            // a `NO_COLOR` terminal too.
            let style = if focused && i == cursor {
                theme.selected()
            } else if on {
                theme.heading()
            } else {
                theme.muted()
            };
            push(spans, used, format!("{mark} {}", labels[i]), style);
        }
    }

    /// The typed answer: a bar-framed box where the field can be edited, the
    /// plain derived value where it cannot.
    fn push_value(
        &self,
        spans: &mut Vec<Span<'static>>,
        used: &mut u16,
        field: Field,
        focused: bool,
        theme: &Theme,
    ) {
        let g = Widgets::of(theme);
        let mut value = self.value(field);
        if !self.editable(field) {
            push(spans, used, value, theme.muted());
            return;
        }
        if focused {
            value.push('_');
        }
        let room = FORM_COLS
            .saturating_sub(*used + 4 + VERDICT)
            .clamp(8, VALUE);
        let bar = g.bar(focused);
        let frame = if focused {
            theme.accent()
        } else {
            theme.muted()
        };
        push(spans, used, format!("{bar} "), frame);
        push(
            spans,
            used,
            text_window(&value, room as usize),
            theme.normal(),
        );
        push(spans, used, format!(" {bar}"), frame);
    }

    /// A closed dropdown: the chosen alternative and a caret. Focused, it
    /// gains the same bars a text box has, so the row still says where the
    /// keyboard is.
    fn push_closed(
        &self,
        spans: &mut Vec<Span<'static>>,
        used: &mut u16,
        field: Field,
        focused: bool,
        theme: &Theme,
    ) {
        let g = Widgets::of(theme);
        let labels = self.choice_labels(field);
        let shown = labels
            .get(self.choice_index(field))
            .cloned()
            .unwrap_or_else(|| self.value(field));
        // Padded to the widest alternative, so the caret does not shuffle
        // sideways as the list is cycled.
        let cols = labels
            .iter()
            .map(|label| util::display_cols(label))
            .max()
            .unwrap_or(0)
            .min(VALUE as usize);
        let frame = if focused {
            theme.accent()
        } else {
            theme.muted()
        };
        if focused {
            push(spans, used, format!("{} ", g.bar_on), frame);
        }
        push(spans, used, pad(&shown, cols), theme.normal());
        push(spans, used, format!(" {}", g.caret), theme.muted());
        if focused {
            push(spans, used, format!(" {}", g.bar_on), frame);
        }
    }

    /// The inline verdict, to the right of the field it judges.
    ///
    /// It falls back to its symbol when the sentence would not fit: the
    /// sentence is already in the plan area below, and a row that wrapped
    /// would move every row under it off the line `layout_hits` promised.
    fn push_verdict(&self, spans: &mut Vec<Span<'static>>, used: u16, field: Field, theme: &Theme) {
        let g = Widgets::of(theme);
        let (text, style) = match self.check(field) {
            Some(Ok(())) if self.editable(field) => (format!("{} 사용 가능", g.ok), theme.ok()),
            Some(Err(message)) => (format!("! {message}"), theme.danger()),
            _ => return,
        };
        let room = FORM_COLS.saturating_sub(used + 2) as usize;
        let text = if util::display_cols(&text) <= room {
            text
        } else {
            text.split(' ').next().unwrap_or("!").to_string()
        };
        spans.push(Span::styled(format!("  {text}"), style));
    }

    fn summary_text(&self) -> String {
        let mut parts = vec![self.kinds_label()];
        if let Some(t) = self.target() {
            parts.push(t.display_name.clone());
        }
        if self.want_postgres {
            parts.push(format!("postgres {}", self.major_version()));
        }
        if self.want_minio {
            parts.push("minio".into());
        }
        if self.project.trim().is_empty() {
            parts.push("(프로젝트명 필요)".into());
        } else {
            parts.push(self.project.trim().to_string());
        }
        // The secret is never typed and never shown (PRD §11.2). The field
        // list has no row for it, so the summary is where the form says so.
        parts.push(format!("{} 자동 생성", Field::Secret.label(self.engine)));
        parts.join(" · ")
    }

    /// Click targets for the last drawn form, in the coordinates
    /// [`Form::lines`] was rendered in: line `n` of the form is row
    /// `inner.y + n`.
    pub fn layout_hits(&self, inner: ratatui::layout::Rect) -> FormLayoutHits {
        let mut fields = Vec::new();
        let mut choices = Vec::new();
        for row in self.geometry() {
            // A row past the bottom of the panel was never drawn, so it is
            // not clickable either.
            if row.line >= inner.height {
                break;
            }
            let y = inner.y.saturating_add(row.line);
            fields.push((
                ratatui::layout::Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
                row.field,
            ));
            for (i, (x, width)) in row.options.iter().enumerate() {
                choices.push((
                    ratatui::layout::Rect {
                        x: inner.x.saturating_add(*x),
                        y,
                        width: *width,
                        height: 1,
                    },
                    row.field,
                    i,
                ));
            }
        }
        FormLayoutHits { fields, choices }
    }
}

pub struct FormLayoutHits {
    pub fields: Vec<(ratatui::layout::Rect, Field)>,
    pub choices: Vec<(ratatui::layout::Rect, Field, usize)>,
}

/// One drawn row of the form: the field it edits, the line it sits on and
/// the columns its options landed in.
struct Row {
    field: Field,
    line: u16,
    /// `(column, width)` per option, in option order. Empty for a text box
    /// and for a closed dropdown, neither of which draws its alternatives.
    options: Vec<(u16, u16)>,
}

/// How a row draws its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cell {
    /// Every alternative on the row, one of them marked.
    Choice,
    /// The chosen alternative and a caret: too many alternatives to spend a
    /// row on.
    Closed,
    /// A box to type in.
    Text,
    /// A value the form derived, which cannot be changed here.
    Note,
}

/// `(column, width)` for each option laid out left to right from `x`.
fn option_columns(x: u16, labels: &[String]) -> Vec<(u16, u16)> {
    let mut out = Vec::with_capacity(labels.len());
    let mut x = x;
    for label in labels {
        let width = MARK + util::display_cols(label) as u16;
        out.push((x, width));
        x = x.saturating_add(width + GAP);
    }
    out
}

/// Append `text` and count the columns it took, so the row knows how much of
/// its budget is left for the verdict.
fn push(
    spans: &mut Vec<Span<'static>>,
    used: &mut u16,
    text: String,
    style: ratatui::style::Style,
) {
    *used = used.saturating_add(util::display_cols(&text) as u16);
    spans.push(Span::styled(text, style));
}

/// The visible slice of a text field, padded to `cols`.
///
/// A value wider than its box scrolls off the *left*: the right-hand end is
/// where the cursor is, and a field that hides what was just typed is worse
/// than one that hides what was typed first.
pub(crate) fn text_window(value: &str, cols: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut width = util::display_cols(value);
    let mut start = 0usize;
    for (offset, c) in value.char_indices() {
        if width <= cols {
            start = offset;
            break;
        }
        width -= c.width().unwrap_or(0);
        start = offset + c.len_utf8();
    }
    pad(&value[start..], cols)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::data::fixture;
    use crate::tui::rows::render_lines;

    fn form(engine: EngineKind) -> Form {
        Form::new(
            vec![fixture::local_target(), fixture::remote_target()],
            engine,
        )
    }

    /// A UTF-8 terminal without colour, so an assertion can name a glyph
    /// without also asserting a style.
    fn unicode() -> Theme {
        Theme {
            color: false,
            unicode: true,
            reduced_motion: true,
        }
    }

    fn type_text(form: &mut Form, text: &str) {
        if !form.editable(form.focus) {
            form.focus = Field::Project;
        }
        for c in text.chars() {
            form.type_char(c);
        }
    }

    /// The rendered row `field` was drawn on, found the way
    /// [`Form::layout_hits`] finds it.
    fn row_of(form: &Form, text: &str, field: Field) -> String {
        let i = form
            .focusable()
            .iter()
            .position(|f| *f == field)
            .unwrap_or_else(|| panic!("{field:?} is not a row of this form"));
        text.lines()
            .nth(FIELDS_LINE as usize + i)
            .unwrap_or_else(|| panic!("the form drew no line for {field:?}:\n{text}"))
            .to_string()
    }

    /// The `cols` terminal columns of `line` starting at column `at`.
    fn columns(line: &str, at: u16, cols: u16) -> String {
        use unicode_width::UnicodeWidthChar;
        let mut out = String::new();
        let mut col = 0u16;
        for c in line.chars() {
            let width = c.width().unwrap_or(0) as u16;
            if col >= at && col + width <= at + cols {
                out.push(c);
            }
            col += width;
        }
        out
    }

    #[test]
    fn fields_appear_in_the_prd_order_with_service_specific_labels() {
        let pg = form(EngineKind::Postgres);
        let labels: Vec<&str> = Field::ALL.iter().map(|f| f.label(pg.engine)).collect();
        assert_eq!(
            labels,
            vec![
                "종류",
                "Target",
                "엔진",
                "프로젝트명",
                "DB명",
                "사용자명",
                "비밀번호",
                "인코딩/로케일",
                "터널 자동시작"
            ]
        );

        let minio = form(EngineKind::Minio);
        assert_eq!(Field::Name.label(minio.engine), "버킷명");
        assert_eq!(Field::Principal.label(minio.engine), "액세스 키");
        assert_eq!(Field::Secret.label(minio.engine), "시크릿 키");
        assert_eq!(Field::Options.label(minio.engine), "리전");
    }

    #[test]
    fn the_kind_field_comes_first_and_retitles_the_modal() {
        let mut f = form(EngineKind::Postgres);
        assert_eq!(Field::ALL[0], Field::Kind);
        assert_eq!(f.title(), "새 리소스 · 데이터베이스");

        f.focus = Field::Kind;
        assert!(f.cycle(true));
        assert_eq!(f.engine, EngineKind::Minio);
        assert_eq!(f.title(), "새 리소스 · 버킷");
    }

    #[test]
    fn typing_a_project_name_derives_postgres_names() {
        let mut f = form(EngineKind::Postgres);
        type_text(&mut f, "Letsbid");
        assert_eq!(f.name, "letsbid_dev");
        assert_eq!(f.principal, "letsbid_user");
        assert!(f.is_valid());
    }

    #[test]
    fn switching_to_object_storage_reshapes_the_derived_names() {
        let mut f = form(EngineKind::Postgres);
        type_text(&mut f, "Dalbit Editor");
        assert_eq!(f.name, "dalbit_editor_dev");

        f.set_engine(EngineKind::Minio);
        assert!(!f.name.contains('_'), "bucket names use `-`: {}", f.name);
        assert!(minio::validate_bucket_name(&f.name).is_ok());
        assert!(minio::validate_access_key(&f.principal).is_ok());
        assert_eq!(f.major_version(), "latest");
    }

    #[test]
    fn a_hand_edited_name_is_never_overwritten_by_the_project_field() {
        let mut f = form(EngineKind::Postgres);
        type_text(&mut f, "Letsbid");
        f.focus = Field::Name;
        f.backspace();
        assert_eq!(f.name, "letsbid_de");

        f.focus = Field::Project;
        type_text(&mut f, "X");
        assert_eq!(f.name, "letsbid_de", "derivation stops after a manual edit");
    }

    #[test]
    fn invalid_identifiers_are_reported_inline_and_block_submission() {
        let mut f = form(EngineKind::Postgres);
        type_text(&mut f, "Letsbid");
        f.focus = Field::Name;
        type_text(&mut f, "!!");
        assert!(matches!(f.check(Field::Name), Some(Err(_))));
        assert!(!f.is_valid());
        assert!(f.first_error().is_some());

        // The verdict stays on the field's own row, next to the box.
        let text = render_lines(&f.lines(&Theme::plain()));
        assert!(row_of(&f, &text, Field::Name).contains('!'));
    }

    #[test]
    fn an_invalid_bucket_name_is_reported_by_the_minio_validator() {
        let mut f = form(EngineKind::Minio);
        type_text(&mut f, "Letsbid");
        f.focus = Field::Name;
        f.name.clear();
        f.name.push_str("A_B");
        assert!(matches!(f.check(Field::Name), Some(Err(_))));
    }

    #[test]
    fn an_empty_form_reports_no_verdict_rather_than_a_false_error() {
        let f = form(EngineKind::Postgres);
        assert_eq!(f.check(Field::Name), None);
        assert!(matches!(f.check(Field::Project), Some(Err(_))));
        assert!(!f.is_valid());
    }

    #[test]
    fn with_no_targets_registered_the_form_says_so_instead_of_offering_none() {
        let f = Form::new(Vec::new(), EngineKind::Postgres);
        assert!(matches!(f.check(Field::Target), Some(Err(_))));
        assert!(!f.is_valid());

        // A target row with nothing to choose from is a note, not a widget.
        let text = render_lines(&f.lines(&unicode()));
        let row = row_of(&f, &text, Field::Target);
        assert!(row.contains("없음"), "{row:?}");
        assert!(!row.contains('●'), "{row:?}");
    }

    #[test]
    fn tab_walks_focusable_fields_and_wraps() {
        let mut f = form(EngineKind::Postgres);
        f.focus = Field::Kind;
        let n = f.focusable().len();
        for _ in 0..n {
            f.next_field();
        }
        assert_eq!(f.focus, Field::Kind);
        f.prev_field();
        assert_eq!(f.focus, Field::Options);
        assert_ne!(f.focus, Field::Tunnel);
    }

    #[test]
    fn kind_is_a_checkbox_pair_not_a_dropdown() {
        let f = form(EngineKind::Postgres);
        let text = render_lines(&f.lines(&unicode()));
        let row = row_of(&f, &text, Field::Kind);
        assert!(row.contains("[x] 데이터베이스"), "{row:?}");
        assert!(row.contains("[ ] 버킷"), "{row:?}");
        assert!(!row.contains('▾'), "the two kinds are independent: {row:?}");
        assert!(text.contains("무엇을 만들까요"));
        assert!(text.contains("종류"));
        assert!(text.contains("요약"));
    }

    #[test]
    fn an_exclusive_choice_is_radio_buttons_until_the_list_stops_fitting() {
        let pg = form(EngineKind::Postgres);
        let text = render_lines(&pg.lines(&unicode()));
        let row = row_of(&pg, &text, Field::Options);
        assert!(
            row.contains("(●) C"),
            "the chosen locale is filled: {row:?}"
        );
        assert!(row.contains("(○) C.UTF-8"), "{row:?}");
        assert!(row.contains("(○) en_US.UTF-8"), "{row:?}");
        assert!(!row.contains('▾'), "three alternatives fit: {row:?}");

        // Four regions do not, so the region closes into a dropdown.
        let mut minio = form(EngineKind::Minio);
        minio.focus = Field::Options;
        let text = render_lines(&minio.lines(&unicode()));
        let row = row_of(&minio, &text, Field::Options);
        assert!(row.contains("us-east-1"), "{row:?}");
        assert!(row.contains('▾'), "{row:?}");
        assert!(!row.contains('●'), "{row:?}");
        assert_eq!(
            minio
                .layout_hits(ratatui::layout::Rect {
                    x: 0,
                    y: 0,
                    width: 78,
                    height: 40,
                })
                .choices
                .iter()
                .filter(|(_, field, _)| *field == Field::Options)
                .count(),
            0,
            "a closed dropdown offers no option to click"
        );
    }

    #[test]
    fn the_focused_field_is_marked_without_relying_on_colour() {
        let mut f = form(EngineKind::Postgres);
        type_text(&mut f, "Letsbid");
        assert_eq!(f.focus, Field::Project);

        // `Theme::plain()` has no colour and no heavy bar, so the gutter mark
        // and the cursor are all that is left to say where the keyboard is.
        let text = render_lines(&f.lines(&Theme::plain()));
        let focused = row_of(&f, &text, Field::Project);
        assert!(focused.starts_with("> "), "no gutter mark: {focused:?}");
        assert!(focused.contains("Letsbid_"), "no cursor: {focused:?}");

        let quiet = row_of(&f, &text, Field::Name);
        assert!(quiet.starts_with("  "), "{quiet:?}");
        assert!(
            !quiet.contains("letsbid_dev_"),
            "an unfocused field must not wear the cursor: {quiet:?}"
        );
    }

    #[test]
    fn a_utf8_terminal_gets_the_heavy_bar_on_the_focused_field() {
        let mut f = form(EngineKind::Postgres);
        type_text(&mut f, "Letsbid");
        let text = render_lines(&f.lines(&unicode()));

        let focused = row_of(&f, &text, Field::Project);
        assert!(focused.contains('┃'), "{focused:?}");
        assert!(focused.contains('▸'), "{focused:?}");

        let quiet = row_of(&f, &text, Field::Name);
        assert!(quiet.contains('│'), "{quiet:?}");
        assert!(!quiet.contains('┃'), "{quiet:?}");
    }

    #[test]
    fn the_ascii_fallback_draws_nothing_a_non_utf8_terminal_cannot_print() {
        let g = Widgets::of(&Theme::plain());
        for glyph in [
            g.bar_on,
            g.bar,
            g.radio_on,
            g.radio_off,
            g.caret,
            g.ok,
            g.gutter,
        ] {
            assert!(glyph.is_ascii(), "{glyph:?} is not ASCII");
        }

        // The Korean labels are the product's words, not chrome, so they
        // stay. What must not survive the fallback is a box-drawing or
        // geometric glyph, which is exactly what such a terminal cannot draw.
        let mut f = form(EngineKind::Minio);
        type_text(&mut f, "Letsbid");
        f.focus = Field::Options;
        let text = render_lines(&f.lines(&Theme::plain()));
        for banned in ['─', '│', '┃', '●', '○', '▾', '▸', '…', '✓'] {
            assert!(
                !text.contains(banned),
                "{banned:?} survived the ASCII fallback:\n{text}"
            );
        }
    }

    #[test]
    fn layout_hits_land_on_the_rows_the_form_drew() {
        let mut f = form(EngineKind::Postgres);
        type_text(&mut f, "Letsbid");
        let text = render_lines(&f.lines(&Theme::plain()));
        let rendered: Vec<&str> = text.lines().collect();
        let inner = ratatui::layout::Rect {
            x: 3,
            y: 5,
            width: 78,
            height: 40,
        };
        let hits = f.layout_hits(inner);

        assert_eq!(hits.fields.len(), f.focusable().len());
        for (rect, field) in &hits.fields {
            let line = rendered[(rect.y - inner.y) as usize];
            assert!(
                line.contains(field.label(f.engine)),
                "{field:?} was drawn on another line than its click target: {line:?}"
            );
        }
        assert!(!hits.choices.is_empty());
        for (rect, field, index) in &hits.choices {
            let line = rendered[(rect.y - inner.y) as usize];
            let label = &f.choice_labels(*field)[*index];
            let cell = columns(line, rect.x - inner.x, rect.width);
            assert!(
                cell.starts_with('(') || cell.starts_with('['),
                "option {index} of {field:?} does not start at its rect: {cell:?}"
            );
            assert!(
                cell.ends_with(label),
                "option {index} of {field:?} is not under its rect: {cell:?}"
            );
        }

        // A row below the panel was never drawn, so it is not clickable.
        let clipped = f.layout_hits(ratatui::layout::Rect { height: 4, ..inner });
        assert_eq!(clipped.fields.len(), 1);
        assert_eq!(clipped.fields[0].1, Field::Kind);
    }

    #[test]
    fn choice_fields_cycle_and_text_fields_ignore_the_cycle_keys() {
        let mut f = form(EngineKind::Postgres);
        f.focus = Field::Target;
        assert!(f.cycle(true));
        assert_eq!(f.target().unwrap().display_name, "dev-vps");

        f.focus = Field::Engine;
        assert_eq!(f.major_version(), "17");
        assert!(f.cycle(true));
        assert_eq!(f.major_version(), "16");

        f.focus = Field::Options;
        assert_eq!(f.locale(), "C");
        assert!(f.cycle(true));
        assert_eq!(f.locale(), "C.UTF-8");

        f.focus = Field::Project;
        assert!(!f.cycle(true));
        f.focus = Field::Secret;
        assert!(!f.cycle(true), "the secret is always generated");
    }

    #[test]
    fn the_bucket_region_cycles_through_presets() {
        let mut f = form(EngineKind::Minio);
        f.focus = Field::Options;
        assert_eq!(f.region(), "us-east-1");
        assert!(f.cycle(true));
        assert_eq!(f.region(), "us-west-2");
        assert!(f.cycle(false));
        assert_eq!(f.region(), "us-east-1");
    }

    #[test]
    fn the_access_key_is_generated_until_the_user_asks_to_type_one() {
        let mut f = form(EngineKind::Minio);
        type_text(&mut f, "Letsbid");
        let generated = f.principal.clone();
        assert!(!f.editable(Field::Principal));

        f.focus = Field::Principal;
        assert!(f.cycle(true));
        assert!(f.editable(Field::Principal));
        f.principal.clear();
        type_text(&mut f, "MYOWNKEY123");
        assert_eq!(f.principal, "MYOWNKEY123");

        // The row carries both answers: which mode, and the key itself.
        f.focus = Field::Principal;
        let text = render_lines(&f.lines(&unicode()));
        let row = row_of(&f, &text, Field::Principal);
        assert!(row.contains("(○) 자동"), "{row:?}");
        assert!(row.contains("(●) 직접"), "{row:?}");
        assert!(row.contains("MYOWNKEY123"), "{row:?}");

        assert!(f.cycle(true), "cycling back restores generation");
        assert_eq!(f.principal, generated);
    }

    #[test]
    fn a_postgres_role_name_is_always_typeable() {
        let f = form(EngineKind::Postgres);
        assert!(f.editable(Field::Principal));
        assert!(f.editable(Field::Name));
        assert!(!f.editable(Field::Secret));
        assert!(!f.editable(Field::Kind));
    }

    #[test]
    fn every_edit_marks_the_preview_stale_so_a_stale_plan_is_never_trusted() {
        let mut f = form(EngineKind::Postgres);
        let epoch = f.invalidate_plan();
        f.accept_plan(epoch, Ok(Plan::new("preview")));
        assert!(!f.plan_stale());

        type_text(&mut f, "L");
        assert!(f.plan_stale());
    }

    #[test]
    fn a_late_plan_for_an_older_edit_is_discarded() {
        let mut f = form(EngineKind::Postgres);
        let first = f.invalidate_plan();
        let second = f.invalidate_plan();
        f.accept_plan(second, Ok(Plan::new("second")));
        f.accept_plan(first, Ok(Plan::new("first")));
        assert_eq!(f.plan.as_ref().unwrap().title, "second");
    }

    #[test]
    fn specs_carry_exactly_what_the_form_collected() {
        let mut f = form(EngineKind::Postgres);
        type_text(&mut f, "Letsbid");
        let spec = f.database_spec();
        assert_eq!(spec.database_name, "letsbid_dev");
        assert_eq!(spec.username, "letsbid_user");
        assert_eq!(spec.encoding, "UTF8");
        assert_eq!(spec.locale, "C");
        assert!(spec.password.is_none(), "the form never carries a secret");
        assert_eq!(f.engine_spec().major_version, "17");

        f.set_engine(EngineKind::Minio);
        let spec = f.bucket_spec();
        assert_eq!(spec.region, "us-east-1");
        assert!(spec.secret_key.is_none());
        assert_eq!(spec.access_key.as_deref(), Some(f.principal.as_str()));
    }

    #[test]
    fn the_rendered_form_shows_the_plan_and_marks_the_focused_field() {
        let mut f = form(EngineKind::Postgres);
        type_text(&mut f, "Letsbid");
        let epoch = f.invalidate_plan();
        f.accept_plan(
            epoch,
            Ok(Plan::new("생성").step(
                crate::core::plan::StepKind::New,
                "컨테이너 linf-postgres-17 생성",
            )),
        );
        let text = render_lines(&f.lines(&unicode()));
        assert!(text.contains("이름"));
        assert!(text.contains("요약"));
        assert!(text.contains("Letsbid"));
        assert!(text.contains("실행 계획"));
        assert!(text.contains("컨테이너 linf-postgres-17 생성"));
        assert!(text.contains("✓ 사용 가능"));
        assert!(
            text.contains("비밀번호 자동 생성"),
            "the summary says the secret is generated:\n{text}"
        );
    }

    #[test]
    fn a_value_wider_than_its_box_keeps_its_tail_visible() {
        assert_eq!(text_window("abc", 5), "abc  ");
        assert_eq!(text_window("abcdef", 3), "def");
        // Two columns per Korean syllable, so the cut is not on char count.
        assert_eq!(text_window("가나다", 4), "나다");
    }

    #[test]
    fn the_generated_access_key_is_stable_across_keystrokes() {
        let mut f = form(EngineKind::Minio);
        type_text(&mut f, "Lets");
        let first = f.principal.clone();
        type_text(&mut f, "bid");
        assert_eq!(f.principal, first);
        assert!(minio::validate_access_key(&first).is_ok());
    }
}
