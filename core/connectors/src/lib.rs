//! Explicit HTTP transport: all DNS answers and every redirect are authorized.
//! No raw sockets, listeners, cookies, downloads or environment proxy fallback.
use denosanbox_policy::{CancellationToken, NetworkRules};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, OnceLock,
};
use std::time::Duration;
use tokio::sync::{oneshot, Semaphore};
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorError {
    InvalidRequest,
    Denied,
    LimitExceeded,
    DeadlineExceeded,
    Cancelled,
    UnsupportedProxy,
    Transport,
    InvalidResponse,
    RedirectLimit,
}
impl ConnectorError {
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::Denied => "network_denied",
            Self::LimitExceeded => "limit_exceeded",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::Cancelled => "cancelled",
            Self::UnsupportedProxy => "unsupported_proxy",
            Self::Transport => "transport_error",
            Self::InvalidResponse => "invalid_response",
            Self::RedirectLimit => "redirect_limit",
        }
    }
}
impl std::fmt::Display for ConnectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}
impl std::error::Error for ConnectorError {}
pub type Result<T> = std::result::Result<T, ConnectorError>;
pub type ResolveFuture = Pin<Box<dyn Future<Output = Result<Vec<IpAddr>>> + Send>>;

/// Trusted host extension point. Guest data cannot replace the resolver.
pub trait Resolver: Send + Sync {
    fn resolve(&self, host: String, port: u16) -> ResolveFuture;
    fn pending(&self) -> usize {
        0
    }
}
#[derive(Debug, Default)]
pub struct SystemResolver {
    pending: Arc<AtomicUsize>,
}
impl Resolver for SystemResolver {
    fn resolve(&self, host: String, port: u16) -> ResolveFuture {
        let pending = self.pending.clone();
        Box::pin(async move {
            static ADMISSION: OnceLock<Arc<Semaphore>> = OnceLock::new();
            let permit = ADMISSION
                .get_or_init(|| Arc::new(Semaphore::new(4)))
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| ConnectorError::Transport)?;
            let (tx, rx) = oneshot::channel();
            struct Pending(Arc<AtomicUsize>);
            impl Drop for Pending {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::AcqRel);
                }
            }
            pending.fetch_add(1, Ordering::AcqRel);
            let pending_guard = Pending(pending);
            std::thread::Builder::new()
                .name("denosanbox-dns".into())
                .spawn(move || {
                    let _pending = pending_guard;
                    let _permit = permit;
                    // getaddrinfo may be non-cooperative. Ownership/admission remains
                    // retained on this native thread after an awaiting task cancels.
                    let result = (host.as_str(), port)
                        .to_socket_addrs()
                        .map_err(|_| ConnectorError::Transport)
                        .and_then(|answers| {
                            let mut ips = Vec::new();
                            for (index, answer) in answers.take(17).enumerate() {
                                if index == 16 {
                                    return Err(ConnectorError::Denied);
                                }
                                if !ips.contains(&answer.ip()) {
                                    ips.push(answer.ip());
                                }
                            }
                            if ips.is_empty() || ips.len() > 16 {
                                Err(ConnectorError::Denied)
                            } else {
                                Ok(ips)
                            }
                        });
                    let _ = tx.send(result);
                })
                .map_err(|_| ConnectorError::Transport)?;
            rx.await.map_err(|_| ConnectorError::Transport)?
        })
    }
    fn pending(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone)]
pub struct ConnectorLimits {
    pub max_request_bytes: usize,
    pub max_response_bytes: usize,
    pub max_header_bytes: usize,
    pub max_headers: usize,
    pub max_redirects: usize,
    pub timeout: Duration,
}
impl Default for ConnectorLimits {
    fn default() -> Self {
        Self {
            max_request_bytes: 1024 * 1024,
            max_response_bytes: 1024 * 1024,
            max_header_bytes: 16 * 1024,
            max_headers: 64,
            max_redirects: 5,
            timeout: Duration::from_secs(30),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpRequest {
    pub url: String,
    pub method: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Vec<u8>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpResponse {
    pub url: String,
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}
#[derive(Clone)]
pub struct HttpConnector {
    limits: ConnectorLimits,
    resolver: Arc<dyn Resolver>,
}
impl std::fmt::Debug for HttpConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpConnector")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}
impl HttpConnector {
    pub fn new(limits: ConnectorLimits) -> Result<Self> {
        Self::with_resolver(limits, Arc::new(SystemResolver::default()))
    }
    pub fn with_resolver(limits: ConnectorLimits, resolver: Arc<dyn Resolver>) -> Result<Self> {
        if limits.timeout.is_zero()
            || limits.timeout > Duration::from_secs(300)
            || limits.max_request_bytes == 0
            || limits.max_response_bytes == 0
            || limits.max_header_bytes == 0
            || limits.max_headers == 0
            || limits.max_redirects > 16
        {
            return Err(ConnectorError::InvalidRequest);
        }
        Ok(Self { limits, resolver })
    }
    pub fn pending_resolutions(&self) -> usize {
        self.resolver.pending()
    }
    pub async fn execute(
        &self,
        request: HttpRequest,
        rules: &NetworkRules,
        cancellation: &CancellationToken,
    ) -> Result<HttpResponse> {
        self.execute_with_policy(request, || rules.clone(), cancellation, self.limits.timeout)
            .await
    }
    /// Re-read trusted policy at every hop and before each body chunk. This
    /// supports live narrowing while a connector is in flight.
    pub async fn execute_with_policy<F>(
        &self,
        request: HttpRequest,
        policy: F,
        cancellation: &CancellationToken,
        remaining: Duration,
    ) -> Result<HttpResponse>
    where
        F: Fn() -> NetworkRules + Send + Sync,
    {
        if cancellation.is_cancelled() {
            return Err(ConnectorError::Cancelled);
        }
        if remaining.is_zero() {
            return Err(ConnectorError::DeadlineExceeded);
        }
        let work = self.execute_inner(request, &policy);
        tokio::pin!(work);
        let timer = tokio::time::sleep(remaining.min(self.limits.timeout));
        tokio::pin!(timer);
        let mut tick = tokio::time::interval(Duration::from_millis(5));
        loop {
            tokio::select! {
                biased;
                _ = &mut timer => return Err(ConnectorError::DeadlineExceeded),
                _ = tick.tick() => { if cancellation.is_cancelled() { return Err(ConnectorError::Cancelled); } }
                result = &mut work => return if cancellation.is_cancelled() { Err(ConnectorError::Cancelled) } else { result },
            }
        }
    }
    async fn resolve_checked<F: Fn() -> NetworkRules>(
        &self,
        host: &str,
        port: u16,
        policy: &F,
    ) -> Result<Vec<IpAddr>> {
        policy()
            .check_host(host, port)
            .map_err(|_| ConnectorError::Denied)?;
        let ips = self.resolver.resolve(host.to_owned(), port).await?;
        if ips.is_empty() || ips.len() > 16 {
            return Err(ConnectorError::Denied);
        }
        policy()
            .check(host, port, &ips)
            .map_err(|_| ConnectorError::Denied)?;
        Ok(ips)
    }
    async fn execute_inner<F: Fn() -> NetworkRules>(
        &self,
        mut request: HttpRequest,
        policy: &F,
    ) -> Result<HttpResponse> {
        let mut url = checked_url(&request.url)?;
        let mut method = reqwest::Method::from_bytes(request.method.as_bytes())
            .map_err(|_| ConnectorError::InvalidRequest)?;
        if !matches!(
            method.as_str(),
            "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS"
        ) {
            return Err(ConnectorError::InvalidRequest);
        }
        if request.body.len() > self.limits.max_request_bytes {
            return Err(ConnectorError::LimitExceeded);
        }
        if matches!(method.as_str(), "GET" | "HEAD") && !request.body.is_empty() {
            return Err(ConnectorError::InvalidRequest);
        }
        checked_request_headers(&request.headers, &self.limits)?;
        let mut upload_remaining = self.limits.max_request_bytes;
        for hop in 0..=self.limits.max_redirects {
            let host = url
                .host_str()
                .ok_or(ConnectorError::InvalidRequest)?
                .to_owned();
            let port = url
                .port_or_known_default()
                .ok_or(ConnectorError::InvalidRequest)?;
            policy()
                .check_host(&host, port)
                .map_err(|_| ConnectorError::Denied)?;
            // An HTTP forward proxy can resolve a different peer than the checked
            // addresses. Never silently weaken pinning or bypass a required proxy.
            if proxy_applies(&url) {
                return Err(ConnectorError::UnsupportedProxy);
            }
            let ips = self.resolve_checked(&host, port, policy).await?;
            let sockets: Vec<_> = ips.iter().map(|ip| SocketAddr::new(*ip, port)).collect();
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .resolve_to_addrs(&host, &sockets)
                .pool_max_idle_per_host(0)
                .build()
                .map_err(|_| ConnectorError::Transport)?;
            if request.body.len() > upload_remaining {
                return Err(ConnectorError::LimitExceeded);
            }
            upload_remaining -= request.body.len();
            let mut builder = client.request(method.clone(), url.clone());
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            let mut response = builder
                .body(request.body.clone())
                .send()
                .await
                .map_err(|_| ConnectorError::Transport)?;
            if !response
                .remote_addr()
                .is_some_and(|peer| sockets.contains(&peer))
            {
                return Err(ConnectorError::Denied);
            }
            policy()
                .check(&host, port, &ips)
                .map_err(|_| ConnectorError::Denied)?;
            if response.headers().len() > self.limits.max_headers
                || response
                    .headers()
                    .iter()
                    .map(|(k, v)| k.as_str().len() + v.as_bytes().len())
                    .sum::<usize>()
                    > self.limits.max_header_bytes
            {
                return Err(ConnectorError::LimitExceeded);
            }
            let status = response.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                if hop == self.limits.max_redirects {
                    return Err(ConnectorError::RedirectLimit);
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or(ConnectorError::InvalidResponse)?;
                let next = checked_url(
                    url.join(location)
                        .map_err(|_| ConnectorError::InvalidResponse)?
                        .as_str(),
                )?;
                if url.scheme() == "https" && next.scheme() == "http" {
                    return Err(ConnectorError::Denied);
                }
                if url.origin() != next.origin() {
                    request.headers.retain(|name, _| {
                        !matches!(
                            name.to_ascii_lowercase().as_str(),
                            "authorization" | "cookie"
                        )
                    });
                }
                if (status == 303 && method != reqwest::Method::HEAD)
                    || (matches!(status, 301 | 302) && method == reqwest::Method::POST)
                {
                    method = reqwest::Method::GET;
                    request.body.clear();
                    request.headers.retain(|name, _| {
                        !matches!(
                            name.to_ascii_lowercase().as_str(),
                            "content-type" | "content-encoding"
                        )
                    });
                }
                url = next;
                continue;
            }
            if response
                .content_length()
                .is_some_and(|len| len > self.limits.max_response_bytes as u64)
            {
                return Err(ConnectorError::LimitExceeded);
            }
            let headers = response
                .headers()
                .iter()
                .map(|(k, v)| {
                    Ok((
                        k.to_string(),
                        v.to_str()
                            .map_err(|_| ConnectorError::InvalidResponse)?
                            .to_owned(),
                    ))
                })
                .collect::<Result<_>>()?;
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| ConnectorError::Transport)?
            {
                policy()
                    .check(&host, port, &ips)
                    .map_err(|_| ConnectorError::Denied)?;
                if chunk.len() > self.limits.max_response_bytes.saturating_sub(body.len()) {
                    return Err(ConnectorError::LimitExceeded);
                }
                body.extend_from_slice(&chunk);
            }
            return Ok(HttpResponse {
                url: url.to_string(),
                status,
                headers,
                body,
            });
        }
        Err(ConnectorError::RedirectLimit)
    }
}
fn checked_url(raw: &str) -> Result<Url> {
    if raw.len() > 8192 {
        return Err(ConnectorError::InvalidRequest);
    }
    let url = Url::parse(raw).map_err(|_| ConnectorError::InvalidRequest)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !matches!(url.host(), Some(url::Host::Domain(_)))
    {
        return Err(ConnectorError::InvalidRequest);
    }
    denosanbox_policy::HostPattern::parse(url.host_str().ok_or(ConnectorError::InvalidRequest)?)
        .map_err(|_| ConnectorError::InvalidRequest)?;
    Ok(url)
}
fn checked_request_headers(
    headers: &BTreeMap<String, String>,
    limits: &ConnectorLimits,
) -> Result<()> {
    if headers.len() > limits.max_headers
        || headers
            .iter()
            .map(|(k, v)| k.len() + v.len())
            .sum::<usize>()
            > limits.max_header_bytes
    {
        return Err(ConnectorError::LimitExceeded);
    }
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "host"
                | "connection"
                | "transfer-encoding"
                | "content-length"
                | "proxy-authorization"
                | "proxy-connection"
                | "upgrade"
                | "te"
                | "trailer"
                | "expect"
        ) || lower.starts_with("proxy-")
            || reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err()
            || reqwest::header::HeaderValue::from_str(value).is_err()
        {
            return Err(ConnectorError::InvalidRequest);
        }
    }
    Ok(())
}
fn proxy_applies(url: &Url) -> bool {
    let host = url.host_str().unwrap_or_default();
    let no_proxy = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .unwrap_or_default();
    if no_proxy.split(',').map(str::trim).any(|entry| {
        entry == "*"
            || entry == host
            || entry
                .strip_prefix('.')
                .is_some_and(|suffix| host == suffix || host.ends_with(&format!(".{suffix}")))
    }) {
        return false;
    }
    let keys: &[&str] = if url.scheme() == "https" {
        &["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
    } else {
        &["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
    };
    keys.iter()
        .any(|key| std::env::var(key).is_ok_and(|value| !value.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Sequence {
        calls: Arc<AtomicUsize>,
        answers: Vec<Vec<IpAddr>>,
    }
    impl Resolver for Sequence {
        fn resolve(&self, _: String, _: u16) -> ResolveFuture {
            let index = self.calls.fetch_add(1, Ordering::AcqRel);
            let answer = self.answers[index.min(self.answers.len() - 1)].clone();
            Box::pin(async move { Ok(answer) })
        }
    }
    #[tokio::test]
    async fn hostname_is_authorized_before_dns_and_changed_answers_are_rechecked() {
        let calls = Arc::new(AtomicUsize::new(0));
        let client = HttpConnector::with_resolver(
            ConnectorLimits::default(),
            Arc::new(Sequence {
                calls: calls.clone(),
                answers: vec![
                    vec!["8.8.8.8".parse().unwrap()],
                    vec!["127.0.0.1".parse().unwrap()],
                ],
            }),
        )
        .unwrap();
        assert_eq!(
            client
                .resolve_checked("allowed.test", 80, &NetworkRules::deny_all)
                .await
                .unwrap_err(),
            ConnectorError::Denied
        );
        assert_eq!(calls.load(Ordering::Acquire), 0);
        let rules = NetworkRules::deny_all().allow("allowed.test", 80).unwrap();
        assert_eq!(
            client
                .resolve_checked("allowed.test", 80, &|| rules.clone())
                .await
                .unwrap(),
            vec!["8.8.8.8".parse::<IpAddr>().unwrap()]
        );
        assert_eq!(
            client
                .resolve_checked("allowed.test", 80, &|| rules.clone())
                .await
                .unwrap_err(),
            ConnectorError::Denied
        );
        assert_eq!(calls.load(Ordering::Acquire), 2);
    }
    #[test]
    fn urls_and_transport_headers_fail_closed() {
        for url in [
            "file:///etc/passwd",
            "data:text/plain,a",
            "http://127.0.0.1/",
            "http://[::1]/",
            "https://user:secret@example.com/",
            "https://example.com/#fragment",
            "http://localhost./",
        ] {
            assert_eq!(
                checked_url(url).unwrap_err(),
                ConnectorError::InvalidRequest
            );
        }
        for name in [
            "Host",
            "Connection",
            "Content-Length",
            "Proxy-Authorization",
            "Transfer-Encoding",
            "Upgrade",
        ] {
            assert_eq!(
                checked_request_headers(
                    &BTreeMap::from([(name.into(), "x".into())]),
                    &ConnectorLimits::default()
                )
                .unwrap_err(),
                ConnectorError::InvalidRequest
            );
        }
    }
    #[tokio::test]
    async fn configured_proxy_is_an_explicit_gate_without_direct_fallback() {
        let url = "https://proxy-policy-proof.invalid/";
        if proxy_applies(&checked_url(url).unwrap()) {
            let calls = Arc::new(AtomicUsize::new(0));
            let client = HttpConnector::with_resolver(
                ConnectorLimits::default(),
                Arc::new(Sequence {
                    calls: calls.clone(),
                    answers: vec![vec!["8.8.8.8".parse().unwrap()]],
                }),
            )
            .unwrap();
            let request = HttpRequest {
                url: url.into(),
                method: "GET".into(),
                headers: BTreeMap::new(),
                body: Vec::new(),
            };
            let rules = NetworkRules::deny_all()
                .allow("proxy-policy-proof.invalid", 443)
                .unwrap();
            assert_eq!(
                client
                    .execute(request, &rules, &CancellationToken::new())
                    .await
                    .unwrap_err(),
                ConnectorError::UnsupportedProxy
            );
            assert_eq!(calls.load(Ordering::Acquire), 0);
        }
    }
}
