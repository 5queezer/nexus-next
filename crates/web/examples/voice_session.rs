//! Prints the session body a voice mint sends to OpenAI, for the evaluation runner in `evals/`:
//! `cargo run -q -p nexus-web --example voice_session -- [variant] [application id]`. The runner
//! pins the model itself, so the model printed here is only the server's default. The session's
//! present is now, in `NEXUS_TIMEZONE` (default Europe/Madrid), as on the server.

fn main() {
    let mut args = std::env::args().skip(1);
    let variant = args.next().unwrap_or_default();
    let application = args.next();
    let config = nexus_web::VoiceConfig {
        model: "gpt-realtime-2.1-mini".to_owned(),
        voice: "marin".to_owned(),
        lifetime: std::time::Duration::from_secs(900),
        ..Default::default()
    };
    let tz: nexus_web::Tz = std::env::var("NEXUS_TIMEZONE")
        .unwrap_or_else(|_| "Europe/Madrid".to_owned())
        .parse()
        .expect("a time zone");
    let now = chrono::Utc::now().with_timezone(&tz);
    println!(
        "{}",
        nexus_web::mint_body(&config, &variant, application.as_deref(), now)
    );
}
