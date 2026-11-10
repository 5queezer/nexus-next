//! A server with `NEXUS_ENV_LABEL` set. The label is process-wide, so these tests have a binary of
//! their own.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use nexus_core::Db;
use tower::ServiceExt;

#[tokio::test]
async fn a_labelled_server_says_so_on_every_page_as_text() {
    nexus_core::set_env_label("Staging <b>&".to_owned());
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(dir.path()).await.unwrap();
    let state = nexus_mcp::McpState::new(
        db,
        "owner-1".to_owned(),
        "owner@example.com".to_owned(),
        nexus_mcp::Renderer::InProcess,
    );
    let app = nexus_web::dashboard_router(state, "Europe/Madrid".parse().unwrap());
    for (uri, title) in [("/", "Pipeline"), ("/queue", "Follow-ups")] {
        let response = app
            .clone()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            body.contains(r#"<span class="env">Staging &lt;b&gt;&amp;</span>"#),
            "{body}"
        );
        assert!(
            body.contains(&format!(
                "<title>[Staging &lt;b&gt;&amp;] {title} - Nexus</title>"
            )),
            "{body}"
        );
        assert!(!body.contains("<b>"));
    }
}
