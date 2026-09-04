//! Pure text building: tables, detail panes and the dashboard.
//!
//! Nothing here touches a terminal, a store or Docker. Every function takes
//! plain data and returns strings or [`Line`]s, which is what makes the
//! column order, the masking rules and the dashboard contents assertable in
//! unit tests (PRD §7.3, §7.6, §7.7, §7.8, §11.1).

use crate::core::config::Config;
use crate::core::model::{EngineStatus, Health, ResourceKind, TunnelStatus};
use crate::core::util;
use crate::tui::data::{Resource, Snapshot};
use crate::tui::theme::Theme;
use chrono::{DateTime, Local, Utc};
use ratatui::text::{Line, Span};

/// Fixed-width mask. The *length* of a secret is information too, so the mask
/// never mirrors it (PRD §11.1).
pub const MASK: &str = "****";
/// Shown where a value exists on the server but was never persisted locally
/// (restricted secret mode).
pub const NO_SECRET: &str = "(저장 안 됨)";
/// Empty cell filler, so a blank never reads as "zero".
pub const EMPTY: &str = "-";

pub fn mask(value: Option<&str>, revealed: bool) -> String {
    match value {
        None => NO_SECRET.to_string(),
        Some(_) if !revealed => MASK.to_string(),
        Some(v) => v.to_string(),
    }
}

/// Pad `text` to `cols` *terminal columns*.
///
/// `format!("{:<12}")` pads by `char` count, which is wrong for Korean: `엔진`
/// is two chars and four columns, so every mixed-script table drifts. Every
/// aligned label in the TUI goes through here instead.
pub fn pad(text: &str, cols: usize) -> String {
    let width = util::display_cols(text);
    let mut out = String::with_capacity(text.len() + cols);
    out.push_str(text);
    for _ in 0..cols.saturating_sub(width) {
        out.push(' ');
    }
    out
}

/// Pad, and cut with an ellipsis when the text is wider than the column.
pub fn fit_cols(text: &str, cols: usize) -> String {
    if util::display_cols(text) > cols {
        return util::truncate_cols(text, cols);
    }
    pad(text, cols)
}

fn stamp(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

fn clock(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local).format("%H:%M").to_string()
}

fn optional_stamp(at: Option<DateTime<Utc>>) -> String {
    at.map(stamp).unwrap_or_else(|| EMPTY.to_string())
}

fn bytes(value: Option<u64>) -> String {
    value
        .map(util::human_bytes)
        .unwrap_or_else(|| EMPTY.to_string())
}

fn count(value: Option<u64>) -> String {
    value
        .map(|v| v.to_string())
        .unwrap_or_else(|| EMPTY.to_string())
}

// ---------------------------------------------------------------------------
// Status and navigation bars
// ---------------------------------------------------------------------------

/// The active-engine card is short, concrete and useful: the service, its
/// reachable port and how much project data it holds. It does not repeat the
/// full status banner or turn the two services into a dashboard of metrics.
pub fn engine_strip(snap: &Snapshot, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    use crate::core::model::EngineKind;

    let engine_of = |kind: EngineKind| snap.engines.iter().find(|o| o.engine.engine == kind);
    let count = |kind: ResourceKind| snap.resources.iter().filter(|r| r.kind() == kind).count();
    let line = |kind: EngineKind, label: &str, resource_kind: ResourceKind, noun: &str| {
        let (state, port, style) = match engine_of(kind) {
            Some(o) if o.status.running => (
                "running".to_string(),
                format!("PORT:{}", o.engine.host_port),
                theme.engine_style(&o.status),
            ),
            Some(o) if o.status.exists => ("stopped".to_string(), "-".to_string(), theme.warn()),
            Some(_) | None => ("없음".to_string(), "-".to_string(), theme.muted()),
        };
        let major = engine_of(kind)
            .map(|o| o.engine.major_version.as_str())
            .unwrap_or("-");
        let left = format!("• {label} {major} ({state} on {port})");
        let right = format!("{} {noun}", count(resource_kind));
        let gap = width
            .saturating_sub(util::display_cols(&left) as u16)
            .saturating_sub(util::display_cols(&right) as u16)
            .max(2) as usize;
        Line::from(vec![
            Span::styled(left, style),
            Span::raw(" ".repeat(gap)),
            Span::styled(right, theme.heading()),
        ])
    };

    vec![
        line(
            EngineKind::Postgres,
            "Postgres",
            ResourceKind::Database,
            "DBs",
        ),
        line(EngineKind::Minio, "MinIO", ResourceKind::Bucket, "Bucket"),
    ]
}

/// `Resources · Engines · Backups · Log  ·  Targets · Tunnels · Doctor`.
///
/// The four everyday screens come first and the three occasional ones sit
/// after a wider gap, so the eye lands in the working set without reading
/// the whole strip.
pub fn nav_line(current: crate::tui::keymap::Screen, theme: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    let mut previous_primary = true;
    for (i, screen) in crate::tui::keymap::Screen::ALL.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                nav_gap(previous_primary, screen),
                theme.muted(),
            ));
        }
        // Reversed *and* bold: the current tab still reads on a terminal that
        // renders reverse video weakly.
        let style = if screen == current {
            theme.selected()
        } else {
            theme.muted()
        };
        spans.push(Span::styled(format!(" {} ", screen.title()), style));
        previous_primary = screen.primary();
    }
    Line::from(spans)
}

/// The gap that precedes `screen`: wide where the everyday screens end.
fn nav_gap(previous_primary: bool, screen: crate::tui::keymap::Screen) -> String {
    if previous_primary && !screen.primary() {
        "   ".to_string()
    } else {
        String::new()
    }
}

/// Column and width of one tab, so a click lands on what the user saw.
pub fn nav_tab_offset(screen: crate::tui::keymap::Screen) -> (u16, u16) {
    let mut x = 0u16;
    let mut previous_primary = true;
    for (i, candidate) in crate::tui::keymap::Screen::ALL.into_iter().enumerate() {
        if i > 0 {
            x = x.saturating_add(crate::core::util::display_cols(&nav_gap(
                previous_primary,
                candidate,
            )) as u16);
        }
        let width = crate::core::util::display_cols(candidate.title()) as u16 + 2;
        if candidate == screen {
            return (x, width);
        }
        x = x.saturating_add(width);
        previous_primary = candidate.primary();
    }
    (0, 0)
}
/// Display width of the navigation row. Mouse targets use this to share the
/// exact centering that the renderer applies to the `Paragraph`.
pub fn nav_line_width() -> u16 {
    let last = *crate::tui::keymap::Screen::ALL
        .last()
        .expect("screen list is not empty");
    let (offset, width) = nav_tab_offset(last);
    offset.saturating_add(width)
}

pub fn screen_label(screen: crate::tui::keymap::Screen) -> &'static str {
    screen.title()
}

// ---------------------------------------------------------------------------
// Resources (PRD §7.6, plus the kind column object storage needs)
// ---------------------------------------------------------------------------

/// The column that matters most is the one the user copies into a `.env`, so
/// `CONNECT` sits in the middle of the row rather than at the far edge, and
/// the target — nearly always `local` for this product — comes last.
pub const RESOURCE_COLUMNS: [&str; 6] = ["NAME", "KIND", "ENGINE", "SIZE", "CONNECT", "WHERE"];

pub const ENGINE_COLUMNS: [&str; 6] = ["ENGINE", "CONTAINER", "STATE", "BIND", "HOLDS", "WHERE"];

/// Short kind token used in the table (PRD §7.6 mock): `db` / `bucket`.
pub fn kind_cell(kind: ResourceKind) -> &'static str {
    match kind {
        ResourceKind::Database => "db",
        ResourceKind::Bucket => "bucket",
    }
}

/// An access key id is not a secret, but it identifies an account, so only its
/// prefix is shown (PRD §7.6: "접두부만 남기고 마스킹").
pub fn mask_access_key(key: &str, revealed: bool) -> String {
    if revealed {
        return key.to_string();
    }
    const KEEP: usize = 4;
    let prefix: String = key.chars().take(KEEP).collect();
    let hidden = key.chars().count().saturating_sub(prefix.chars().count());
    format!("{prefix}{}", "*".repeat(hidden))
}

/// What the user would paste, and why they cannot yet when that is the case.
///
/// Locally this is just the published port. Remotely it is the tunnel's local
/// port when one is up, and the reason it is not otherwise — a bare `-` here
/// would read as "no port needed" on a resource that very much needs one.
fn connect_cell(resource: &Resource, theme: &Theme) -> String {
    match resource.tunnel() {
        Some(session) if session.status == TunnelStatus::Active => format!(
            ":{} {}",
            session.local_port,
            theme.tunnel_symbol(TunnelStatus::Active)
        ),
        _ if resource.target().is_remote() => "터널 필요".to_string(),
        _ => format!(":{}", resource.engine().host_port),
    }
}

pub fn resource_rows(snap: &Snapshot, theme: &Theme) -> Vec<Vec<String>> {
    snap.resources
        .iter()
        .map(|r| {
            vec![
                r.name().to_string(),
                kind_cell(r.kind()).to_string(),
                r.engine().label(),
                bytes(r.size_bytes()),
                connect_cell(r, theme),
                r.target().display_name.clone(),
            ]
        })
        .collect()
}

/// The two containers, as rows. `HOLDS` is what makes the screen worth a
/// keystroke: it says how much of the user's work is inside each one.
pub fn engine_rows(snap: &Snapshot, theme: &Theme) -> Vec<Vec<String>> {
    snap.engines
        .iter()
        .map(|o| {
            let holds = snap.resource_count(&o.engine.id);
            vec![
                o.engine.label(),
                o.engine.container_name.clone(),
                format!("{} {}", theme.engine_symbol(&o.status), o.status.state),
                format!("{}:{}", o.engine.bind_address, o.engine.host_port),
                match o.engine.engine.resource_kind() {
                    ResourceKind::Database => format!("{holds} db"),
                    ResourceKind::Bucket => format!("{holds} bucket"),
                },
                o.target.display_name.clone(),
            ]
        })
        .collect()
}

/// Connection facts for the detail pane, resolved by the caller so this
/// module never reaches into the secret store.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Endpoint {
    /// Credential-bearing form, shown only while `s` is held open.
    pub url: Option<String>,
    pub redacted_url: Option<String>,
    pub env_block: Option<String>,
    /// Password or secret access key. Fully masked until revealed.
    pub secret: Option<String>,
    /// `http://127.0.0.1:19000` — the S3 endpoint every SDK asks for.
    pub address: Option<String>,
    pub region: Option<String>,
    /// Why there is no URL — typically "start the tunnel first" (TUN-006).
    pub note: Option<String>,
}

fn field(label: &str, value: String, theme: &Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<11}"), theme.muted()),
        Span::styled(value, theme.normal()),
    ])
}

/// PRD §7.6. A database and a bucket answer different questions, so the pane
/// is not one table with renamed rows: the shared head and tail are the same,
/// the middle is per service.
pub fn resource_detail_lines(
    resource: &Resource,
    status: Option<&EngineStatus>,
    endpoint: &Endpoint,
    revealed: bool,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled(resource.name().to_string(), theme.heading()),
            Span::styled(
                format!(
                    "   {} · {}",
                    kind_cell(resource.kind()),
                    resource.engine().label()
                ),
                theme.muted(),
            ),
        ]),
        field(
            "target",
            format!(
                "{} ({})",
                resource.target().display_name,
                resource.target().location()
            ),
            theme,
        ),
        field(
            "engine",
            match status {
                Some(s) => format!(
                    "{} · {} · {}",
                    resource.engine().container_name,
                    s.state,
                    health_word(s.health)
                ),
                None => resource.engine().container_name.clone(),
            },
            theme,
        ),
    ];

    match resource.kind() {
        ResourceKind::Database => {
            lines.push(field("owner", resource.principal().to_string(), theme));
            lines.push(url_line("url", endpoint, revealed, theme));
            lines.push(field(
                "password",
                mask(endpoint.secret.as_deref(), revealed),
                theme,
            ));
            lines.push(field(
                "size",
                format!(
                    "{}   connections {}",
                    bytes(resource.size_bytes()),
                    count(resource.usage())
                ),
                theme,
            ));
        }
        ResourceKind::Bucket => {
            lines.push(endpoint_line(resource, endpoint, theme));
            lines.push(field(
                "bucket",
                format!(
                    "{}   region {}",
                    resource.name(),
                    endpoint.region.as_deref().unwrap_or(EMPTY)
                ),
                theme,
            ));
            lines.push(field(
                "access key",
                format!(
                    "{}   policy {}",
                    mask_access_key(resource.principal(), revealed),
                    crate::core::minio::policy_name(resource.name())
                ),
                theme,
            ));
            lines.push(field(
                "secret key",
                mask(endpoint.secret.as_deref(), revealed),
                theme,
            ));
            lines.push(field(
                "objects",
                format!(
                    "{}개 · {}",
                    count(resource.usage()),
                    bytes(resource.size_bytes())
                ),
                theme,
            ));
            lines.push(url_line("url", endpoint, revealed, theme));
        }
    }

    lines.push(field(
        "created",
        format!(
            "{}   last backup  {}",
            stamp(resource.created_at()),
            optional_stamp(resource.last_backup_at())
        ),
        theme,
    ));
    lines.push(tunnel_line(resource, theme));
    lines
}

/// The credential-bearing URL, redacted unless `s` is holding it open, or the
/// reason there is none (TUN-006).
fn url_line(label: &str, endpoint: &Endpoint, revealed: bool, theme: &Theme) -> Line<'static> {
    let url = if revealed {
        endpoint
            .url
            .clone()
            .or_else(|| endpoint.redacted_url.clone())
    } else {
        endpoint.redacted_url.clone()
    };
    match (url, &endpoint.note) {
        (Some(url), _) => field(label, url, theme),
        (None, Some(note)) => Line::from(vec![
            Span::styled(format!("{label:<11}"), theme.muted()),
            Span::styled(note.clone(), theme.warn()),
        ]),
        (None, None) => field(label, EMPTY.to_string(), theme),
    }
}

/// `http://127.0.0.1:19000  (터널)      console :9001` (PRD §7.6 bucket mock).
fn endpoint_line(resource: &Resource, endpoint: &Endpoint, theme: &Theme) -> Line<'static> {
    let mut value = match &endpoint.address {
        Some(address) => address.clone(),
        None => EMPTY.to_string(),
    };
    if resource.target().is_remote() && resource.tunnel().is_some() {
        value.push_str("  (터널)");
    }
    if let Some(console) = resource.engine().console_port {
        value.push_str(&format!("      console :{console}"));
    }
    field("endpoint", value, theme)
}

fn tunnel_line(resource: &Resource, theme: &Theme) -> Line<'static> {
    match resource.tunnel() {
        Some(s) => Line::from(vec![
            Span::styled(format!("{:<11}", "tunnel"), theme.muted()),
            Span::styled(
                format!("{} {}", theme.tunnel_symbol(s.status), s.status.as_str()),
                theme.tunnel_style(s.status),
            ),
            Span::styled(
                format!(
                    " · {}:{} → {} · pid {}",
                    s.local_host,
                    s.local_port,
                    s.remote_port,
                    s.pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into())
                ),
                theme.normal(),
            ),
        ]),
        None if resource.target().is_remote() => field(
            "tunnel",
            format!(
                "없음 · 예약 포트 {}",
                resource
                    .preferred_tunnel_port()
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| EMPTY.into())
            ),
            theme,
        ),
        None => field("tunnel", "해당 없음 (local)".to_string(), theme),
    }
}

fn health_word(health: Health) -> &'static str {
    match health {
        Health::Healthy => "healthy",
        Health::Unhealthy => "unhealthy",
        Health::Starting => "starting",
        Health::None => "no healthcheck",
    }
}

// ---------------------------------------------------------------------------
// Targets
// ---------------------------------------------------------------------------

pub const TARGET_COLUMNS: [&str; 5] = ["NAME", "KIND", "LOCATION", "DOCKER", "STATE"];

pub fn target_rows(snap: &Snapshot, _theme: &Theme) -> Vec<Vec<String>> {
    snap.targets
        .iter()
        .map(|o| {
            vec![
                o.target.display_name.clone(),
                o.target.kind.as_str().to_string(),
                o.target.location(),
                o.docker.clone().unwrap_or_else(|| EMPTY.to_string()),
                if o.reachable {
                    "connected".to_string()
                } else {
                    o.detail.clone()
                },
            ]
        })
        .collect()
}

pub fn target_detail_lines(snap: &Snapshot, index: usize, theme: &Theme) -> Vec<Line<'static>> {
    let Some(overview) = snap.targets.get(index) else {
        return vec![Line::from(Span::styled(
            "등록된 Target이 없습니다. `a`로 추가하세요.".to_string(),
            theme.muted(),
        ))];
    };
    let t = &overview.target;
    let mut lines = vec![
        Line::from(Span::styled(t.display_name.clone(), theme.heading())),
        field("kind", t.kind.as_str().to_string(), theme),
        field("location", t.location(), theme),
        field("docker", t.docker_command.clone(), theme),
        field(
            "engine ver",
            overview.docker.clone().unwrap_or_else(|| EMPTY.to_string()),
            theme,
        ),
        field(
            "host key",
            t.host_key_fingerprint
                .clone()
                .unwrap_or_else(|| "해당 없음".to_string()),
            theme,
        ),
        field("created", stamp(t.created_at), theme),
        field("last seen", optional_stamp(t.last_connected_at), theme),
        Line::from(vec![
            Span::styled(format!("{:<11}", "state"), theme.muted()),
            Span::styled(
                overview.detail.clone(),
                if overview.reachable {
                    theme.ok()
                } else {
                    theme.danger()
                },
            ),
        ]),
        Line::raw(String::new()),
        Line::from(Span::styled("ENGINES".to_string(), theme.heading())),
    ];

    let engines = snap.engines_for_target(&t.id);
    if engines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  등록된 엔진이 없습니다.".to_string(),
            theme.muted(),
        )));
    }
    for overview in engines {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {} ", theme.engine_symbol(&overview.status)),
                theme.engine_style(&overview.status),
            ),
            Span::styled(format!("{:<16}", overview.engine.label()), theme.normal()),
            Span::styled(
                format!(
                    "{:<10} {} {}",
                    overview.status.state,
                    snap.resource_count(&overview.engine.id),
                    kind_cell(overview.engine.engine.resource_kind())
                ),
                theme.muted(),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            format!(
                "     {} · {}:{}",
                overview.engine.container_name,
                overview.engine.bind_address,
                overview.engine.host_port
            ),
            theme.muted(),
        )));
    }

    let foreign: Vec<_> = snap
        .foreign
        .iter()
        .filter(|(target_id, _)| target_id == &t.id)
        .collect();
    if !foreign.is_empty() {
        lines.push(Line::raw(String::new()));
        lines.push(Line::from(Span::styled(
            "UNMANAGED (읽기 전용)".to_string(),
            theme.heading(),
        )));
        for (_, container) in foreign {
            lines.push(Line::from(Span::styled(
                format!(
                    "  {} · {} · {}",
                    container.name, container.image, container.state
                ),
                theme.muted(),
            )));
        }
    }
    lines
}

// ---------------------------------------------------------------------------
// Tunnels (PRD §7.7)
// ---------------------------------------------------------------------------

pub const TUNNEL_COLUMNS: [&str; 8] = [
    "RESOURCE", "KIND", "LOCAL", "REMOTE", "TARGET", "PID", "SINCE", "STATE",
];

pub fn tunnel_rows(snap: &Snapshot, theme: &Theme) -> Vec<Vec<String>> {
    snap.tunnels
        .iter()
        .map(|v| {
            let s = &v.session;
            vec![
                v.resource_name.clone(),
                v.resource_kind.as_str().to_string(),
                format!("{}:{}", s.local_host, s.local_port),
                s.remote_port.to_string(),
                v.target_name.clone(),
                s.pid.map(|p| p.to_string()).unwrap_or_else(|| EMPTY.into()),
                clock(s.started_at),
                format!("{} {}", theme.tunnel_symbol(s.status), s.status.as_str()),
            ]
        })
        .collect()
}

pub fn tunnel_detail_lines(snap: &Snapshot, index: usize, theme: &Theme) -> Vec<Line<'static>> {
    let Some(view) = snap.tunnels.get(index) else {
        return vec![Line::from(Span::styled(
            "터널 기록이 없습니다. Resources 화면에서 `t`로 시작하세요.".to_string(),
            theme.muted(),
        ))];
    };
    let s = &view.session;
    vec![
        Line::from(Span::styled(view.resource_name.clone(), theme.heading())),
        field("kind", view.resource_kind.as_str().to_string(), theme),
        field("target", view.target_name.clone(), theme),
        field("local", format!("{}:{}", s.local_host, s.local_port), theme),
        field(
            "remote",
            format!("{}:{}", s.remote_host, s.remote_port),
            theme,
        ),
        field(
            "pid",
            s.pid.map(|p| p.to_string()).unwrap_or_else(|| EMPTY.into()),
            theme,
        ),
        field("started", stamp(s.started_at), theme),
        field("stopped", optional_stamp(s.stopped_at), theme),
        Line::from(vec![
            Span::styled(format!("{:<11}", "state"), theme.muted()),
            Span::styled(
                format!("{} {}", theme.tunnel_symbol(s.status), s.status.as_str()),
                theme.tunnel_style(s.status),
            ),
        ]),
        field("pid file", s.pid_file_path.clone(), theme),
    ]
}

// ---------------------------------------------------------------------------
// Backups
// ---------------------------------------------------------------------------

pub const BACKUP_COLUMNS: [&str; 6] = ["CREATED", "RESOURCE", "FORMAT", "SIZE", "STATUS", "FILE"];

pub fn backup_rows(snap: &Snapshot, _theme: &Theme) -> Vec<Vec<String>> {
    snap.backups
        .iter()
        .map(|b| {
            vec![
                stamp(b.created_at),
                snap.resource_name(&b.resource_id)
                    .unwrap_or(&b.resource_id)
                    .to_string(),
                b.format.as_str().to_string(),
                util::human_bytes(b.size),
                b.status.as_str().to_string(),
                b.file_name.clone(),
            ]
        })
        .collect()
}

pub fn backup_detail_lines(snap: &Snapshot, index: usize, theme: &Theme) -> Vec<Line<'static>> {
    let Some(record) = snap.backups.get(index) else {
        return vec![Line::from(Span::styled(
            "백업 기록이 없습니다. Resources 화면에서 `b`로 백업하세요.".to_string(),
            theme.muted(),
        ))];
    };
    vec![
        Line::from(Span::styled(record.file_name.clone(), theme.heading())),
        field(
            "resource",
            format!(
                "{} ({})",
                snap.resource_name(&record.resource_id)
                    .unwrap_or(&record.resource_id),
                record.resource_kind.as_str()
            ),
            theme,
        ),
        field("format", record.format.as_str().to_string(), theme),
        field("size", util::human_bytes(record.size), theme),
        field("created", stamp(record.created_at), theme),
        field("directory", record.storage_location.clone(), theme),
        field("checksum", record.checksum.clone(), theme),
        Line::from(vec![
            Span::styled(format!("{:<11}", "status"), theme.muted()),
            Span::styled(
                record.status.as_str().to_string(),
                match record.status {
                    crate::core::model::BackupStatus::Ok => theme.ok(),
                    crate::core::model::BackupStatus::Failed => theme.danger(),
                    crate::core::model::BackupStatus::Running => theme.warn(),
                },
            ),
        ]),
    ]
}

// ---------------------------------------------------------------------------
// Activity log (PRD §7.8)
// ---------------------------------------------------------------------------

pub const ACTIVITY_COLUMNS: [&str; 5] = ["TIME", "TARGET", "RESOURCE", "ACTION", "RESULT"];

pub fn activity_rows(snap: &Snapshot, theme: &Theme) -> Vec<Vec<String>> {
    let target_name = |id: Option<&String>| -> String {
        let Some(id) = id else {
            return EMPTY.to_string();
        };
        snap.targets
            .iter()
            .find(|t| &t.target.id == id)
            .map(|t| t.target.display_name.clone())
            .unwrap_or_else(|| id.clone())
    };
    snap.activity
        .iter()
        .map(|a| {
            let resource = a
                .resource_id
                .as_deref()
                .and_then(|id| snap.resource_name(id))
                .map(str::to_string)
                .unwrap_or_else(|| a.resource_type.clone());
            vec![
                stamp(a.started_at),
                target_name(a.target_id.as_ref()),
                resource,
                a.action.clone(),
                format!("{} {}", theme.activity_symbol(a.status), a.status.as_str()),
            ]
        })
        .collect()
}

/// The expanded form of one entry: the steps that ran and whether anything was
/// rolled back. Secrets are already redacted by `core` (PRD §7.8).
pub fn activity_detail_lines(snap: &Snapshot, index: usize, theme: &Theme) -> Vec<Line<'static>> {
    let Some(record) = snap.activity.get(index) else {
        return vec![Line::from(Span::styled(
            "기록이 없습니다.".to_string(),
            theme.muted(),
        ))];
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{} ", theme.activity_symbol(record.status)),
                theme.activity_style(record.status),
            ),
            Span::styled(
                format!("{} {}", record.resource_type, record.action),
                theme.heading(),
            ),
        ]),
        field("summary", record.redacted_summary.clone(), theme),
        field("origin", record.origin.as_str().to_string(), theme),
        field("started", stamp(record.started_at), theme),
        field("completed", optional_stamp(record.completed_at), theme),
        Line::raw(String::new()),
        Line::from(Span::styled("STEPS".to_string(), theme.heading())),
    ];
    if record.steps.is_empty() {
        lines.push(Line::from(Span::styled(
            "  기록된 단계가 없습니다.".to_string(),
            theme.muted(),
        )));
    }
    for (i, step) in record.steps.iter().enumerate() {
        lines.push(Line::from(Span::styled(
            format!("  {:>2}. {step}", i + 1),
            theme.normal(),
        )));
    }
    lines
}

/// Diagnostics text for `Y` on the log screen: pasteable into an issue, with
/// no secrets because the records never held any.
pub fn activity_diagnostics(snap: &Snapshot, index: usize) -> String {
    let Some(record) = snap.activity.get(index) else {
        return String::new();
    };
    let mut out = format!(
        "local-infra activity\n{} {} · {} · {}\n{}\n",
        record.resource_type,
        record.action,
        record.origin.as_str(),
        record.status.as_str(),
        record.redacted_summary
    );
    for (i, step) in record.steps.iter().enumerate() {
        out.push_str(&format!("{:>2}. {step}\n", i + 1));
    }
    out
}

// ---------------------------------------------------------------------------
// Doctor
// ---------------------------------------------------------------------------

/// The only system view in the TUI is actionable: it shows the last diagnostic
/// result and `y` runs it again. Configuration stays in `config.toml`; this
/// screen deliberately has no read-only knobs that look editable.
pub const DOCTOR_COLUMNS: [&str; 3] = ["CHECK", "STATE", "DETAIL"];

pub fn doctor_rows(snap: &Snapshot) -> Vec<Vec<String>> {
    snap.checks
        .iter()
        .map(|check| {
            vec![
                check.name.clone(),
                if check.ok {
                    "ok".into()
                } else {
                    "needs attention".into()
                },
                check.detail.clone(),
            ]
        })
        .collect()
}

/// The expanded diagnostic result. Keymap errors and startup notices belong
/// here because neither is an editable setting; both explain a degraded run.
pub fn doctor_detail_lines(
    snap: &Snapshot,
    problems: &[String],
    notices: &[String],
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        "DOCTOR".to_string(),
        theme.heading(),
    ))];
    if snap.checks.is_empty() {
        lines.push(Line::from(Span::styled(
            "  진단 결과가 아직 없습니다. 하단 단축키로 실행하세요.".to_string(),
            theme.muted(),
        )));
    }
    for check in &snap.checks {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {} ", if check.ok { "ok " } else { "!  " }),
                if check.ok { theme.ok() } else { theme.danger() },
            ),
            Span::styled(pad(&check.name, 18), theme.normal()),
            Span::styled(check.detail.clone(), theme.muted()),
        ]));
        if let Some(remedy) = &check.remedy {
            lines.push(Line::from(Span::styled(
                format!("      → {remedy}"),
                theme.warn(),
            )));
        }
    }

    if !problems.is_empty() {
        lines.push(Line::raw(String::new()));
        lines.push(Line::from(Span::styled(
            "KEYMAP".to_string(),
            theme.heading(),
        )));
        for problem in problems {
            lines.push(Line::from(Span::styled(
                format!("  ! {problem}"),
                theme.danger(),
            )));
        }
    }

    if !notices.is_empty() {
        lines.push(Line::raw(String::new()));
        lines.push(Line::from(Span::styled(
            "NOTICES".to_string(),
            theme.heading(),
        )));
        for notice in notices {
            lines.push(Line::from(Span::styled(
                format!("  ! {notice}"),
                theme.warn(),
            )));
        }
    }
    lines
}

// Engines (the two containers the whole product is built on)
// ---------------------------------------------------------------------------

/// Everything about one container, for the inspect popup on the engines
/// screen: what it is, where it listens, and what the user has inside it.
pub fn engine_detail_lines(snap: &Snapshot, index: usize, theme: &Theme) -> Vec<Line<'static>> {
    let Some(overview) = snap.engines.get(index) else {
        return vec![Line::from(Span::styled(
            "선택된 엔진이 없습니다.".to_string(),
            theme.muted(),
        ))];
    };
    let e = &overview.engine;
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{} ", theme.engine_symbol(&overview.status)),
                theme.engine_style(&overview.status),
            ),
            Span::styled(e.label(), theme.heading()),
            Span::styled(
                format!("  {}", overview.status.state),
                theme.engine_style(&overview.status),
            ),
        ]),
        Line::raw(String::new()),
    ];
    for (label, value) in [
        ("컨테이너", e.container_name.clone()),
        ("이미지", e.image.clone()),
        ("볼륨", e.volume_name.clone()),
        ("바인딩", format!("{}:{}", e.bind_address, e.host_port)),
        ("관리자", e.admin_user.clone()),
        ("Target", overview.target.display_name.clone()),
        ("생성", stamp(e.created_at)),
    ] {
        lines.push(Line::from(vec![
            Span::styled(pad(label, 12), theme.muted()),
            Span::styled(value, theme.normal()),
        ]));
    }

    let held: Vec<&Resource> = snap
        .resources
        .iter()
        .filter(|r| r.engine().id == e.id)
        .collect();
    lines.push(Line::raw(String::new()));
    lines.push(crate::tui::chrome::titled_rule(
        &format!("이 컨테이너 안 ({}건)", held.len()),
        56,
        theme,
    ));
    if held.is_empty() {
        lines.push(Line::from(Span::styled(
            "  아직 없습니다. Resources 화면에서 `n`으로 만드세요.".to_string(),
            theme.muted(),
        )));
    }
    for resource in held {
        lines.push(Line::from(vec![
            Span::styled("  ".to_string(), theme.muted()),
            Span::styled(pad(resource.name(), 24), theme.normal()),
            Span::styled(pad(kind_cell(resource.kind()), 8), theme.muted()),
            Span::styled(bytes(resource.size_bytes()), theme.muted()),
        ]));
    }
    lines
}

/// Numbered plan preview shared by the create form and every confirmation
/// modal, so "실행 전 계획" always looks the same (PRD §7.5, §7.9).
pub fn plan_lines(plan: &crate::core::plan::Plan, theme: &Theme) -> Vec<Line<'static>> {
    use crate::core::plan::StepKind;
    let mut lines = Vec::new();
    for (i, step) in plan.steps.iter().enumerate() {
        let style = match step.kind {
            StepKind::Destroy => theme.danger(),
            StepKind::New => theme.accent(),
            StepKind::Reuse => theme.muted(),
            StepKind::Verify => theme.normal(),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {:>2}. ", i + 1), theme.muted()),
            Span::styled(step.title.clone(), theme.normal()),
            Span::styled(format!(" ({})", step.kind.label()), style),
        ]));
        if let Some(detail) = &step.detail {
            lines.push(Line::from(Span::styled(
                format!("      {detail}"),
                theme.muted(),
            )));
        }
    }
    for warning in &plan.warnings {
        lines.push(Line::from(vec![
            Span::styled(" !  ".to_string(), theme.warn()),
            Span::styled(warning.clone(), theme.warn()),
        ]));
    }
    lines
}

/// Header plus body rows for whichever screen is showing, so the event loop
/// and the renderer agree on row identity (and therefore on what the cursor
/// selects) without computing it twice differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableData {
    pub headers: &'static [&'static str],
    pub rows: Vec<Vec<String>>,
    /// Indices of `rows` that survive the active `/` filter, in order.
    pub visible: Vec<usize>,
}

impl TableData {
    pub fn len(&self) -> usize {
        self.visible.len()
    }

    pub fn is_empty(&self) -> bool {
        self.visible.is_empty()
    }

    /// Row index in the underlying snapshot list for a cursor position.
    pub fn source_index(&self, cursor: usize) -> Option<usize> {
        self.visible.get(cursor).copied()
    }
}

/// Case-insensitive substring match against every cell of a row (PRD §7.6).
pub fn matching(rows: &[Vec<String>], filter: &str) -> Vec<usize> {
    let needle = filter.trim().to_ascii_lowercase();
    rows.iter()
        .enumerate()
        .filter(|(_, row)| {
            needle.is_empty()
                || row
                    .iter()
                    .any(|cell| cell.to_ascii_lowercase().contains(&needle))
        })
        .map(|(i, _)| i)
        .collect()
}

/// Natural column widths: the widest cell, header included, clamped so one
/// long file name cannot push every other column off screen.
pub fn column_widths(data: &TableData, max: u16) -> Vec<u16> {
    data.headers
        .iter()
        .enumerate()
        .map(|(column, header)| {
            let widest = data
                .visible
                .iter()
                .filter_map(|i| data.rows.get(*i))
                .filter_map(|row| row.get(column))
                .map(|cell| util::display_cols(cell))
                .max()
                .unwrap_or(0);
            (widest.max(util::display_cols(header)) as u16).min(max)
        })
        .collect()
}

/// Build the table for `screen`, then apply `filter`.
pub fn table_for(
    screen: crate::tui::keymap::Screen,
    snap: &Snapshot,
    _config: &Config,
    filter: &str,
    theme: &Theme,
) -> TableData {
    use crate::tui::keymap::Screen;
    let (headers, rows): (&'static [&'static str], Vec<Vec<String>>) = match screen {
        Screen::Resources => (&RESOURCE_COLUMNS, resource_rows(snap, theme)),
        Screen::Engines => (&ENGINE_COLUMNS, engine_rows(snap, theme)),
        Screen::Targets => (&TARGET_COLUMNS, target_rows(snap, theme)),
        Screen::Tunnels => (&TUNNEL_COLUMNS, tunnel_rows(snap, theme)),
        Screen::Backups => (&BACKUP_COLUMNS, backup_rows(snap, theme)),
        Screen::Activity => (&ACTIVITY_COLUMNS, activity_rows(snap, theme)),
        Screen::Doctor => (&DOCTOR_COLUMNS, doctor_rows(snap)),
    };
    let visible = matching(&rows, filter);
    TableData {
        headers,
        rows,
        visible,
    }
}

/// Flatten styled lines into plain text. Test-only: assertions care about the
/// words, and the styles have their own coverage in `theme`.
#[cfg(test)]
pub(crate) fn render_lines(lines: &[Line<'static>]) -> String {
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::model::ActivityStatus;
    use crate::tui::data::fixture;
    use crate::tui::keymap::Screen;

    fn plain() -> Theme {
        Theme::plain()
    }

    #[test]
    fn the_resource_table_leads_with_the_name_and_carries_what_to_paste() {
        assert_eq!(
            RESOURCE_COLUMNS,
            ["NAME", "KIND", "ENGINE", "SIZE", "CONNECT", "WHERE"]
        );
    }

    #[test]
    fn databases_and_buckets_share_one_table_and_are_told_apart_by_kind() {
        let snap = fixture::snapshot();
        let rows = resource_rows(&snap, &plain());
        assert_eq!(rows.len(), 3);
        for row in &rows {
            assert_eq!(row.len(), RESOURCE_COLUMNS.len());
        }
        let kinds: Vec<&str> = rows.iter().map(|r| r[1].as_str()).collect();
        assert_eq!(kinds, vec!["db", "bucket", "db"]);

        let bucket = rows.iter().find(|r| r[1] == "bucket").unwrap();
        assert_eq!(bucket[0], "letsbid-dev-assets");
        assert_eq!(bucket[2], "minio latest");
        assert_eq!(bucket[3], "12.0 MB");
    }

    #[test]
    fn an_unreachable_engine_leaves_the_stats_empty_instead_of_failing() {
        let local = fixture::local_target();
        let mut resource = fixture::database(&local, "letsbid_dev", None);
        if let Resource::Database(view) = &mut resource {
            view.stats = crate::core::model::DatabaseStats::default();
        }
        let snap = Snapshot {
            resources: vec![resource],
            ..Snapshot::empty()
        };
        let rows = resource_rows(&snap, &plain());
        assert_eq!(rows[0][3], EMPTY, "an unknown size is not zero");
    }

    /// A local resource is reachable at the engine's own port; a remote one is
    /// reachable only through a tunnel, and says so rather than showing `-`.
    #[test]
    fn the_connect_column_is_a_port_locally_and_a_tunnel_remotely() {
        let snap = fixture::snapshot();
        let rows = resource_rows(&snap, &plain());
        let remote = rows.iter().find(|r| r[0] == "parantica_dev").unwrap();
        assert!(remote[4].starts_with(":15432"), "{}", remote[4]);
        let local = rows.iter().find(|r| r[0] == "letsbid_dev").unwrap();
        assert_eq!(local[4], ":5432");
    }

    #[test]
    fn a_remote_resource_without_a_tunnel_says_it_needs_one() {
        let remote = fixture::remote_target();
        let resource = fixture::database(&remote, "parantica_dev", None);
        let snap = Snapshot {
            resources: vec![resource],
            ..Snapshot::empty()
        };
        let rows = resource_rows(&snap, &plain());
        assert_eq!(rows[0][4], "터널 필요");
    }

    #[test]
    fn the_engine_table_says_how_much_work_each_container_holds() {
        let snap = fixture::snapshot();
        let rows = engine_rows(&snap, &plain());
        assert_eq!(
            ENGINE_COLUMNS,
            ["ENGINE", "CONTAINER", "STATE", "BIND", "HOLDS", "WHERE"]
        );
        assert!(!rows.is_empty());
        for row in &rows {
            assert_eq!(row.len(), ENGINE_COLUMNS.len());
        }
        let holds: Vec<&str> = rows.iter().map(|r| r[4].as_str()).collect();
        assert!(
            holds.iter().any(|h| h.ends_with("db")),
            "the postgres row counts databases: {holds:?}"
        );
        assert!(
            holds.iter().any(|h| h.ends_with("bucket")),
            "the minio row counts buckets: {holds:?}"
        );
    }

    #[test]
    fn secrets_are_masked_until_revealed_and_never_leak_into_the_masked_form() {
        let local = fixture::local_target();
        let resource = fixture::database(&local, "letsbid_dev", None);
        let endpoint = Endpoint {
            url: Some("postgresql://letsbid_user:s3cr3t@127.0.0.1:5432/letsbid_dev".into()),
            redacted_url: Some("postgresql://letsbid_user:****@127.0.0.1:5432/letsbid_dev".into()),
            secret: Some("s3cr3t".into()),
            ..Endpoint::default()
        };

        let hidden = render(&resource_detail_lines(
            &resource,
            None,
            &endpoint,
            false,
            &plain(),
        ));
        assert!(hidden.contains(MASK));
        assert!(!hidden.contains("s3cr3t"));

        let shown = render(&resource_detail_lines(
            &resource,
            None,
            &endpoint,
            true,
            &plain(),
        ));
        assert!(shown.contains("s3cr3t"));
    }

    #[test]
    fn the_bucket_detail_pane_shows_endpoint_prefix_masked_key_and_objects() {
        let local = fixture::local_target();
        let resource = fixture::bucket_resource(&local, "letsbid-dev-assets");
        let endpoint = Endpoint {
            url: Some(
                "s3://AKIALINF0000000EXAMPLE:sEcReT@127.0.0.1:9000/letsbid-dev-assets".into(),
            ),
            redacted_url: Some(
                "s3://AKIALINF0000000EXAMPLE:****@127.0.0.1:9000/letsbid-dev-assets".into(),
            ),
            secret: Some("sEcReT".into()),
            address: Some("http://127.0.0.1:9000".into()),
            region: Some("us-east-1".into()),
            ..Endpoint::default()
        };
        let text = render(&resource_detail_lines(
            &resource,
            None,
            &endpoint,
            false,
            &plain(),
        ));
        assert!(text.contains("bucket · minio latest"));
        assert!(text.contains("http://127.0.0.1:9000"));
        assert!(text.contains("console :9001"));
        assert!(text.contains("region us-east-1"));
        assert!(text.contains("AKIA****************"));
        assert!(!text.contains("AKIALINF0000000EXAMPLE\n"));
        assert!(text.contains("policy linf-letsbid-dev-assets"));
        assert!(text.contains("142개"));
        assert!(text.contains(MASK));
        assert!(!text.contains("sEcReT"));
    }

    #[test]
    fn an_access_key_keeps_only_its_prefix_and_never_its_characters() {
        assert_eq!(
            mask_access_key("AKIALINF0000000EXAMP", false),
            "AKIA****************"
        );
        assert_eq!(mask_access_key("AKIA", false), "AKIA");
        assert_eq!(mask_access_key("AB", false), "AB");
        assert_eq!(
            mask_access_key("AKIALINF0000000EXAMP", true),
            "AKIALINF0000000EXAMP"
        );
    }

    #[test]
    fn a_missing_secret_says_so_instead_of_pretending_to_mask_one() {
        assert_eq!(mask(None, true), NO_SECRET);
        assert_eq!(mask(None, false), NO_SECRET);
        assert_eq!(mask(Some("abc"), false), MASK);
        assert_eq!(mask(Some("abc"), true), "abc");
    }

    #[test]
    fn a_remote_resource_without_a_tunnel_explains_itself_in_the_detail_pane() {
        let vps = fixture::remote_target();
        let resource = fixture::database(&vps, "parantica_dev", None);
        let endpoint = Endpoint {
            note: Some("SSH 터널이 필요합니다".into()),
            ..Endpoint::default()
        };
        let text = render(&resource_detail_lines(
            &resource,
            None,
            &endpoint,
            false,
            &plain(),
        ));
        assert!(text.contains("SSH 터널이 필요합니다"));
        assert!(text.contains("예약 포트 15432"));
    }

    #[test]
    fn tunnel_table_follows_the_prd_field_order() {
        assert_eq!(
            TUNNEL_COLUMNS,
            ["RESOURCE", "KIND", "LOCAL", "REMOTE", "TARGET", "PID", "SINCE", "STATE"]
        );
        let snap = fixture::snapshot();
        let rows = tunnel_rows(&snap, &plain());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], "parantica_dev");
        assert_eq!(rows[0][1], "database");
        assert_eq!(rows[0][2], "127.0.0.1:15432");
        assert_eq!(rows[0][4], "dev-vps");
        assert_eq!(rows[0][5], "48122");
    }

    #[test]
    fn activity_table_matches_the_prd_columns_and_names_the_resource() {
        assert_eq!(
            ACTIVITY_COLUMNS,
            ["TIME", "TARGET", "RESOURCE", "ACTION", "RESULT"]
        );
        let snap = fixture::snapshot();
        let rows = activity_rows(&snap, &plain());
        assert_eq!(rows[0][1], "local");
        assert_eq!(rows[0][2], "letsbid_dev");
        assert_eq!(rows[0][3], "create");
        assert!(rows[0][4].contains("ok"));
    }

    #[test]
    fn expanded_activity_shows_every_step_that_ran() {
        let snap = fixture::snapshot();
        let text = render(&activity_detail_lines(&snap, 0, &plain()));
        assert!(text.contains("STEPS"));
        assert!(text.contains("1. 엔진 확인"));
        assert!(text.contains("3. 접속 테스트"));
    }

    #[test]
    fn diagnostics_copy_carries_the_steps_and_no_secret_placeholder() {
        let snap = fixture::snapshot();
        let text = activity_diagnostics(&snap, 0);
        assert!(text.starts_with("local-infra activity"));
        assert!(text.contains("database create"));
        assert!(text.contains("1. 엔진 확인"));
    }

    #[test]
    fn the_engine_card_gives_each_service_its_port_and_held_work() {
        let snap = fixture::snapshot();
        let text = render(&engine_strip(&snap, 100, &plain()));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one direct line per service");
        assert!(lines[0].contains("Postgres 17"), "{}", lines[0]);
        assert!(lines[0].contains("PORT:5432"), "{}", lines[0]);
        assert!(lines[0].contains("2 DBs"), "{}", lines[0]);
        assert!(lines[1].contains("MinIO latest"), "{}", lines[1]);
        assert!(lines[1].contains("PORT:9000"), "{}", lines[1]);
        assert!(lines[1].contains("1 Bucket"), "{}", lines[1]);
    }

    #[test]
    fn a_missing_engine_is_called_out_rather_than_shown_as_running() {
        let mut snap = fixture::snapshot();
        for overview in &mut snap.engines {
            overview.status = crate::core::model::EngineStatus::missing();
        }
        let text = render(&engine_strip(&snap, 100, &plain()));
        assert!(text.contains("없음 on -"), "{text}");
        assert!(!text.contains("running"), "{text}");
    }

    #[test]
    fn every_screen_keeps_its_own_name_in_the_nav_strip() {
        let text = render(&[nav_line(Screen::Resources, &plain())]);
        for screen in Screen::ALL {
            assert!(text.contains(screen.title()), "missing {screen:?}");
        }
    }

    #[test]
    fn doctor_lists_only_actionable_diagnostic_results() {
        let snap = fixture::snapshot();
        let rows = doctor_rows(&snap);
        assert_eq!(DOCTOR_COLUMNS, ["CHECK", "STATE", "DETAIL"]);
        assert!(!rows.is_empty());
        assert!(rows.iter().any(|row| row[0] == "docker"));
        assert!(rows.iter().all(|row| row.len() == DOCTOR_COLUMNS.len()));
        assert!(!rows.iter().any(|row| row[0].starts_with("ui.")));
    }

    #[test]
    fn doctor_detail_reports_keymap_problems_and_startup_notices() {
        let snap = fixture::snapshot();
        let text = render(&doctor_detail_lines(
            &snap,
            &["알 수 없는 동작 이름: `nope`".to_string()],
            &["키링을 사용할 수 없습니다".to_string()],
            &plain(),
        ));
        assert!(text.contains("DOCTOR"));
        assert!(text.contains("KEYMAP"));
        assert!(text.contains("nope"));
        assert!(text.contains("NOTICES"));
        assert!(text.contains("키링"));
    }

    #[test]
    fn a_destructive_plan_step_is_rendered_with_its_warnings() {
        use crate::core::plan::{Plan, StepKind};
        let plan = Plan::new("삭제")
            .step_detailed(StepKind::Destroy, "볼륨 삭제", "되돌릴 수 없습니다")
            .warn("3개 DB의 데이터가 사라집니다");
        let text = render(&plan_lines(&plan, &plain()));
        assert!(text.contains("1. 볼륨 삭제"));
        assert!(text.contains("되돌릴 수 없습니다"));
        assert!(text.contains("3개 DB의 데이터가 사라집니다"));
    }

    #[test]
    fn empty_screens_point_at_the_key_that_fills_them() {
        let snap = crate::tui::data::Snapshot::empty();
        assert!(render(&tunnel_detail_lines(&snap, 0, &plain())).contains("`t`"));
        assert!(render(&backup_detail_lines(&snap, 0, &plain())).contains("`b`"));
        assert!(render(&target_detail_lines(&snap, 0, &plain())).contains("`a`"));
    }

    #[test]
    fn activity_status_words_accompany_the_symbol_so_colour_is_never_alone() {
        let snap = crate::tui::data::Snapshot {
            activity: vec![fixture::activity("drop", ActivityStatus::RolledBack)],
            ..crate::tui::data::Snapshot::empty()
        };
        let rows = activity_rows(&snap, &plain());
        assert!(rows[0][4].contains("rolled_back"));
    }

    fn render(lines: &[Line<'static>]) -> String {
        render_lines(lines)
    }
}
