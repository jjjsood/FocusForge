// SPDX-License-Identifier: GPL-3.0-or-later

//! The HTTPS release source.

use std::sync::Arc;
use std::time::Duration;

use ureq::tls::{RootCerts, TlsConfig};

use super::ReleaseSource;

/// The endpoint answering with the newest final release.
pub const RELEASE_URL: &str = "https://api.github.com/repos/jjjsood/FocusForge/releases/latest";

const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BODY: u64 = 1 << 20;

/// One HTTPS GET to [`RELEASE_URL`], 5 s in total, body capped at 1 MiB.
///
/// Any status other than 200, a redirect included, is an error. The request
/// carries only `User-Agent: FocusForge/<version>` and
/// `Accept: application/vnd.github+json`; servers are verified against the
/// operating system's trust store.
pub struct HttpReleaseSource {
    client: ureq::Agent,
    url: String,
}

impl HttpReleaseSource {
    /// A source for [`RELEASE_URL`].
    pub fn new() -> Self {
        Self::with_url(RELEASE_URL)
    }

    /// A source for an arbitrary full URL.
    pub fn with_url(url: impl Into<String>) -> Self {
        let tls = TlsConfig::builder()
            .root_certs(RootCerts::PlatformVerifier)
            .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .build();
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .user_agent(concat!("FocusForge/", env!("CARGO_PKG_VERSION")))
            .accept("application/vnd.github+json")
            .max_redirects(0)
            .tls_config(tls)
            .build();
        Self {
            client: ureq::Agent::new_with_config(config),
            url: url.into(),
        }
    }
}

impl Default for HttpReleaseSource {
    fn default() -> Self {
        Self::new()
    }
}

impl ReleaseSource for HttpReleaseSource {
    fn fetch(&self) -> Result<String, String> {
        let mut response = match self.client.get(&self.url).call() {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(status)) => {
                return Err(format!("unexpected HTTP status {status}"));
            }
            Err(e) => return Err(e.to_string()),
        };
        let status = response.status().as_u16();
        if status != 200 {
            return Err(format!("unexpected HTTP status {status}"));
        }
        // The reader's limit fails a body that is exactly at it, so it sits one
        // byte above the cap and a body of exactly the cap is accepted.
        response
            .body_mut()
            .with_config()
            .limit(MAX_BODY + 1)
            .read_to_string()
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_url_is_https() {
        assert_eq!(
            RELEASE_URL,
            "https://api.github.com/repos/jjjsood/FocusForge/releases/latest"
        );
    }
}
