//! First-run setup. Derived from the snapshot — nothing extra is stored.
//!
//! Until the user has one database or bucket, the dashboard is a single
//! focused card: only the step that needs an answer right now. Past and
//! future steps stay off the screen so there is one thing to do.

use crate::core::doctor::Check;
use crate::tui::data::Snapshot;
use crate::tui::theme::Theme;
use ratatui::text::{Line, Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Doctor has not reported yet; the first frame after launch.
    Checking,
    /// Docker CLI or the registered daemon is down.
    DockerDown,
    /// Docker answers, but no target is registered.
    RegisterLocal,
    /// A reachable target exists; no project resource yet.
    CreateFirst,
    /// At least one database or bucket exists. The normal dashboard takes over.
    Done,
}

impl Phase {
    pub fn active(self) -> bool {
        !matches!(self, Phase::Done)
    }

    /// 1-based index in the three-step first run, for a quiet `2/3` mark.
    pub fn number(self) -> u8 {
        match self {
            Phase::Checking | Phase::DockerDown => 1,
            Phase::RegisterLocal => 2,
            Phase::CreateFirst => 3,
            Phase::Done => 3,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Phase::Checking => "환경을 확인하는 중입니다",
            Phase::DockerDown => "Docker를 먼저 켜 주세요",
            Phase::RegisterLocal => "이 컴퓨터를 등록할까요?",
            Phase::CreateFirst => "첫 DB 또는 버킷을 만듭니다",
            Phase::Done => "",
        }
    }
}

pub fn phase(snap: &Snapshot) -> Phase {
    if !snap.resources.is_empty() {
        return Phase::Done;
    }
    if snap.targets.is_empty() {
        return match docker_cli(snap) {
            None => Phase::Checking,
            Some(check) if check.ok => Phase::RegisterLocal,
            Some(_) => Phase::DockerDown,
        };
    }
    if snap.targets.iter().any(|t| t.reachable) {
        Phase::CreateFirst
    } else {
        Phase::DockerDown
    }
}

/// Label for the primary key (Enter) on the hint bar.
pub fn primary_label(phase: Phase) -> &'static str {
    match phase {
        Phase::Checking | Phase::DockerDown => "다시 확인",
        Phase::RegisterLocal => "이 컴퓨터 등록",
        Phase::CreateFirst => "첫 리소스 만들기",
        Phase::Done => "",
    }
}

/// Body of the current step only. No checklist of what is next or already done.
pub fn lines(snap: &Snapshot, notices: &[String], theme: &Theme) -> Vec<Line<'static>> {
    let current = phase(snap);
    let docker = docker_cli(snap);
    let mut lines = Vec::new();

    lines.push(muted(format!("{}/3", current.number()), theme));
    lines.push(Line::raw(String::new()));
    lines.push(Line::from(Span::styled(
        current.title().to_string(),
        theme.heading(),
    )));
    lines.push(Line::raw(String::new()));

    match current {
        Phase::Checking => {
            lines.push(muted(
                "Docker가 응답하는지 살펴봅니다. 잠시만 기다리거나 Enter를 누르세요.",
                theme,
            ));
        }
        Phase::DockerDown => {
            let detail = docker
                .map(|c| c.detail.as_str())
                .unwrap_or("Docker가 응답하지 않습니다.");
            lines.push(warn(detail.to_string(), theme));
            let remedy = docker
                .and_then(|c| c.remedy.as_deref())
                .unwrap_or("Docker Desktop을 켠 다음 Enter로 다시 확인하세요.");
            lines.push(muted(remedy.to_string(), theme));
        }
        Phase::RegisterLocal => {
            lines.push(muted(
                "로컬 Docker를 Target으로 둡니다. 컨테이너는 아직 만들지 않습니다.",
                theme,
            ));
        }
        Phase::CreateFirst => {
            lines.push(muted(
                "프로젝트 이름만 넣으면 엔진·계정·비밀번호는 자동으로 준비됩니다.",
                theme,
            ));
        }
        Phase::Done => {}
    }

    if current.active() {
        lines.push(Line::raw(String::new()));
        lines.push(Line::from(Span::styled(
            format!("Enter  ·  {}", primary_label(current)),
            theme.accent(),
        )));
    }

    if !notices.is_empty() {
        lines.push(Line::raw(String::new()));
        for notice in notices {
            lines.push(Line::from(Span::styled(
                format!("! {notice}"),
                theme.warn(),
            )));
        }
    }

    lines
}

fn docker_cli(snap: &Snapshot) -> Option<&Check> {
    snap.checks.iter().find(|c| c.name == "Docker CLI")
}

fn muted(text: impl Into<String>, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(text.into(), theme.muted()))
}

fn warn(text: impl Into<String>, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(text.into(), theme.warn()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::doctor::Check;
    use crate::tui::data::fixture;
    use crate::tui::rows::render_lines;

    fn docker_ok() -> Check {
        Check {
            name: "Docker CLI".into(),
            ok: true,
            detail: "docker 27.1.1".into(),
            remedy: None,
        }
    }

    fn docker_down() -> Check {
        Check {
            name: "Docker CLI".into(),
            ok: false,
            detail: "`docker` 명령을 실행할 수 없습니다.".into(),
            remedy: Some("Docker Desktop을 설치하고 실행하세요.".into()),
        }
    }

    fn render(snap: &Snapshot) -> String {
        render_lines(&lines(snap, &[], &Theme::plain()))
    }

    #[test]
    fn an_empty_snapshot_waits_for_doctor() {
        assert_eq!(phase(&Snapshot::empty()), Phase::Checking);
        let text = render(&Snapshot::empty());
        assert!(text.contains("환경을 확인"));
        assert!(text.contains("다시 확인"));
        assert!(!text.contains("이 컴퓨터 등록"));
        assert!(!text.contains("첫 DB"));
    }

    #[test]
    fn docker_down_tells_the_user_to_start_the_daemon() {
        let mut snap = Snapshot::empty();
        snap.checks.push(docker_down());
        assert_eq!(phase(&snap), Phase::DockerDown);
        let text = render(&snap);
        assert!(text.contains("Docker Desktop"));
        assert!(text.contains("다시 확인"));
        assert!(!text.contains("이 컴퓨터 등록"));
        assert!(!text.contains("2`로 Targets"));
    }

    #[test]
    fn docker_ok_with_no_target_offers_local_registration() {
        let mut snap = Snapshot::empty();
        snap.checks.push(docker_ok());
        assert_eq!(phase(&snap), Phase::RegisterLocal);
        let text = render(&snap);
        assert!(text.contains("이 컴퓨터를 등록할까요"));
        assert!(text.contains("컨테이너는 아직"));
        assert!(!text.contains("Docker 연결"));
        assert!(!text.contains("첫 DB 또는 버킷"));
        assert!(!text.contains("1."));
        assert!(!text.contains("3."));
    }

    #[test]
    fn a_reachable_target_without_resources_opens_the_create_step() {
        let local = fixture::local_target();
        let mut snap = Snapshot::empty();
        snap.checks.push(docker_ok());
        snap.targets.push(crate::core::target::TargetOverview {
            target: local,
            reachable: true,
            docker: Some("27.1.1".into()),
            detail: "connected".into(),
        });
        assert_eq!(phase(&snap), Phase::CreateFirst);
        let text = render(&snap);
        assert!(text.contains("첫 DB 또는 버킷"));
        assert!(text.contains("프로젝트 이름만"));
        assert!(!text.contains("이 컴퓨터를 등록할까요"));
        assert!(!text.contains("`local` 등록됨"));
    }

    #[test]
    fn any_resource_ends_onboarding() {
        assert_eq!(phase(&fixture::snapshot()), Phase::Done);
    }

    #[test]
    fn notices_still_show_during_setup() {
        let mut snap = Snapshot::empty();
        snap.checks.push(docker_ok());
        let text = render_lines(&lines(
            &snap,
            &["키링을 사용할 수 없습니다".to_string()],
            &Theme::plain(),
        ));
        assert!(text.contains("키링을 사용할 수 없습니다"));
    }
}
