//! Execution plans (PRD §7.5, §12.3).
//!
//! Every mutating use case can produce its plan *without* performing it, so the
//! TUI can render a live preview and the CLI can offer `--plan`. The same
//! `Plan` is then replayed step by step by the executing call, which is what
//! makes "실행 전 계획, 실행 후 검증" cheap to keep honest.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// A resource will be created.
    New,
    /// An existing resource will be reused untouched.
    Reuse,
    /// A check, not a change.
    Verify,
    /// Something will be removed. Rendered separately and never silent.
    Destroy,
}

impl StepKind {
    pub fn label(self) -> &'static str {
        match self {
            StepKind::New => "신규",
            StepKind::Reuse => "재사용",
            StepKind::Verify => "확인",
            StepKind::Destroy => "삭제",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanStep {
    pub kind: StepKind,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl PlanStep {
    pub fn new(kind: StepKind, title: impl Into<String>) -> Self {
        Self {
            kind,
            title: title.into(),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct Plan {
    pub title: String,
    pub steps: Vec<PlanStep>,
    /// Consequences the user must read before confirming (PRD §7.9).
    pub warnings: Vec<String>,
}

impl Plan {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            steps: Vec::new(),
            warnings: Vec::new(),
        }
    }

    pub fn step(mut self, kind: StepKind, title: impl Into<String>) -> Self {
        self.steps.push(PlanStep::new(kind, title));
        self
    }

    pub fn step_detailed(
        mut self,
        kind: StepKind,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        self.steps
            .push(PlanStep::new(kind, title).with_detail(detail));
        self
    }

    pub fn warn(mut self, warning: impl Into<String>) -> Self {
        self.warnings.push(warning.into());
        self
    }

    pub fn push(&mut self, step: PlanStep) {
        self.steps.push(step);
    }

    pub fn is_destructive(&self) -> bool {
        self.steps.iter().any(|s| s.kind == StepKind::Destroy)
    }

    pub fn len(&self) -> usize {
        self.steps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Numbered plain-text rendering shared by the CLI and the TUI preview.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (i, step) in self.steps.iter().enumerate() {
            out.push_str(&format!(
                "{:>2}. {} ({})",
                i + 1,
                step.title,
                step.kind.label()
            ));
            if let Some(detail) = &step.detail {
                out.push_str(&format!("\n    {detail}"));
            }
            out.push('\n');
        }
        for warning in &self.warnings {
            out.push_str(&format!(" !  {warning}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_renders_numbered_steps_with_new_or_reuse_marked() {
        let plan = Plan::new("새 데이터베이스")
            .step(StepKind::Verify, "postgres:17 이미지 확인")
            .step_detailed(
                StepKind::New,
                "컨테이너 linf-postgres-17 생성",
                "127.0.0.1:5432",
            )
            .step(StepKind::Reuse, "볼륨 linf-pg17-data 사용");
        let text = plan.render();
        assert!(text.contains(" 1. postgres:17 이미지 확인 (확인)"));
        assert!(text.contains(" 2. 컨테이너 linf-postgres-17 생성 (신규)"));
        assert!(text.contains("    127.0.0.1:5432"));
        assert!(text.contains(" 3. 볼륨 linf-pg17-data 사용 (재사용)"));
        assert!(!plan.is_destructive());
    }

    #[test]
    fn destructive_plans_are_flagged_and_carry_warnings() {
        let plan = Plan::new("볼륨 삭제")
            .step(StepKind::Destroy, "볼륨 linf-pg17-data 삭제")
            .warn("3개 DB의 모든 데이터가 영구 삭제됩니다");
        assert!(plan.is_destructive());
        assert!(plan
            .render()
            .contains(" !  3개 DB의 모든 데이터가 영구 삭제됩니다"));
    }
}
