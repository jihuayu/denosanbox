use denosanbox_connectors::{ConnectorError, ConnectorLimits, HttpConnector, HttpRequest};
use denosanbox_policy::{CancellationToken, NetworkRules};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

struct Server {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new<F: Fn(&str) -> (Duration, String) + Send + Sync + 'static>(handler: F) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let handler = Arc::new(handler);
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let handler = handler.clone();
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut data = Vec::new();
                    let mut buf = [0; 1024];
                    while !data.windows(4).any(|w| w == b"\r\n\r\n") && data.len() < 65536 {
                        let n = stream.read(&mut buf).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        data.extend_from_slice(&buf[..n]);
                    }
                    let request = String::from_utf8_lossy(&data).into_owned();
                    seen.lock().unwrap().push(request.clone());
                    let (delay, response) = handler(&request);
                    tokio::time::sleep(delay).await;
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        Self {
            port,
            requests,
            task,
        }
    }
    fn url(&self, path: &str) -> String {
        format!("http://localhost:{}{path}", self.port)
    }
}
fn request(url: String) -> HttpRequest {
    HttpRequest {
        url,
        method: "GET".into(),
        headers: BTreeMap::new(),
        body: Vec::new(),
    }
}
fn ok_response() -> String {
    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".into()
}

#[tokio::test]
async fn denied_default_and_public_only_rules_never_send_a_request() {
    let server = Server::new(|_| (Duration::ZERO, ok_response())).await;
    let client = HttpConnector::new(ConnectorLimits::default()).unwrap();
    for rules in [
        NetworkRules::deny_all(),
        NetworkRules::deny_all()
            .allow("localhost", server.port)
            .unwrap(),
    ] {
        let error = client
            .execute(request(server.url("/")), &rules, &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error, ConnectorError::Denied);
    }
    assert!(server.requests.lock().unwrap().is_empty());
}
#[tokio::test]
async fn explicit_private_grant_reaches_the_pinned_peer() {
    let server = Server::new(|_| (Duration::ZERO, ok_response())).await;
    let client = HttpConnector::new(ConnectorLimits::default()).unwrap();
    let rules = NetworkRules::deny_all()
        .allow_private("localhost", server.port)
        .unwrap();
    let response = client
        .execute(request(server.url("/")), &rules, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"ok");
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn redirects_recheck_ports_and_strip_credentials_between_origins() {
    let destination = Server::new(|_| (Duration::ZERO, ok_response())).await;
    let port = destination.port;
    let source=Server::new(move |_|(Duration::ZERO,format!("HTTP/1.1 302 Found\r\nLocation: http://localhost:{port}/next\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"))).await;
    let client = HttpConnector::new(ConnectorLimits::default()).unwrap();
    let rules = NetworkRules::deny_all()
        .allow_private("localhost", source.port)
        .unwrap();
    let error = client
        .execute(request(source.url("/")), &rules, &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error, ConnectorError::Denied);
    assert!(destination.requests.lock().unwrap().is_empty());
    let rules = rules.allow_private("localhost", destination.port).unwrap();
    let mut input = request(source.url("/"));
    input
        .headers
        .insert("Authorization".into(), "Bearer secret".into());
    input
        .headers
        .insert("Cookie".into(), "private=secret".into());
    assert_eq!(
        client
            .execute(input, &rules, &CancellationToken::new())
            .await
            .unwrap()
            .body,
        b"ok"
    );
    let requests = destination.requests.lock().unwrap();
    let next = requests[0].to_ascii_lowercase();
    assert!(!next.contains("authorization:"));
    assert!(!next.contains("cookie:"));
}
#[tokio::test]
async fn request_response_headers_and_redirects_have_explicit_bounds() {
    let server = Server::new(|_| {
        (
            Duration::ZERO,
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{}",
                "x".repeat(100)
            ),
        )
    })
    .await;
    let rules = NetworkRules::deny_all()
        .allow_private("localhost", server.port)
        .unwrap();
    let client = HttpConnector::new(ConnectorLimits {
        max_request_bytes: 8,
        max_response_bytes: 8,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        client
            .execute(request(server.url("/")), &rules, &CancellationToken::new())
            .await
            .unwrap_err(),
        ConnectorError::LimitExceeded
    );
    let mut input = request(server.url("/"));
    input.method = "POST".into();
    input.body = vec![0; 9];
    assert_eq!(
        client
            .execute(input, &rules, &CancellationToken::new())
            .await
            .unwrap_err(),
        ConnectorError::LimitExceeded
    );
    let mut input = request(server.url("/"));
    input.headers.insert("Host".into(), "forged.test".into());
    assert_eq!(
        client
            .execute(input, &rules, &CancellationToken::new())
            .await
            .unwrap_err(),
        ConnectorError::InvalidRequest
    );
    let redirect=Server::new(|_|(Duration::ZERO,"HTTP/1.1 302 Found\r\nLocation: /again\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into())).await;
    let rules = NetworkRules::deny_all()
        .allow_private("localhost", redirect.port)
        .unwrap();
    let client = HttpConnector::new(ConnectorLimits {
        max_redirects: 1,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        client
            .execute(
                request(redirect.url("/")),
                &rules,
                &CancellationToken::new()
            )
            .await
            .unwrap_err(),
        ConnectorError::RedirectLimit
    );
    assert_eq!(redirect.requests.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn deadlines_and_cancellation_stop_waiting_for_slow_network() {
    let server = Server::new(|_| (Duration::from_millis(250), ok_response())).await;
    let rules = NetworkRules::deny_all()
        .allow_private("localhost", server.port)
        .unwrap();
    let client = HttpConnector::new(ConnectorLimits {
        timeout: Duration::from_millis(30),
        ..Default::default()
    })
    .unwrap();
    let start = Instant::now();
    assert_eq!(
        client
            .execute(request(server.url("/")), &rules, &CancellationToken::new())
            .await
            .unwrap_err(),
        ConnectorError::DeadlineExceeded
    );
    assert!(start.elapsed() < Duration::from_millis(180));
    let client = HttpConnector::new(ConnectorLimits::default()).unwrap();
    let token = CancellationToken::new();
    let cancel = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel.cancel();
    });
    let start = Instant::now();
    assert_eq!(
        client
            .execute(request(server.url("/")), &rules, &token)
            .await
            .unwrap_err(),
        ConnectorError::Cancelled
    );
    assert!(start.elapsed() < Duration::from_millis(180));
}
#[tokio::test]
async fn live_narrowing_rejects_a_delayed_response() {
    let server = Server::new(|_| (Duration::from_millis(40), ok_response())).await;
    let rules = Arc::new(Mutex::new(
        NetworkRules::deny_all()
            .allow_private("localhost", server.port)
            .unwrap(),
    ));
    let changed = rules.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(10)).await;
        *changed.lock().unwrap() = NetworkRules::deny_all();
    });
    let client = HttpConnector::new(ConnectorLimits::default()).unwrap();
    assert_eq!(
        client
            .execute_with_policy(
                request(server.url("/")),
                || rules.lock().unwrap().clone(),
                &CancellationToken::new(),
                Duration::from_secs(1)
            )
            .await
            .unwrap_err(),
        ConnectorError::Denied
    );
}
