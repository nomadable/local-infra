//! The bottom hint bar and the help overlay (TUI-005, TUI-008, PRD §7.11).
//!
//! Keys come from the [`Keymap`] so a `[keymap]` override changes the hints
//! too; labels are chosen per context, because the same action means
//! something different on the tunnels screen than in a delete modal.
//!
//! Everything here is a pure function over plain data.

use crate::core::model::ResourceKind;
use crate::tui::keymap::{Action, Keymap, Screen};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    pub key: String,
    pub label: String,
    /// Bound action, so a click on the hint bar can run it. `None` for
    /// keys that are not in the keymap (`Ctrl+C` cancel, filter Enter).
    pub action: Option<crate::tui::keymap::Action>,
}
impl Hint {
    /// ` key label ` — the row the footer draws, gap included.
    ///
    /// Brackets around every key turn the footer into a wall of punctuation;
    /// the weight difference between a bold key and a plain label is enough.
    pub fn cols(&self) -> usize {
        let cols = crate::core::util::display_cols;
        cols(&self.key) + 1 + cols(&self.label)
    }
}

/// Which panel of a master-detail screen has the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Detail,
}

impl Focus {
    pub fn other(self) -> Self {
        match self {
            Focus::List => Focus::Detail,
            Focus::Detail => Focus::List,
        }
    }
}

/// Where the keyboard currently is. Determines which keys are legal, which is
/// exactly what the hint bar must show and nothing more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintContext {
    Screen {
        screen: Screen,
        focus: Focus,
        /// A row is selected; row-scoped actions are meaningful.
        selected: bool,
        /// Kind of the selected resource, when the screen has one. A bucket
        /// rotates a key, a database rotates a password (PRD §7.6).
        resource: Option<ResourceKind>,
        /// Work is in flight, so cancellation is offered first.
        busy: bool,
    },
    /// First-run guide: one primary action, no resource verbs.
    Onboard {
        phase: crate::tui::onboard::Phase,
        busy: bool,
    },
    /// The `/` filter line is open.
    Filter,
    /// The create form (PRD §7.5).
    Form,
    /// The SSH target form (PRD §7.4).
    SshForm,
    /// A destructive confirmation (PRD §7.9).
    Confirm {
        needs_typing: bool,
        armed: bool,
    },
    Palette,
    Help,
    /// A result or error box.
    Message {
        copy: bool,
    },
    /// Inspect popup opened from a list row.
    Inspect,
}

/// `Ctrl+C` is not a rebindable action: while work is in flight it always
/// trips the `Cancel` token (TUI-006).
pub const CANCEL_KEY: &str = "Ctrl+C";

/// Actions offered on each screen, in the order the PRD lists them.
fn screen_actions(
    screen: Screen,
    focus: Focus,
    selected: bool,
    resource: Option<ResourceKind>,
) -> Vec<(Action, &'static str)> {
    if focus == Focus::Detail {
        return vec![
            (Action::FocusNext, "목록으로"),
            (Action::Down, "스크롤"),
            (Action::Copy, "복사"),
            (Action::Refresh, "새로 고침"),
        ];
    }
    let rows: Vec<(Action, &'static str)> = match screen {
        // The two containers: lifecycle plus the logs you read when one of
        // them will not start.
        Screen::Engines => {
            if selected {
                vec![
                    (Action::Open, "상세"),
                    (Action::EngineRestart, "재시작"),
                    (Action::Logs, "로그"),
                    (Action::EngineEnsure, "다시 만들기"),
                    (Action::Delete, "삭제"),
                    (Action::Refresh, "새로 고침"),
                ]
            } else {
                vec![
                    (Action::EngineEnsure, "엔진 만들기"),
                    (Action::Refresh, "새로 고침"),
                ]
            }
        }

        Screen::Targets => {
            if selected {
                vec![
                    (Action::Add, "Target 추가"),
                    (Action::Open, "상세"),
                    (Action::Test, "접속 테스트"),
                    (Action::Logs, "엔진 로그"),
                    (Action::Delete, "등록 해제"),
                    (Action::Refresh, "새로 고침"),
                ]
            } else {
                vec![(Action::Add, "Target 추가"), (Action::Refresh, "새로 고침")]
            }
        }
        // PRD §7.6. A database rotates a password and is duplicated; a bucket
        // rotates its key and has no duplicate operation.
        Screen::Resources => {
            if !selected {
                return vec![
                    (Action::NewDatabase, "새 리소스"),
                    (Action::Refresh, "새로 고침"),
                ];
            }
            let bucket = resource == Some(ResourceKind::Bucket);
            let mut actions = vec![
                (Action::NewDatabase, "새 리소스"),
                (Action::Copy, "URL 복사"),
                (Action::CopyExpanded, "env 복사"),
                (Action::TunnelToggle, "터널"),
                (
                    Action::Test,
                    if bucket {
                        "접근 테스트"
                    } else {
                        "접속 테스트"
                    },
                ),
                (Action::Backup, "백업"),
                (Action::Restore, "복원"),
                (
                    Action::RotatePassword,
                    if bucket {
                        "키 교체"
                    } else {
                        "비밀번호 교체"
                    },
                ),
            ];
            if !bucket {
                actions.push((Action::Duplicate, "복제"));
            }
            actions.extend([
                (Action::Delete, "삭제"),
                (Action::Logs, "엔진 로그"),
                (Action::RevealSecret, "비밀 값 표시"),
                (Action::Filter, "필터"),
            ]);
            actions
        }
        // PRD §7.7. `s`/`S`/`r` are taken by reveal/refresh globally, so the
        // per-tunnel verbs hang off `t` (toggle) and `a` (all).
        Screen::Tunnels => {
            if selected {
                vec![
                    (Action::TunnelToggle, "시작/중지"),
                    (Action::Add, "모두 시작"),
                    (Action::Test, "상태 재확인"),
                    (Action::Delete, "기록 삭제"),
                    (Action::Refresh, "새로 고침"),
                ]
            } else {
                vec![(Action::Add, "모두 시작"), (Action::Refresh, "새로 고침")]
            }
        }
        Screen::Backups => {
            if selected {
                vec![
                    (Action::Restore, "복원"),
                    (Action::Test, "무결성 검증"),
                    (Action::Delete, "기록 삭제"),
                    (Action::Refresh, "새로 고침"),
                ]
            } else {
                vec![(Action::Refresh, "새로 고침")]
            }
        }
        // PRD §7.8: expand an entry, copy diagnostics.
        Screen::Activity => vec![
            (Action::Open, "펼치기"),
            (Action::CopyExpanded, "진단 복사"),
            (Action::Filter, "필터"),
            (Action::Refresh, "새로 고침"),
        ],
        Screen::Doctor => vec![(Action::Test, "진단 실행"), (Action::Refresh, "새로 고침")],
    };
    rows
}

const GLOBAL: [(Action, &str); 3] = [
    (Action::Palette, "명령"),
    (Action::Help, "도움말"),
    (Action::Quit, "종료"),
];

/// Keys valid right now, primary action first (PRD §14.2).
pub fn hints(keymap: &Keymap, context: HintContext) -> Vec<Hint> {
    let mut out = Vec::new();
    let push = |action: Action, label: &str, out: &mut Vec<Hint>| {
        if let Some(chord) = keymap.chord_for(action) {
            out.push(Hint {
                key: chord.to_string(),
                label: label.to_string(),
                action: Some(action),
            });
        }
    };

    match context {
        HintContext::Screen {
            screen,
            focus,
            selected,
            resource,
            busy,
        } => {
            if busy {
                out.push(Hint {
                    key: CANCEL_KEY.to_string(),
                    label: "실행 취소".to_string(),
                    action: None,
                });
            }
            out.push(Hint {
                key: "←/→".to_string(),
                label: "화면".to_string(),
                action: Some(Action::NextScreen),
            });

            for (action, label) in screen_actions(screen, focus, selected, resource) {
                push(action, label, &mut out);
            }
            for (action, label) in GLOBAL {
                push(action, label, &mut out);
            }
        }
        HintContext::Onboard { phase, busy } => {
            if busy {
                out.push(Hint {
                    key: CANCEL_KEY.to_string(),
                    label: "실행 취소".to_string(),
                    action: None,
                });
            }
            push(
                Action::Open,
                crate::tui::onboard::primary_label(phase),
                &mut out,
            );
            if matches!(
                phase,
                crate::tui::onboard::Phase::Checking | crate::tui::onboard::Phase::DockerDown
            ) {
                push(Action::Refresh, "새로 고침", &mut out);
            }
            for (action, label) in GLOBAL {
                push(action, label, &mut out);
            }
        }

        HintContext::Filter => {
            push(Action::Cancel, "필터 해제", &mut out);
            out.push(Hint {
                key: "Enter".to_string(),
                label: "적용".to_string(),
                action: None,
            });
        }
        HintContext::Form => {
            out.push(Hint {
                key: "Space".to_string(),
                label: "선택".to_string(),
                action: None,
            });
            out.push(Hint {
                key: "← →".to_string(),
                label: "질문".to_string(),
                action: None,
            });
            push(Action::Down, "항목", &mut out);
            push(Action::Submit, "실행", &mut out);
            push(Action::Cancel, "취소", &mut out);
        }
        HintContext::SshForm => {
            out.push(Hint {
                key: "Tab".to_string(),
                label: "다음 항목".to_string(),
                action: None,
            });
            push(Action::Submit, "호스트 키 조회", &mut out);
            push(Action::Cancel, "취소", &mut out);
        }

        HintContext::Confirm {
            needs_typing,
            armed,
        } => {
            push(
                Action::Submit,
                match (needs_typing, armed) {
                    (true, false) => "이름 입력 후 활성화",
                    _ => "확인",
                },
                &mut out,
            );
            push(Action::FocusNext, "취소/삭제 선택", &mut out);
            push(Action::Cancel, "취소", &mut out);
        }
        HintContext::Palette => {
            out.push(Hint {
                key: "Enter".to_string(),
                label: "실행".to_string(),
                action: None,
            });
            push(Action::Down, "다음", &mut out);
            push(Action::Up, "이전", &mut out);
            push(Action::Cancel, "닫기", &mut out);
        }
        HintContext::Help => {
            push(Action::Cancel, "닫기", &mut out);
            push(Action::Down, "스크롤", &mut out);
        }
        HintContext::Message { copy } => {
            if copy {
                push(Action::Copy, "URL 복사", &mut out);
                push(Action::CopyExpanded, "env 복사", &mut out);
            }
            push(Action::Cancel, "닫기", &mut out);
            push(Action::Down, "스크롤", &mut out);
        }
        HintContext::Inspect => {
            push(Action::Cancel, "닫기", &mut out);
            push(Action::Down, "스크롤", &mut out);
        }
    }
    out
}

/// How many whole hints fit in `width`, and whether anything was dropped.
/// Entries are never cut in half: half a key hint is worse than no hint.
pub fn fit_count(hints: &[Hint], width: usize) -> (usize, bool) {
    let mut used = 0usize;
    for (i, hint) in hints.iter().enumerate() {
        let piece = hint.cols();
        let gap = if i == 0 { 0 } else { 2 };
        if used + gap + piece > width {
            return (i, true);
        }
        used += gap + piece;
    }
    (hints.len(), false)
}

/// Split hints across at most `max_rows` rows of `width` columns, and say
/// whether anything still had to be dropped.
///
/// The resources screen has ten verbs (PRD §7.6 draws them on two lines), and
/// on 80 columns they do not fit on one row. Wrapping beats hiding them.
pub fn wrap(hints: &[Hint], width: usize, max_rows: usize) -> (Vec<Vec<Hint>>, bool) {
    let mut rows: Vec<Vec<Hint>> = Vec::new();
    let mut rest = hints;
    while !rest.is_empty() && rows.len() < max_rows.max(1) {
        let (fits, _) = fit_count(rest, width);
        // Always consume one entry, so a single over-wide hint cannot loop.
        let take = fits.clamp(1, rest.len());
        rows.push(rest[..take].to_vec());
        rest = &rest[take..];
    }
    (rows, !rest.is_empty())
}

/// Render hints into one row. `ellipsis` marks that something was dropped.
pub fn fit(hints: &[Hint], width: usize, ellipsis: &str) -> String {
    let cols = crate::core::util::display_cols;
    let (shown, truncated) = fit_count(hints, width);
    let mut out = String::new();
    for hint in &hints[..shown] {
        if !out.is_empty() {
            out.push_str("  ");
        }
        out.push_str(&format!("{} {}", hint.key, hint.label));
    }

    if truncated && width.saturating_sub(cols(&out)) > cols(ellipsis) {
        out.push(' ');
        out.push_str(ellipsis);
    }
    out
}

/// Two-column help rows, `?` overlay (TUI-008). The keymap owns the content;
/// this only groups it into columns that fit the overlay.
pub fn help_columns(keymap: &Keymap, columns: usize) -> Vec<Vec<(String, String)>> {
    let rows = keymap.help_rows();
    let columns = columns.max(1);
    let per = rows.len().div_ceil(columns);
    rows.chunks(per.max(1)).map(<[_]>::to_vec).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keymap() -> Keymap {
        Keymap::defaults()
    }

    fn screen(screen: Screen) -> HintContext {
        on(screen, Some(ResourceKind::Database))
    }

    fn on(screen: Screen, resource: Option<ResourceKind>) -> HintContext {
        HintContext::Screen {
            screen,
            focus: Focus::List,
            selected: true,
            resource,
            busy: false,
        }
    }

    fn keys(hints: &[Hint]) -> Vec<&str> {
        hints.iter().map(|h| h.key.as_str()).collect()
    }

    fn labels(hints: &[Hint]) -> Vec<&str> {
        hints.iter().map(|h| h.label.as_str()).collect()
    }

    #[test]
    fn the_engines_bar_offers_container_lifecycle_not_resource_verbs() {
        let km = keymap();
        let hints = hints(&km, screen(Screen::Engines));
        let labels = labels(&hints);
        assert_eq!(keys(&hints)[0], "←/→");
        assert!(labels.contains(&"재시작"), "{labels:?}");
        assert!(labels.contains(&"로그"), "{labels:?}");
        assert!(labels.contains(&"다시 만들기"), "{labels:?}");
        assert!(
            !labels.contains(&"URL 복사"),
            "a container has no connection URL: {labels:?}"
        );
    }

    #[test]
    fn the_resources_bar_leads_with_creating_and_copying() {
        let km = keymap();
        let hints = hints(&km, screen(Screen::Resources));
        let labels = labels(&hints);
        assert_eq!(labels[0], "화면");
        assert_eq!(labels[1], "새 리소스", "{labels:?}");
        assert!(labels.contains(&"URL 복사"), "{labels:?}");
        assert!(labels.contains(&"env 복사"), "{labels:?}");
    }

    #[test]
    fn the_setup_bar_offers_the_next_step_first() {
        let km = keymap();
        let hints = hints(
            &km,
            HintContext::Onboard {
                phase: crate::tui::onboard::Phase::RegisterLocal,
                busy: false,
            },
        );
        assert_eq!(keys(&hints)[0], "Enter");
        assert_eq!(labels(&hints)[0], "이 컴퓨터 등록");
        assert!(!labels(&hints).contains(&"새 리소스"));
        assert!(!labels(&hints).contains(&"터널"));
        assert!(!labels(&hints).contains(&"새로 고침"));
    }

    #[test]
    fn the_resources_bar_follows_the_prd_verb_order_for_a_database() {
        let km = keymap();
        let hints = hints(&km, screen(Screen::Resources));
        let expected = [
            "URL 복사",
            "env 복사",
            "터널",
            "접속 테스트",
            "백업",
            "복원",
            "비밀번호 교체",
            "복제",
            "삭제",
            "엔진 로그",
        ];
        let found: Vec<&str> = labels(&hints)
            .into_iter()
            .filter(|label| expected.contains(label))
            .collect();
        assert_eq!(found, expected);
    }

    #[test]
    fn a_selected_bucket_rotates_a_key_and_cannot_be_duplicated() {
        let km = keymap();
        let hints = hints(&km, on(Screen::Resources, Some(ResourceKind::Bucket)));
        let labels = labels(&hints);
        assert!(labels.contains(&"키 교체"));
        assert!(labels.contains(&"접근 테스트"));
        assert!(!labels.contains(&"비밀번호 교체"));
        assert!(!labels.contains(&"복제"));
    }

    #[test]
    fn row_scoped_keys_disappear_when_nothing_is_selected() {
        let km = keymap();
        let empty = HintContext::Screen {
            screen: Screen::Resources,
            focus: Focus::List,
            selected: false,
            resource: None,
            busy: false,
        };
        let hints = hints(&km, empty);
        assert!(!labels(&hints).contains(&"삭제"));
        assert!(labels(&hints).contains(&"새 리소스"));
    }

    #[test]
    fn work_in_flight_offers_ctrl_c_first() {
        let km = keymap();
        let busy = HintContext::Screen {
            screen: Screen::Resources,
            focus: Focus::List,
            selected: true,
            resource: Some(ResourceKind::Database),
            busy: true,
        };
        let hints = hints(&km, busy);
        assert_eq!(hints[0].key, CANCEL_KEY);
        assert_eq!(hints[0].label, "실행 취소");
    }

    #[test]
    fn detail_focus_offers_scrolling_not_row_verbs() {
        let km = keymap();
        let hints = hints(
            &km,
            HintContext::Screen {
                screen: Screen::Resources,
                focus: Focus::Detail,
                selected: true,
                resource: Some(ResourceKind::Database),
                busy: false,
            },
        );
        assert!(labels(&hints).contains(&"스크롤"));
        assert!(!labels(&hints).contains(&"삭제"));
    }

    #[test]
    fn a_delete_modal_says_the_submit_key_is_inert_until_the_name_matches() {
        let km = keymap();
        let locked = hints(
            &km,
            HintContext::Confirm {
                needs_typing: true,
                armed: false,
            },
        );
        assert_eq!(locked[0].label, "이름 입력 후 활성화");

        let armed = hints(
            &km,
            HintContext::Confirm {
                needs_typing: true,
                armed: true,
            },
        );
        assert_eq!(armed[0].label, "확인");
    }

    #[test]
    fn the_form_bar_matches_the_prd_footer() {
        let km = keymap();
        let hints = hints(&km, HintContext::Form);
        assert_eq!(keys(&hints)[0], "Space");
        assert_eq!(labels(&hints)[0], "선택");
        assert!(labels(&hints).contains(&"질문"));
        assert!(labels(&hints).contains(&"실행"));
        assert!(labels(&hints).contains(&"취소"));
    }

    #[test]
    fn rebinding_an_action_rebinds_its_hint() {
        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert("refresh".to_string(), "F".to_string());
        let (km, problems) = Keymap::defaults().with_overrides(&overrides);
        assert!(problems.is_empty());
        let hints = hints(&km, screen(Screen::Engines));
        assert!(keys(&hints).contains(&"F"));
        assert!(!keys(&hints).contains(&"r"));
    }

    #[test]
    fn fitting_drops_whole_entries_and_marks_the_cut() {
        let hints = vec![
            Hint {
                key: "n".into(),
                label: "new".into(),
                action: None,
            },
            Hint {
                key: "t".into(),
                label: "tunnel".into(),
                action: None,
            },
            Hint {
                key: "q".into(),
                label: "quit".into(),
                action: None,
            },
        ];
        assert_eq!(fit(&hints, 80, "…"), "n new  t tunnel  q quit");

        let cut = fit(&hints, 14, "…");
        assert!(cut.starts_with("n new"), "{cut}");
        assert!(cut.ends_with('…'));
        assert!(crate::core::util::display_cols(&cut) <= 14);
    }

    #[test]
    fn fitting_never_exceeds_the_width_even_for_one_long_entry() {
        let hints = vec![Hint {
            key: "Ctrl+S".into(),
            label: "아주 긴 라벨입니다".into(),
            action: None,
        }];
        let text = fit(&hints, 6, "…");
        assert!(crate::core::util::display_cols(&text) <= 6);
    }

    #[test]
    fn help_columns_cover_every_binding_exactly_once() {
        let km = keymap();
        let total = km.help_rows().len();
        let columns = help_columns(&km, 2);
        assert!(columns.len() <= 2);
        assert_eq!(columns.iter().map(Vec::len).sum::<usize>(), total);
    }
}
