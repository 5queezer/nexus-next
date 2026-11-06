//! Startup configuration for `nexus serve`: flags and environment in, a validated config out.
//! Secrets are only ever read from files, so none appears in the process list.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use axum::http::Uri;
use axum::http::uri::Scheme;
use clap::Args;
use tracing::Level;

const MIN_TOKEN_LEN: usize = 32;

#[derive(Args, Debug)]
pub struct ServeArgs {
    /// Address to listen on. Keep it on loopback and publish through a tunnel or Tailscale.
    #[arg(long, env = "NEXUS_BIND", default_value = "127.0.0.1:8080")]
    pub bind: SocketAddr,
    /// Where nexus.db and blobs/ live.
    #[arg(long, env = "NEXUS_DATA_DIR", default_value = "/var/lib/nexus")]
    pub data_dir: PathBuf,
    /// Public origin clients reach this server at, for example https://nexus.example.net.
    #[arg(long, env = "NEXUS_BASE_URL")]
    pub base_url: Option<String>,
    /// File holding the secret token of /calendar.ics. Without it the feed is not served.
    #[arg(long, env = "NEXUS_CALENDAR_TOKEN_FILE")]
    pub calendar_token_file: Option<PathBuf>,
    /// Serve the dashboard and JSON API without authentication. Binds to loopback only.
    #[arg(long)]
    pub insecure_local_only: bool,
    /// Most verbose level logged: error, warn, info, debug or trace.
    #[arg(long, env = "NEXUS_LOG_LEVEL", default_value = "info")]
    pub log_level: Level,
}

#[derive(Debug)]
pub struct ServeConfig {
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    pub base_url: Option<String>,
    pub calendar_token: Option<String>,
    pub insecure_local_only: bool,
    pub log_level: Level,
}

impl ServeArgs {
    /// Checks every setting and reports all problems at once.
    pub fn validate(self) -> Result<ServeConfig, String> {
        let mut problems = Vec::new();

        if self.insecure_local_only && !self.bind.ip().is_loopback() {
            problems.push(format!(
                "--insecure-local-only needs a loopback --bind, got {}",
                self.bind
            ));
        }
        if self.data_dir.as_os_str().is_empty() {
            problems.push("--data-dir must not be empty".to_owned());
        }
        let base_url = self.base_url.as_deref().map(|url| {
            normalize_base_url(url).unwrap_or_else(|e| {
                problems.push(format!("--base-url: {e}"));
                String::new()
            })
        });
        let calendar_token = self.calendar_token_file.as_ref().map(|path| {
            read_token(path).unwrap_or_else(|e| {
                problems.push(format!("--calendar-token-file {}: {e}", path.display()));
                String::new()
            })
        });

        if !problems.is_empty() {
            return Err(problems.join("\n"));
        }
        Ok(ServeConfig {
            bind: self.bind,
            data_dir: self.data_dir,
            base_url,
            calendar_token,
            insecure_local_only: self.insecure_local_only,
            log_level: self.log_level,
        })
    }
}

/// An origin without path, query, fragment or credentials. Plain http is only for loopback.
fn normalize_base_url(raw: &str) -> Result<String, String> {
    let uri: Uri = raw.parse().map_err(|_| "not a valid URL".to_owned())?;
    let (Some(scheme), Some(authority)) = (uri.scheme(), uri.authority()) else {
        return Err("must look like https://host".to_owned());
    };
    if authority.as_str().contains('@') {
        return Err("must not contain credentials".to_owned());
    }
    if !matches!(uri.path(), "" | "/") || uri.query().is_some() || raw.contains('#') {
        return Err("must be an origin only, without path, query or fragment".to_owned());
    }
    let loopback = matches!(authority.host(), "localhost" | "127.0.0.1" | "[::1]");
    if *scheme != Scheme::HTTPS && !(*scheme == Scheme::HTTP && loopback) {
        return Err("must use https (http is only accepted for localhost)".to_owned());
    }
    Ok(format!("{scheme}://{authority}"))
}

fn read_token(path: &std::path::Path) -> Result<String, String> {
    let mode = std::fs::metadata(path)
        .map_err(|e| format!("cannot read: {}", e.kind()))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "mode {:o} lets other users read the secret, use 0400 or 0600",
            mode & 0o777
        ));
    }
    let token = std::fs::read_to_string(path).map_err(|e| format!("cannot read: {}", e.kind()))?;
    let token = token.trim();
    if token.len() < MIN_TOKEN_LEN || token.contains(char::is_whitespace) {
        return Err(format!(
            "must hold one token of at least {MIN_TOKEN_LEN} characters without whitespace"
        ));
    }
    Ok(token.to_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn args() -> ServeArgs {
        ServeArgs {
            bind: "127.0.0.1:8080".parse().unwrap(),
            data_dir: "/var/lib/nexus".into(),
            base_url: None,
            calendar_token_file: None,
            insecure_local_only: false,
            log_level: Level::INFO,
        }
    }

    fn token_file(mode: u32, content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        fs::write(&path, content).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        (dir, path)
    }

    #[test]
    fn defaults_are_valid() {
        let config = args().validate().unwrap();
        assert!(config.calendar_token.is_none() && config.base_url.is_none());
    }

    #[test]
    fn base_url_is_normalised_or_rejected() {
        for (raw, expected) in [
            ("https://nexus.example.net", "https://nexus.example.net"),
            ("https://nexus.example.net/", "https://nexus.example.net"),
            (
                "https://nexus.example.net:8443",
                "https://nexus.example.net:8443",
            ),
            ("http://localhost:8080", "http://localhost:8080"),
        ] {
            assert_eq!(normalize_base_url(raw).unwrap(), expected, "{raw}");
        }
        for bad in [
            "nexus.example.net",
            "http://nexus.example.net",
            "https://nexus.example.net/app",
            "https://nexus.example.net/?x=1",
            "https://nexus.example.net/#frag",
            "https://user:pw@nexus.example.net",
            "ftp://nexus.example.net",
            "",
        ] {
            assert!(normalize_base_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn insecure_mode_requires_loopback() {
        let mut a = args();
        a.insecure_local_only = true;
        a.bind = "0.0.0.0:8080".parse().unwrap();
        assert!(a.validate().unwrap_err().contains("loopback"));
        let mut a = args();
        a.insecure_local_only = true;
        a.bind = "[::1]:8080".parse().unwrap();
        assert!(a.validate().is_ok());
    }

    #[test]
    fn token_file_is_read_and_trimmed() {
        let secret = "a".repeat(40);
        let (_dir, path) = token_file(0o400, &format!("{secret}\n"));
        let mut a = args();
        a.calendar_token_file = Some(path);
        assert_eq!(a.validate().unwrap().calendar_token.unwrap(), secret);
    }

    #[test]
    fn bad_token_files_are_rejected_without_echoing_the_secret() {
        let weak = "short-secret-value";
        let loose = "b".repeat(40);
        for (mode, content, needle) in [
            (0o400, weak, "at least"),
            (
                0o400,
                "two words in one token file of enough length",
                "whitespace",
            ),
            (0o644, loose.as_str(), "other users"),
        ] {
            let (_dir, path) = token_file(mode, content);
            let mut a = args();
            a.calendar_token_file = Some(path);
            let error = a.validate().unwrap_err();
            assert!(error.contains(needle), "{error}");
            assert!(!error.contains(content), "{error}");
        }
        let mut a = args();
        a.calendar_token_file = Some("/nonexistent/token".into());
        assert!(a.validate().unwrap_err().contains("cannot read"));
    }

    #[test]
    fn all_problems_are_reported_together() {
        let mut a = args();
        a.base_url = Some("http://example.net".into());
        a.calendar_token_file = Some("/nonexistent/token".into());
        let error = a.validate().unwrap_err();
        assert_eq!(error.lines().count(), 2, "{error}");
    }
}
