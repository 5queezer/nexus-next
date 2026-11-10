//! A server with `NEXUS_ENV_LABEL` set. The label is process-wide, so this test has a binary of its
//! own.

mod common;

use axum::http::StatusCode;
use common::*;

#[tokio::test]
async fn a_labelled_server_says_so_on_the_login_page_as_text() {
    nexus_core::set_env_label("Staging <b>&".to_owned());
    let h = Harness::new().await;
    let page = h.send(get_req("/login")).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(
        page.body
            .contains(r#"<span class="env">Staging &lt;b&gt;&amp;</span>"#),
        "{}",
        page.body
    );
    assert!(
        page.body
            .contains("<title>[Staging &lt;b&gt;&amp;] Log in to Nexus</title>"),
        "{}",
        page.body
    );
    assert!(!page.body.contains("<b>"));
}
