//! Log hygiene: whatever a request carries in its URL, headers or body, only method, route
//! template and status reach the logs. This file holds a single test because it installs the
//! process-wide tracing subscriber.

use std::io::Write;
use std::sync::{Arc, Mutex};

use nexus_server::config::ServeConfig;
use nexus_server::serve;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing_subscriber::fmt::MakeWriter;

const CALENDAR_TOKEN: &str = "cal-token-0123456789abcdef0123456789abcdef";

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

async fn send(addr: std::net::SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

#[tokio::test(flavor = "multi_thread")]
async fn tokens_query_strings_ids_and_bodies_never_reach_the_logs() {
    let capture = Capture::default();
    serve::init_logging(tracing::Level::TRACE, capture.clone());

    let dir = tempfile::tempdir().unwrap();
    let config = ServeConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        data_dir: dir.path().to_path_buf(),
        base_url: None,
        calendar_token: Some(CALENDAR_TOKEN.to_owned()),
        insecure_local_only: true,
        log_level: tracing::Level::TRACE,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve::run(config, listener, async move {
        let _ = stopped.await;
    }));

    let feed = send(
        addr,
        &format!(
            "GET /calendar.ics?token={CALENDAR_TOKEN}&who=jane.doe@example.com HTTP/1.1\r\n\
             Host: x\r\nAuthorization: Bearer hunter2-bearer\r\nCookie: sid=hunter2-cookie\r\n\
             Connection: close\r\n\r\n"
        ),
    )
    .await;
    assert!(feed.starts_with("HTTP/1.1 200"), "{feed}");
    send(
        addr,
        "GET /calendar.ics?token=wrong-guess-12345 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    send(
        addr,
        "GET /applications/424242?q=secret-search HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    let body = "body-secret-payload";
    send(
        addr,
        &format!(
            "POST /api/mcp/token?code=oauth-code-xyz HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();

    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    for secret in [
        CALENDAR_TOKEN,
        "token=",
        "wrong-guess",
        "jane.doe",
        "hunter2",
        "424242",
        "secret-search",
        "oauth-code",
        "body-secret",
        "?",
    ] {
        assert!(!logs.contains(secret), "log leaked {secret:?}:\n{logs}");
    }
    assert!(
        logs.contains("route=/calendar.ics") && logs.contains("status=200"),
        "{logs}"
    );
    assert!(logs.contains("route=/applications/{id}"), "{logs}");
    assert!(logs.contains("route=unmatched"), "{logs}");
}
