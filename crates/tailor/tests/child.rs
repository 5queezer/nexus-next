//! The parent's side of the worker, against small scripts instead of a compile.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use nexus_tailor::{Facts, RenderError, RenderRequest, Renderer};

fn script(dir: &std::path::Path, body: &str) -> PathBuf {
    let path = dir.join("worker.sh");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn renderer(exe: PathBuf, rss_limit_bytes: u64, timeout: Duration) -> Renderer {
    Renderer::Subprocess {
        exe,
        rss_limit_bytes,
        timeout,
    }
}

fn request() -> RenderRequest {
    RenderRequest {
        source: "x".to_owned(),
        facts: Facts::default(),
        pdf: false,
        preview_pages: 0,
    }
}

fn scratch() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn alive(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        // The state after the command name. A zombie is gone but for its entry.
        stat.rsplit(')').next().unwrap().split_whitespace().next() != Some("Z")
    })
}

async fn wait_gone(pid: &str) {
    for _ in 0..200 {
        if std::fs::metadata(format!("/proc/{pid}")).is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{pid} is still there, alive: {}", alive(pid));
}

#[tokio::test]
async fn the_child_holds_only_stdio_and_a_cleared_environment() {
    let scratch = scratch();
    let dir = scratch.path();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let exe = script(
        dir,
        &format!(
            "exec 3>&-\nls /proc/self/fd > {0}/fds\nenv > {0}/env\ncat >/dev/null",
            dir.display()
        ),
    );
    let result = renderer(exe, u64::MAX, Duration::from_secs(5))
        .render(request())
        .await;
    assert!(matches!(result, Err(RenderError::Worker(_))), "{result:?}");
    drop(listener);
    let fds = std::fs::read_to_string(dir.join("fds")).unwrap();
    let mut fds: Vec<&str> = fds.lines().collect();
    fds.sort_unstable();
    // `ls` opens /proc/self/fd itself as one more descriptor.
    assert_eq!(fds[..3], ["0", "1", "2"]);
    assert!(fds.len() <= 4, "{fds:?}");
    let env = std::fs::read_to_string(dir.join("env")).unwrap();
    assert!(!env.contains("HOME="), "{env}");
    assert!(!env.contains("CARGO"), "{env}");
    assert!(env.contains("RAYON_NUM_THREADS=2"), "{env}");
}

#[tokio::test]
async fn timeout_kills_and_reaps() {
    let scratch = scratch();
    let dir = scratch.path();
    let exe = script(
        dir,
        &format!("echo $$ > {}/pid\nexec sleep 60", dir.display()),
    );
    let result = renderer(exe, u64::MAX, Duration::from_millis(300))
        .render(request())
        .await;
    assert_eq!(result, Err(RenderError::Timeout));
    let pid = std::fs::read_to_string(dir.join("pid")).unwrap();
    wait_gone(pid.trim()).await;
}

#[tokio::test]
async fn memory_limit_kills_and_reaps() {
    let scratch = scratch();
    let dir = scratch.path();
    let exe = script(
        dir,
        &format!("echo $$ > {}/pid\nexec sleep 60", dir.display()),
    );
    let result = renderer(exe, 1, Duration::from_secs(20))
        .render(request())
        .await;
    assert_eq!(result, Err(RenderError::MemoryExceeded));
    let pid = std::fs::read_to_string(dir.join("pid")).unwrap();
    wait_gone(pid.trim()).await;
}

#[tokio::test]
async fn a_dropped_future_kills_the_child() {
    let scratch = scratch();
    let dir = scratch.path();
    let pid_file = dir.join("pid");
    let exe = script(
        dir,
        &format!("echo $$ > {}\nexec sleep 60", pid_file.display()),
    );
    let renderer = renderer(exe, u64::MAX, Duration::from_secs(60));
    let request = request();
    let mut render = Box::pin(renderer.render(request));
    // Other tests may hold the one-compile lock first, so wait for this child to really run.
    let pid = loop {
        tokio::select! {
            result = &mut render => panic!("{result:?}"),
            () = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
        if let Ok(pid) = std::fs::read_to_string(&pid_file)
            && !pid.trim().is_empty()
        {
            break pid;
        }
    };
    drop(render);
    wait_gone(pid.trim()).await;
}

#[tokio::test]
async fn one_compile_at_a_time() {
    let scratch = scratch();
    let dir = scratch.path();
    let exe = script(
        dir,
        &format!(
            "if ! mkdir {0}/lock; then touch {0}/overlap; fi\nsleep 0.2\nrmdir {0}/lock\nexit 1",
            dir.display()
        ),
    );
    let renderer = renderer(exe, u64::MAX, Duration::from_secs(10));
    let (a, b) = tokio::join!(renderer.render(request()), renderer.render(request()));
    assert!(a.is_err() && b.is_err());
    assert!(!dir.join("overlap").exists());
}

#[tokio::test]
async fn a_crash_and_garbage_are_worker_errors() {
    let scratch = scratch();
    let dir = scratch.path();
    for body in [
        "cat >/dev/null\nkill -SEGV $$",
        "cat >/dev/null\necho garbage-output-longer-than-eight-bytes",
    ] {
        let exe = script(dir, body);
        let result = renderer(exe, u64::MAX, Duration::from_secs(5))
            .render(request())
            .await;
        assert!(
            matches!(result, Err(RenderError::Worker(_))),
            "{body}: {result:?}"
        );
    }
}

/// A worker never sends this much: `compile` refuses a larger result itself. So the parent, which
/// holds it all in memory, doesn't read further.
#[tokio::test]
async fn too_much_output_is_cut_off() {
    let scratch = scratch();
    let exe = script(scratch.path(), "cat >/dev/null\nhead -c 20971520 /dev/zero");
    let result = renderer(exe, u64::MAX, Duration::from_secs(20))
        .render(request())
        .await;
    assert_eq!(
        result,
        Err(RenderError::Worker("too much output".to_owned()))
    );
}
