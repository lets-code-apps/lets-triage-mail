//! Let's Triage Mail: local-first email triage for any IMAP account.
//!
//! Nothing is implemented yet. See `docs/SPEC.md` for the v1 plan and
//! `docs/ARCHITECTURE.md` for the module layout this file will grow into.

fn main() {
    // `env!` reads a value at compile time; Cargo sets these from `Cargo.toml`.
    println!(
        "{} {}: early development, nothing to run yet.",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );
}
