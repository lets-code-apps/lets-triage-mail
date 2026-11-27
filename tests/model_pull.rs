//! Fresh-process `model pull` (LET-137, review round 1).
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

use std::process::Command;

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
