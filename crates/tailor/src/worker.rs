//! The child process, and the parent's side of it.

use std::io::{Read, Write};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::time::MissedTickBehavior;

use crate::{
    CompleteRequest, Completions, Diagnostic, MAX_RENDER_RESULT_BYTES, MAX_TEXT_BYTES, RenderError,
    RenderRequest, Rendered, RenderedText,
};

/// Above what a limited compile returns: a result at its limit, the text at its limit, and a full
/// list of diagnostics, which `compile.rs` keeps well under 1 MiB. The worker refuses a larger result
/// itself, so reaching this means the child is not behaving as it should.
const MAX_OUTPUT_BYTES: u64 = (MAX_RENDER_RESULT_BYTES + MAX_TEXT_BYTES + 1024 * 1024) as u64;
/// A compile holds hundreds of megabytes, so only one runs at a time.
static ONE_COMPILE: Semaphore = Semaphore::const_new(1);
const RSS_POLL: Duration = Duration::from_millis(20);
/// Typst lays out pages in parallel. The unit's `TasksMax` and the board's cores set the bound.
const RENDER_THREADS: &str = "2";

/// The first frame on stdout. A PDF and the previews follow as frames of their own.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Head {
    has_pdf: bool,
    preview_count: u32,
    page_count: u32,
    text: RenderedText,
    warnings: Vec<Diagnostic>,
}

fn write_frame(out: &mut impl Write, frame: &[u8]) -> std::io::Result<()> {
    out.write_all(&(frame.len() as u64).to_le_bytes())?;
    out.write_all(frame)
}

fn next_frame<'a>(output: &mut &'a [u8]) -> Result<&'a [u8], RenderError> {
    let truncated = || RenderError::Worker("truncated output".to_owned());
    let (len, rest) = output.split_first_chunk::<8>().ok_or_else(truncated)?;
    let len = usize::try_from(u64::from_le_bytes(*len)).map_err(|_| truncated())?;
    if rest.len() < len {
        return Err(truncated());
    }
    let (frame, rest) = rest.split_at(len);
    *output = rest;
    Ok(frame)
}

/// What a child is asked for. A render's JSON is its [`RenderRequest`] and nothing around it.
#[derive(Deserialize)]
#[serde(untagged)]
enum Job {
    Render(RenderRequest),
    Complete(CompleteRequest),
}

enum Done {
    Rendered(Box<Rendered>),
    Completions(Completions),
}

/// What `nexus __render` runs. Reads one JSON [`RenderRequest`] or [`CompleteRequest`] from stdin and
/// answers on stdout.
pub fn run_worker() -> ! {
    // Under memory pressure the kernel takes the compile, never the server. Worth trying, not worth
    // failing a render for: the parent's RSS watch is the real limit.
    let _ = std::fs::write("/proc/self/oom_score_adj", "1000");
    let mut input = Vec::new();
    let result = std::io::stdin()
        .lock()
        .read_to_end(&mut input)
        .map_err(|e| RenderError::Worker(e.to_string()))
        .and_then(|_| {
            serde_json::from_slice(&input).map_err(|e| RenderError::Worker(e.to_string()))
        })
        .and_then(|job: Job| match job {
            Job::Render(req) => {
                crate::compile::render(&req).map(|rendered| Done::Rendered(Box::new(rendered)))
            }
            Job::Complete(req) => crate::compile::complete(&req).map(Done::Completions),
        });
    let mut out = std::io::stdout().lock();
    let json_frame = |out: &mut std::io::StdoutLock, json: serde_json::Result<Vec<u8>>| {
        let json = json.map_err(std::io::Error::other)?;
        write_frame(out, &json)
    };
    let written = match result {
        Ok(Done::Rendered(rendered)) => write_rendered(&mut out, *rendered),
        Ok(Done::Completions(completions)) => {
            let answer = Ok::<_, RenderError>(completions);
            json_frame(&mut out, serde_json::to_vec(&answer))
        }
        // The same frame says why to whichever the parent asked for.
        Err(error) => json_frame(&mut out, serde_json::to_vec(&Err::<Head, _>(error))),
    };
    let flushed = written.and_then(|()| out.flush());
    std::process::exit(i32::from(flushed.is_err()))
}

fn write_rendered(out: &mut impl Write, rendered: Rendered) -> std::io::Result<()> {
    let head = Ok::<_, RenderError>(Head {
        has_pdf: rendered.pdf.is_some(),
        preview_count: rendered.previews.len() as u32,
        page_count: rendered.page_count,
        text: rendered.text,
        warnings: rendered.warnings,
    });
    write_frame(
        out,
        &serde_json::to_vec(&head).map_err(std::io::Error::other)?,
    )?;
    for frame in rendered.pdf.iter().chain(&rendered.previews) {
        write_frame(out, frame)?;
    }
    Ok(())
}

fn parse_output(output: &[u8]) -> Result<Rendered, RenderError> {
    let mut rest = output;
    let bad = |e: serde_json::Error| RenderError::Worker(format!("bad output: {e}"));
    let head: Result<Head, RenderError> =
        serde_json::from_slice(next_frame(&mut rest)?).map_err(bad)?;
    let head = head?;
    let pdf = if head.has_pdf {
        Some(next_frame(&mut rest)?.to_vec())
    } else {
        None
    };
    let previews = (0..head.preview_count)
        .map(|_| next_frame(&mut rest).map(<[u8]>::to_vec))
        .collect::<Result<_, _>>()?;
    Ok(Rendered {
        pdf,
        previews,
        page_count: head.page_count,
        text: head.text,
        warnings: head.warnings,
    })
}

/// The child's resident set, or none when it has just gone away.
fn rss_bytes(pid: u32) -> Option<u64> {
    let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let pages: u64 = statm.split_ascii_whitespace().nth(1)?.parse().ok()?;
    Some(pages * rustix::param::page_size() as u64)
}

pub(crate) async fn render_in_child(
    exe: &Path,
    rss_limit_bytes: u64,
    timeout: Duration,
    req: &RenderRequest,
) -> Result<Rendered, RenderError> {
    let input = serde_json::to_vec(req).map_err(|e| RenderError::Worker(e.to_string()))?;
    parse_output(&run_child(exe, rss_limit_bytes, timeout, &input).await?)
}

pub(crate) async fn complete_in_child(
    exe: &Path,
    rss_limit_bytes: u64,
    timeout: Duration,
    req: &CompleteRequest,
) -> Result<Completions, RenderError> {
    let input = serde_json::to_vec(req).map_err(|e| RenderError::Worker(e.to_string()))?;
    let output = run_child(exe, rss_limit_bytes, timeout, &input).await?;
    serde_json::from_slice(next_frame(&mut output.as_slice())?)
        .map_err(|e| RenderError::Worker(format!("bad output: {e}")))?
}

/// One job in a child of its own, which ends with this future, within the limits. Gives what the
/// child wrote.
async fn run_child(
    exe: &Path,
    rss_limit_bytes: u64,
    timeout: Duration,
    input: &[u8],
) -> Result<Vec<u8>, RenderError> {
    let worker_error = |e: std::io::Error| RenderError::Worker(e.to_string());
    let _one_compile = ONE_COMPILE
        .acquire()
        .await
        .map_err(|e| RenderError::Worker(e.to_string()))?;
    // Stdio is explicit and Rust opens every other descriptor close-on-exec, so the child holds
    // none of the parent's sockets, database files or logs.
    let mut child = Command::new(exe)
        .arg("__render")
        .env_clear()
        .env("RAYON_NUM_THREADS", RENDER_THREADS)
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(worker_error)?;
    let pid = child
        .id()
        .ok_or_else(|| RenderError::Worker("no pid".to_owned()))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| RenderError::Worker("no stdin".to_owned()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| RenderError::Worker("no stdout".to_owned()))?;
    let exchange = async {
        let write = async {
            // The child may die before reading it all. Its exit status tells why.
            let _ = stdin.write_all(input).await;
            drop(stdin);
        };
        let mut output = Vec::new();
        let mut stdout = stdout.take(MAX_OUTPUT_BYTES + 1);
        let read = stdout.read_to_end(&mut output);
        let (_, read) = tokio::join!(write, read);
        read.map(|_| output)
    };
    let watch_rss = async {
        let mut ticks = tokio::time::interval(RSS_POLL);
        // Over MemoryHigh the kernel stalls every read here for up to seconds. Bursts of late ticks
        // would then hold this task for minutes and starve the timeout.
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            if rss_bytes(pid).is_some_and(|rss| rss > rss_limit_bytes) {
                return;
            }
        }
    };
    let output = tokio::select! {
        biased;
        () = tokio::time::sleep(timeout) => Err(RenderError::Timeout),
        output = exchange => output.map_err(worker_error),
        () = watch_rss => Err(RenderError::MemoryExceeded),
    };
    let output = match output {
        Ok(output) if (output.len() as u64) <= MAX_OUTPUT_BYTES => output,
        Ok(_) => {
            let _ = child.kill().await;
            return Err(RenderError::Worker("too much output".to_owned()));
        }
        Err(error) => {
            let _ = child.kill().await;
            return Err(error);
        }
    };
    let status = tokio::time::timeout(timeout, child.wait()).await;
    let status = match status {
        Ok(status) => status.map_err(worker_error)?,
        Err(_) => {
            let _ = child.kill().await;
            return Err(RenderError::Timeout);
        }
    };
    if !status.success() {
        return Err(RenderError::Worker(format!("worker ended with {status}")));
    }
    Ok(output)
}
