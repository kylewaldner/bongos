//! Refuses requests to Zed's own hosts at the HTTP client, so code inherited from Zed cannot reach them.

use std::sync::Arc;

use futures::future::BoxFuture;
use http_client::{AsyncBody, HttpClient, Request, Response, Url, http::HeaderValue};

/// Hosts that are refused, along with every subdomain of them.
///
/// `cdn.agentclientprotocol.com` is deliberately absent: the ACP registry is fetched from it, and
/// it is allowed by one documented line in `script/bongos/hosts-allow.txt`. Blocking it is one
/// more entry here.
const BLOCKED_HOSTS: &[&str] = &["zed.dev", "zed.industries"];

/// Whether `host` is a blocked host or a subdomain of one. The comparison ignores case.
pub fn is_blocked_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    BLOCKED_HOSTS.iter().any(|blocked| {
        host == *blocked
            || host
                .strip_suffix(blocked)
                .is_some_and(|prefix| prefix.ends_with('.'))
    })
}

/// An [`HttpClient`] that fails requests to blocked hosts and forwards every other request.
pub struct GuardedHttpClient {
    inner: Arc<dyn HttpClient>,
}

impl GuardedHttpClient {
    pub fn new(inner: Arc<dyn HttpClient>) -> Self {
        Self { inner }
    }
}

impl HttpClient for GuardedHttpClient {
    fn user_agent(&self) -> Option<&HeaderValue> {
        self.inner.user_agent()
    }

    fn proxy(&self) -> Option<&Url> {
        self.inner.proxy()
    }

    fn send(
        &self,
        req: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        if let Some(host) = req.uri().host()
            && is_blocked_host(host)
        {
            let message = format!("bongos does not contact {host}");
            return Box::pin(async move {
                Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, message).into())
            });
        }
        self.inner.send(req)
    }

    #[cfg(feature = "test-support")]
    fn as_fake(&self) -> &http_client::FakeHttpClient {
        self.inner.as_fake()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_client::FakeHttpClient;

    fn status(client: &GuardedHttpClient, url: &str) -> anyhow::Result<u16> {
        let request = Request::get(url).body(AsyncBody::empty())?;
        let response = smol::block_on(client.send(request))?;
        Ok(response.status().as_u16())
    }

    #[test]
    fn test_guard_blocks_zed_hosts_only() {
        let guarded = GuardedHttpClient::new(FakeHttpClient::with_200_response());
        for blocked in [
            "https://zed.dev/",
            "https://CLOUD.ZED.DEV/x",
            "https://zed.dev./",
        ] {
            assert!(
                status(&guarded, blocked).is_err(),
                "{blocked} should be blocked"
            );
        }
        for allowed in [
            "https://zed.dev.example.com/",
            "https://cdn.agentclientprotocol.com/registry.json",
            "https://github.com/",
            "https://notzed.dev/",
        ] {
            assert_eq!(
                status(&guarded, allowed).ok(),
                Some(200),
                "{allowed} should pass"
            );
        }
    }
}
