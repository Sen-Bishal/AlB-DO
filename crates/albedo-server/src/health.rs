//! A2 · `/_albedo/health` and `/_albedo/ready` — what Docker's `HEALTHCHECK`,
//! Fly's checks, a load balancer and a Kubernetes probe ask.
//!
//! 🔴 **The shipped `HEALTHCHECK` fetched `/`.** An app whose home page sits
//! behind sign-in answers that with a redirect, a slow one answers it late, a
//! broken one with a 500 — and each marks a healthy server unhealthy for an
//! orchestrator to kill. A probe has to ask the server, not the app.
//!
//! Two questions, because "no" means different things:
//!
//! - **health** — is the process answering? No database, no render, no app
//!   code. A "no" here means *restart it*, so it must never say no for anything
//!   a restart would not fix.
//! - **ready** — should traffic come here? No while shutting down, so a load
//!   balancer stops sending requests *before* the drain closes connections —
//!   the half of a rolling deploy that otherwise drops them — and no when the
//!   database does not answer a read in time.
//!
//! Mounted on the router itself, ahead of dispatch: no middleware, no route
//! gate, no rate limit. A probe a sign-in gate could redirect, or that SHUTTER
//! could 429 because the prober shares an address with real traffic, is the
//! defect being replaced. The bodies name a state and nothing else — no
//! version, nothing a stranger could fingerprint.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::sync::watch;

/// Liveness.
pub const HEALTH_PATH: &str = "/_albedo/health";

/// Readiness.
pub const READY_PATH: &str = "/_albedo/ready";

/// A database read slower than this is not ready. Probes time out at a few
/// seconds; answering before they do is the difference between "not ready"
/// and "dead".
const DATABASE_DEADLINE: Duration = Duration::from_secs(2);

type Substrate = Arc<OnceLock<Arc<dyn dom_render_compiler::forge::DataSubstrate>>>;

fn answer(status: StatusCode, state: &str) -> Response {
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_TYPE, "application/json"),
        ],
        format!("{{\"status\":\"{state}\"}}"),
    )
        .into_response()
}

/// `GET /_albedo/health`.
pub async fn health() -> Response {
    answer(StatusCode::OK, "ok")
}

/// What `/_albedo/ready` reads.
#[derive(Clone)]
pub struct Readiness {
    /// Flips once, when shutdown begins.
    pub shutdown: watch::Receiver<bool>,
    /// Filled before the listener binds, in a build with FORGE.
    pub substrate: Substrate,
}

/// `GET /_albedo/ready`.
///
/// The read is `sqlite_master`: it goes through the same reader connection
/// every page uses, which is what "the database answers" means here. It does
/// not prove the disk underneath is healthy — the page cache can answer it.
pub async fn ready(readiness: Readiness) -> Response {
    if *readiness.shutdown.borrow() {
        return answer(StatusCode::SERVICE_UNAVAILABLE, "draining");
    }
    if let Some(substrate) = readiness.substrate.get() {
        let read = tokio::time::timeout(
            DATABASE_DEADLINE,
            substrate.query("SELECT count(*) FROM sqlite_master", &[]),
        )
        .await;
        if !matches!(read, Ok(Ok(_))) {
            return answer(StatusCode::SERVICE_UNAVAILABLE, "database");
        }
    }
    answer(StatusCode::OK, "ready")
}

#[cfg(test)]
mod tests {
    use super::*;
    use dom_render_compiler::forge::value::{Result, Rows, SqlValue, SubstrateError};
    use dom_render_compiler::forge::{DataSubstrate, Transaction};

    /// A database that refuses every read.
    struct Broken;

    #[async_trait::async_trait]
    impl DataSubstrate for Broken {
        async fn migrate(&self, _: &str) -> Result<()> {
            Err(SubstrateError::Backend("disk gone".into()))
        }
        async fn query(&self, _: &str, _: &[SqlValue]) -> Result<Rows> {
            Err(SubstrateError::Backend("disk gone".into()))
        }
        async fn execute(&self, _: &str, _: &[SqlValue]) -> Result<u64> {
            Err(SubstrateError::Backend("disk gone".into()))
        }
        async fn begin(&self) -> Result<Box<dyn Transaction>> {
            Err(SubstrateError::Backend("disk gone".into()))
        }
    }

    #[tokio::test]
    async fn a_database_that_does_not_answer_is_not_ready() {
        let (_tx, shutdown) = watch::channel(false);
        let substrate: Substrate = Arc::new(OnceLock::new());
        let _ = substrate.set(Arc::new(Broken));
        let response = ready(Readiness { shutdown, substrate }).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
