//! Read-only discovery of containers this app did not create (PRD §8.7).
//!
//! MIG-001 lets the user *see* the database containers already running on a
//! target so they can decide what to do with them. MIG-002 is the other half of
//! the promise: nothing here ever adopts, relabels, stops or edits a foreign
//! resource. [`take_over`] exists only to say no out loud, because a silent
//! no-op would look like it had worked.

use crate::core::ctx::Ctx;
use crate::core::docker;
use crate::core::error::{Error, Result};
use crate::core::exec::Executor;
use crate::core::model::{ForeignContainer, Target};

/// Every container on the target paired with its live managed flag.
///
/// The flag is read from the container's own labels rather than from the local
/// database, so a container the app created but forgot about still counts as
/// managed, and a relabelled one stops counting immediately (ENG-006).
async fn inventory(x: &Executor) -> Result<Vec<(ForeignContainer, bool)>> {
    let all = docker::list_containers(x).await?;
    let mut out = Vec::with_capacity(all.len());
    for container in all {
        let managed = docker::is_managed(x, &container.name).await?;
        out.push((container, managed));
    }
    Ok(out)
}

fn keep(items: Vec<(ForeignContainer, bool)>, managed: bool) -> Vec<ForeignContainer> {
    items
        .into_iter()
        .filter(|(_, m)| *m == managed)
        .map(|(c, _)| c)
        .collect()
}

/// Containers on the target without `local-infra.managed=true` (MIG-001).
pub async fn foreign_containers(ctx: &Ctx, target: &Target) -> Result<Vec<ForeignContainer>> {
    let x = ctx.executor(target)?;
    Ok(keep(inventory(&x).await?, false))
}

/// The complement: containers this app owns on the target.
pub async fn managed_containers(ctx: &Ctx, target: &Target) -> Result<Vec<ForeignContainer>> {
    let x = ctx.executor(target)?;
    Ok(keep(inventory(&x).await?, true))
}

fn refusal(container: &str) -> Error {
    Error::Refused(format!(
        "컨테이너 `{container}`의 관리권을 인수하지 않습니다. \
         local-infra는 자신이 만들지 않은 리소스에 `{}` label을 붙이거나 설정을 바꾸지 않습니다. \
         공유 엔진으로 옮기려면 `linf engine ensure`로 새 엔진을 만든 뒤 \
         `linf backup run`과 `linf backup restore`로 데이터를 이전하세요.",
        docker::LABEL_MANAGED
    ))
}

/// Always refuses (MIG-002). Adoption is deliberately not implemented in the
/// MVP, and pretending to succeed would be worse than saying so.
pub async fn take_over(_ctx: &Ctx, _target: &Target, container: &str) -> Result<()> {
    Err(refusal(container))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(name: &str, image: &str) -> ForeignContainer {
        ForeignContainer {
            id: format!("id-{name}"),
            name: name.to_string(),
            image: image.to_string(),
            state: "running".into(),
            ports: "0.0.0.0:5432->5432/tcp".into(),
            guessed_engine: Some("postgres".into()),
        }
    }

    fn sample() -> Vec<(ForeignContainer, bool)> {
        vec![
            (container("linf-postgres-17", "postgres:17"), true),
            (container("letsbid-db-1", "postgres:16"), false),
            (container("some-redis", "redis:7"), false),
        ]
    }

    #[test]
    fn discovery_splits_on_the_managed_label_only() {
        let foreign = keep(sample(), false);
        assert_eq!(
            foreign.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["letsbid-db-1", "some-redis"]
        );

        let managed = keep(sample(), true);
        assert_eq!(
            managed.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["linf-postgres-17"]
        );
    }

    #[test]
    fn the_two_views_partition_the_inventory_without_loss() {
        let total = sample().len();
        assert_eq!(
            keep(sample(), true).len() + keep(sample(), false).len(),
            total
        );
    }

    #[test]
    fn taking_over_a_foreign_container_is_refused_with_a_way_forward() {
        let err = refusal("letsbid-db-1");
        assert!(matches!(err, Error::Refused(_)), "{err:?}");
        assert_eq!(err.exit_code(), 2, "user-facing refusal, not a crash");
        let message = err.to_string();
        assert!(message.contains("letsbid-db-1"), "{message}");
        assert!(message.contains("backup restore"), "{message}");
    }

    #[tokio::test]
    async fn an_unreachable_daemon_fails_loudly_instead_of_reporting_nothing() {
        let x = Executor::Local {
            docker: "false".into(),
        };
        // An empty list would read as "no foreign containers here", which is a
        // dangerous thing to say when we simply could not look.
        assert!(inventory(&x).await.is_err());
    }
}
