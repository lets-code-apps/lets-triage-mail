//! Fresh-process `model pull` (LET-137, review round 1 finding 4 and round 2 finding 2).
//!
//! `install_tls_provider` is a process-wide `std::sync::Once`, so a unit test in the library
//! cannot prove that a standalone pull sets up TLS: an earlier test in the same process already
//! installed the provider and masks the failure. This test spawns the real binary as a brand-new
//! process (finding 4).
//!
//! `reqwest` is built with `rustls-no-provider`, so building ANY client panics if no provider is
//! installed — and the client is built before any socket is opened. So pointing `model pull` at a
//! dead loopback port exercises exactly that construction path: fixed, the client builds, the
//! connection is refused, and the CLI prints one `ok:false` line and exits 1; broken, the client
//! build panics and there is no `ok:false` line at all. No real Ollama, no network beyond loopback.
//!
//! The second test (finding 2) drives the same real binary against a raw loopback HTTP server that
//! streams the pull body slowly: it asserts the first progress line reaches the CLI's stdout WHILE
//! the download is still running (buffered output would only appear after the process exits, which
//! cannot happen before the server finishes). std sockets only: no new dependencies.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::thread::{sleep, spawn};
use std::time::{Duration, Instant};

/// The binary `cargo` builds for integration tests (the real `lets-triage-mail` CLI).
const BIN: &str = env!("CARGO_BIN_EXE_lets-triage-mail");

struct PullResult {
    pub ok: bool,
    pub out: String,
    pub err: String,
}

/// Runs `model pull --url <base> qwen3:4b` as a fresh OS process and captures its output.
fn run_pull(base: &str) -> PullResult {
    let output = Command::new(BIN)
        .args(["model", "pull", "--url", base, "qwen3:4b"])
        .output()
        .expect("the CLI runs");
    PullResult {
        ok: output.status.success(),
        out: String::from_utf8_lossy(&output.stdout).to_string(),
        err: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

#[test]
fn a_standalone_pull_builds_the_client_without_a_tls_panic() {
    // Finding 4: in a fresh process `model pull` must build its client without panicking on a
    // missing rustls crypto provider. A dead port makes the request fail cleanly, so a well-built
    // run leaves an `ok:false` line on stdout and no crypto-provider panic on stderr.
    let result = run_pull("http://127.0.0.1:1");
    if result.err.contains("crypto provider") || result.err.contains("rustls") {
        panic!(
            "the pull panicked building its client (missing TLS provider); stderr was:\n{}",
            result.err,
        );
    }
    if result.out.contains("\"ok\": false") || result.out.contains("\"ok\":false") {
        // The client built and the connection was simply refused: exactly the fixed behaviour.
        // A refused connection exits non-zero, so the process must NOT report success.
        if result.ok {
            panic!("a refused connection must exit non-zero, but the CLI reported success");
        }
        return;
    }
    panic!(
        "expected a clean `ok:false` refusal from a refused connection, but stdout was:\n{}\nstderr:\n{}",
        result.out, result.err,
    );
}

#[test]
fn pull_progress_reaches_stdout_while_the_download_is_still_running() {
    // Round-2 finding 2's fresh-process half: `model pull` must write AND flush each NDJSON line
    // the moment the server sends it. The test server sends ONE `downloading` line and then holds
    // the connection open for two whole seconds before the `success` line. A CLI that buffered its
    // stdout could not hand a line to this test before the child exits, and the child cannot exit
    // before the server closes the body - so a first line that arrives well inside the hold can
    // only be live, flush-per-line progress (see `consume_pull_stream_to` and the emit closure in
    // `cli/model.rs`). std sockets only: no new dependencies.
    let server = TcpListener::bind("127.0.0.1:0").expect("the test server binds a free port");
    let port = server.local_addr().expect("the bound address").port();
    let base = format!("http://127.0.0.1:{port}",);

    // A raw HTTP/1.1 answer with no Content-Length and `Connection: close`: the body runs to the
    // socket close, so reqwest streams it as it arrives, the shape a real model download has.
    // The server first swallows the CLI's request: a socket closed with unread request data
    // left over resets the connection, and reqwest would then report a broken connection rather
    // than the streamed body it was fed.
    let streamer = spawn(move || {
        let (mut client, _) = server
            .accept()
            .expect("the CLI connects to the test server");
        let mut raw: Vec<u8> = vec![];
        let mut one: Vec<u8> = vec![0u8];
        // Request headers, byte by byte, up to the CRLF CRLF that ends them.
        loop {
            if client.read(&mut one[..]).unwrap_or(0) == 0 {
                break;
            }
            raw.push(one[0]);
            let n = raw.len();
            if n >= 4
                && raw[n - 4] == b'\r'
                && raw[n - 3] == b'\n'
                && raw[n - 2] == b'\r'
                && raw[n - 1] == b'\n'
            {
                break;
            }
        }
        // Then exactly the body the `content-length` header declares.
        let headers = String::from_utf8_lossy(&raw[..]).to_lowercase();
        let mut body_bytes = 0;
        for line in headers.lines() {
            if let Some(pair) = line.split_once(":")
                && pair.0.trim() == "content-length"
            {
                body_bytes = pair.1.trim().parse::<usize>().unwrap_or(0);
                break;
            }
        }
        for _ in 0..body_bytes {
            if client.read(&mut one[..]).unwrap_or(0) == 0 {
                break;
            }
        }
        // Now the streamed answer: headers, one progress line, a two-second stall, then success.
        client.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n"
        ).expect("the test server sends its headers");
        client
            .write_all(b"{\"status\":\"downloading abc123def456\"}\n")
            .expect("the progress line");
        client.flush().expect("the progress line is on the wire");
        sleep(Duration::from_secs(2));
        client
            .write_all(b"{\"status\":\"success\"}\n")
            .expect("the success line");
        client.flush().expect("the success line is on the wire");
        // `client` closes with this scope: only now does the CLI see the body end.
    });

    let mut child = Command::new(BIN)
        .args(["model", "pull", "--url", &base, "qwen3:4b"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the CLI starts");

    // Read the child's live pipe up to the end of the FIRST line, with 1.5 s of patience: generous
    // for a slow CI runner, and still inside the server's two-second hold. One byte at a time, so
    // the read cannot swallow the `success` line and hide the regression.
    let started_at = Instant::now();
    let mut raw: Vec<u8> = vec![];
    let mut one: Vec<u8> = vec![0u8];
    loop {
        if Instant::now() - started_at > Duration::from_millis(1500) {
            break;
        }
        let read = child
            .stdout
            .as_mut()
            .expect("the CLI's stdout is piped")
            .read(&mut one[..]);
        if read.unwrap_or(0) == 0 {
            break; // EOF with no line: the CLI exited without printing, the buffered defect.
        }
        raw.push(one[0]);
        if one[0] == b'\n' {
            break;
        }
    }
    let waited: Duration = Instant::now() - started_at;
    let first_line = String::from_utf8_lossy(&raw[..]).to_string();

    // Three assertions, in the order they separate the failure modes: nothing printed (a stall
    // or a broken request), too late (buffered until the body ended), and the wrong line (progress
    // not separated from success).
    if !first_line.contains("\n") {
        panic!("no progress line reached the CLI inside the download window; waited {waited:?}",);
    }
    if waited > Duration::from_millis(1500) {
        panic!("the first progress line only arrived once the download was over: {waited:?}",);
    }
    if !first_line.contains("downloading") || first_line.contains("success") {
        panic!("the first line was not live download progress: {first_line:?}",);
    }

    // Drain the rest of the pipe to EOF: the live-first download must still reach its `success`
    // stage. The exit code is not asserted: this test socket ends the body by closing, which the
    // CLI correctly reports as an interrupted read, and the clean-body exit codes are covered by
    // the library tests in `model_setup.rs`.
    let mut rest: Vec<u8> = vec![];
    let drain_started = Instant::now();
    loop {
        if Instant::now() - drain_started > Duration::from_secs(10) {
            break;
        }
        let read = child
            .stdout
            .as_mut()
            .expect("the CLI's stdout is piped")
            .read(&mut one[..]);
        if read.unwrap_or(0) == 0 {
            break;
        }
        rest.push(one[0]);
    }
    let finished = String::from_utf8_lossy(&rest[..]).to_string();
    if !finished.contains("success") {
        panic!("the live-first download never reached its success stage: {finished:?}",);
    }
    let _ = child.wait();
    let _ = streamer.join();
}
