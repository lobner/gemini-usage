//! The one HTTP agent both pollers share, configured to behave like Go's
//! `http.Client` on macOS: certificates are checked against the system trust
//! store, and a non-2xx status is a response to inspect, not an error.

use std::io::Read;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

/// The shared agent. Connections are pooled across polls, as Go's default
/// transport does.
pub fn agent() -> &'static Agent {
    static AGENT: OnceLock<Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        Agent::config_builder()
            .http_status_as_error(false)
            .tls_config(
                TlsConfig::builder()
                    .provider(TlsProvider::NativeTls)
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .into()
    })
}

/// The time a request may take: its own client timeout, cut short by the
/// deadline of the whole check. This is what Go gets from passing a context with
/// a deadline to a client that has its own timeout.
pub fn timeout(own: Duration, deadline: Instant) -> Duration {
    own.min(deadline.saturating_duration_since(Instant::now()))
}

/// Reads at most `limit` bytes of the body, like Go's `io.LimitReader`: the
/// rest is ignored, not an error.
pub fn read_limited(body: &mut ureq::Body, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    body.as_reader().take(limit).read_to_end(&mut buf)?;
    Ok(buf)
}
