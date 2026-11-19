//! Main-stack auto-start. A worktree's django joins the main stack's compose
//! network and uses its postgres/redis there; if they aren't up, the worktree's
//! django fails every request until they are.
//!
//! Health is asked of Compose rather than inferred from host ports: the main
//! stack publishes nothing on the host, and a bound port says nothing about
//! whether postgres behind it is accepting connections yet.

use std::process::Command;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use super::config::OrchestratorConfig;

pub struct MainStackStatus {
    pub was_started: bool,
    pub waited: Duration,
}

/// If the health services are already healthy, fast-path return. Otherwise run
/// `docker compose up -d` in the main-stack dir, retry once after
/// `docker rm -f` of the hardcoded-named orphan containers on conflict,
/// then poll until they're healthy, up to 90s.
pub fn ensure_running(cfg: &OrchestratorConfig) -> Result<MainStackStatus, String> {
    if infra_healthy(cfg) {
        return Ok(MainStackStatus {
            was_started: false,
            waited: Duration::ZERO,
        });
    }

    if !cfg.main_stack_dir.exists() {
        return Err(format!(
            "main stack dir not found: {}",
            cfg.main_stack_dir.display()
        ));
    }

    info!(dir = %cfg.main_stack_dir.display(), "starting main stack");
    let mut output = compose_up(cfg);
    if !output.status.success() {
        // Most common cause is a leftover container holding a hardcoded
        // name. Force-remove the suspects and retry once.
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("already in use") || stderr.contains("Conflict") {
            warn!("compose up hit a name conflict — removing orphans and retrying");
            for name in &cfg.main_stack_orphan_container_names {
                let _ = Command::new("docker").args(["rm", "-f", name]).output();
            }
            output = compose_up(cfg);
        }
    }
    if !output.status.success() {
        return Err(format!(
            "docker compose up -d failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let started = Instant::now();
    let limit = Duration::from_secs(90);
    while started.elapsed() < limit {
        if infra_healthy(cfg) {
            return Ok(MainStackStatus {
                was_started: true,
                waited: started.elapsed(),
            });
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(format!(
        "main stack services weren't healthy within {}s",
        limit.as_secs()
    ))
}

fn infra_healthy(cfg: &OrchestratorConfig) -> bool {
    let Ok(out) = Command::new("docker")
        .args(["compose", "ps", "--format", "json"])
        .current_dir(&cfg.main_stack_dir)
        .output()
    else {
        return false;
    };
    out.status.success()
        && services_healthy(
            &String::from_utf8_lossy(&out.stdout),
            &cfg.main_stack_health_services,
        )
}

/// `docker compose ps --format json` prints one object per line. A service
/// without a healthcheck reports an empty `Health`, so running is all it can
/// promise.
fn services_healthy(ps_json: &str, wanted: &[String]) -> bool {
    let containers: Vec<serde_json::Value> = ps_json
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    wanted.iter().all(|service| {
        containers.iter().any(|c| {
            c["Service"] == service.as_str()
                && c["State"] == "running"
                && matches!(c["Health"].as_str(), Some("healthy" | "") | None)
        })
    })
}

fn compose_up(cfg: &OrchestratorConfig) -> std::process::Output {
    Command::new("docker")
        .args(["compose", "up", "-d"])
        .current_dir(&cfg.main_stack_dir)
        .output()
        // If docker itself can't be invoked, fabricate a failed Output so
        // the caller's error path runs uniformly.
        .unwrap_or_else(|e| std::process::Output {
            status: std::process::ExitStatus::default(),
            stdout: Vec::new(),
            stderr: format!("docker invoke failed: {e}").into_bytes(),
        })
}

/// Restart a specific container by name. Used after the main stack was
/// just (re)started — any worktree django that was running has stale
/// connections to the old postgres process. Best-effort; silent on failure.
pub fn restart_container(name: &str) {
    let _ = Command::new("docker").args(["restart", name]).output();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wanted() -> Vec<String> {
        vec!["postgres".into(), "redis".into()]
    }

    #[test]
    fn every_wanted_service_must_be_running_and_healthy() {
        let ps = r#"{"Service":"postgres","State":"running","Health":"healthy"}
{"Service":"redis","State":"running","Health":"healthy"}
{"Service":"django","State":"running","Health":""}"#;
        assert!(services_healthy(ps, &wanted()));
    }

    /// Postgres reports running well before it accepts connections; a django
    /// started in that window fails its first queries.
    #[test]
    fn a_service_still_starting_is_not_healthy() {
        let ps = r#"{"Service":"postgres","State":"running","Health":"starting"}
{"Service":"redis","State":"running","Health":"healthy"}"#;
        assert!(!services_healthy(ps, &wanted()));
    }

    #[test]
    fn a_missing_service_is_not_healthy() {
        let ps = r#"{"Service":"redis","State":"running","Health":"healthy"}"#;
        assert!(!services_healthy(ps, &wanted()));
        assert!(!services_healthy("", &wanted()));
    }

    #[test]
    fn a_service_without_a_healthcheck_only_has_to_run() {
        let ps = r#"{"Service":"postgres","State":"running","Health":""}
{"Service":"redis","State":"exited","Health":""}"#;
        assert!(!services_healthy(ps, &wanted()));
        assert!(services_healthy(ps, &["postgres".into()]));
    }
}
