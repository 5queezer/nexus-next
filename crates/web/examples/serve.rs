//! Serves the dashboard, API and calendar from a data directory on 127.0.0.1, for looking at the
//! pages: `cargo run -p nexus-web --example serve -- <data-dir> [port]`.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let data_dir = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: serve <data-dir> [port]"))?;
    let port = args.next().unwrap_or_else(|| "8099".to_owned());
    let db = nexus_core::Db::open(data_dir.as_ref()).await?;
    let app = nexus_web::dashboard_router(db.clone(), "Europe/Madrid".parse().unwrap())
        .merge(nexus_web::api_router(db.clone()))
        .merge(nexus_web::calendar_router(db, "dev-token".to_owned()));
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await?;
    println!("http://127.0.0.1:{port}/  (calendar token: dev-token)");
    axum::serve(listener, app).await?;
    Ok(())
}
