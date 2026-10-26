//! When the digest is due: the pure decision, and the loop that acts on it.
//!
//! The rule is one line: **a digest is due once the configured time of day has passed and
//! today's has not been written yet.** That one rule covers three situations:
//!
//! - normal days: it becomes due at 07:00 and is written within a minute;
//! - a Mac that was asleep at 07:00: the first check after waking finds it due (catch-up);
//! - a first start at 15:00: today's digest is missing and the time has passed, so it is
//!   written at once.
//!
//! `last_digest_date` (in the store) records the day of the last digest, so a day never gets
//! two, however often the loop looks.
//!
//! # Why the loop polls
//!
//! Sleeping "until 07:00" in one go is wrong on a laptop or a Mac that sleeps: tokio's timers
//! use a monotonic clock that does not run while the machine is asleep on macOS, so the
//! sleep would end late. Instead the loop wakes every [`DigestTiming::poll`] (a minute) and
//! asks the wall clock again. The check costs one small database read.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::civil;
use tokio_util::sync::CancellationToken;

use super::deliver::{DigestError, send_digest, with_store};
use crate::config::{AccountConfig, Config};
use crate::store::Store;

/// How often the loop looks at the clock, and how long it waits after a failure.
#[derive(Debug, Clone)]
pub struct DigestTiming {
    /// Time between two looks at the clock.
    pub poll: Duration,
    /// Wait before trying again after a failed delivery (server down, folder missing).
    pub retry: Duration,
}

impl Default for DigestTiming {
    fn default() -> Self {
        DigestTiming {
            poll: Duration::from_secs(60),
            retry: Duration::from_secs(15 * 60),
        }
    }
}

/// What the loop should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Write the digest now.
    SendNow,
    /// Nothing to do for this long (at most; the loop may look earlier).
    WaitFor(Duration),
}

/// Decides the next step.
///
/// - `now` is the local wall-clock time.
/// - `at` is the configured `(hour, minute)`.
/// - `last_date` is the day (`YYYY-MM-DD`) of the last digest, if any. Text that is not a
///   date counts as "never", so a damaged value cannot silence the digest.
///
/// A `last_date` that is today **or later** means "done for today": if the clock was set back,
/// we wait instead of sending a second digest.
pub fn next_step(now: civil::DateTime, at: (u8, u8), last_date: Option<&str>) -> Step {
    let today = now.date();
    let done_today = last_date
        .and_then(|text| text.parse::<civil::Date>().ok())
        .is_some_and(|last| last >= today);
    let slot_today = today.at(at.0 as i8, at.1 as i8, 0, 0);

    if !done_today && now >= slot_today {
        return Step::SendNow;
    }

    // Not due yet: today's slot if it is still ahead and unused, otherwise tomorrow's.
    let target = if !done_today && now < slot_today {
        slot_today
    } else {
        // `tomorrow()` only fails at the very end of the supported dates.
        today.tomorrow().map_or(slot_today, |tomorrow| {
            tomorrow.at(at.0 as i8, at.1 as i8, 0, 0)
        })
    };
    let wait = now
        .duration_until(target)
        .try_into()
        .unwrap_or(Duration::ZERO);
    Step::WaitFor(wait)
}

/// Writes this account's digest every day, until `stop` fires.
///
/// A failure is logged and retried after [`DigestTiming::retry`]; it never touches the inbox
/// worker, which runs next to this loop in the same account unit.
pub async fn digest_loop(
    account: &AccountConfig,
    config: &Config,
    store: &Arc<Mutex<Store>>,
    stop: &CancellationToken,
    timing: &DigestTiming,
) {
    // Always `Some` after config validation; a config that skipped it gets no digest.
    let Some(at) = config.digest.time_of_day() else {
        return;
    };

    loop {
        let wait = match one_round(account, config, store, at, timing).await {
            Ok(wait) => wait,
            Err(error) => {
                tracing::warn!(
                    account = %account.name, %error,
                    retry_in_secs = timing.retry.as_secs(),
                    "daily digest failed; the inbox worker is not affected"
                );
                timing.retry
            }
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = stop.cancelled() => return,
        }
    }
}

/// Looks at the clock once and sends the digest if it is due. Returns how long to sleep.
async fn one_round(
    account: &AccountConfig,
    config: &Config,
    store: &Arc<Mutex<Store>>,
    at: (u8, u8),
    timing: &DigestTiming,
) -> Result<Duration, DigestError> {
    let now = jiff::Zoned::now();
    let name = account.name.clone();
    let last = with_store(store, move |s| s.last_digest_date(&name)).await?;

    match next_step(now.datetime(), at, last.as_deref()) {
        Step::WaitFor(wait) => Ok(wait.min(timing.poll)),
        Step::SendNow => {
            let delivery = send_digest(account, config, store, &now).await?;
            tracing::info!(account = %account.name, ?delivery, "daily digest written");
            // The next look finds today's digest recorded and waits for tomorrow.
            Ok(timing.poll)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(h: i8, m: i8) -> civil::DateTime {
        civil::date(2026, 10, 26).at(h, m, 0, 0)
    }

    const SEVEN: (u8, u8) = (7, 0);

    #[test]
    fn before_the_time_it_waits_for_it() {
        assert_eq!(
            next_step(at(6, 30), SEVEN, Some("2026-10-25")),
            Step::WaitFor(Duration::from_secs(30 * 60))
        );
    }

    #[test]
    fn at_the_time_it_sends() {
        assert_eq!(
            next_step(at(7, 0), SEVEN, Some("2026-10-25")),
            Step::SendNow
        );
        assert_eq!(
            next_step(at(7, 1), SEVEN, Some("2026-10-25")),
            Step::SendNow
        );
    }

    #[test]
    fn after_a_sleep_it_catches_up() {
        // The Mac was asleep from 06:00 to 11:45; the first look after waking sends it.
        assert_eq!(
            next_step(at(11, 45), SEVEN, Some("2026-10-25")),
            Step::SendNow
        );
        // A missed day, too: the last digest was three days ago.
        assert_eq!(
            next_step(at(11, 45), SEVEN, Some("2026-10-23")),
            Step::SendNow
        );
    }

    #[test]
    fn a_first_start_after_the_time_sends_at_once() {
        assert_eq!(next_step(at(15, 0), SEVEN, None), Step::SendNow);
    }

    #[test]
    fn one_digest_per_day() {
        // Sent today: wait for tomorrow's slot (07:00 tomorrow is 15h30 after 15:30).
        assert_eq!(
            next_step(at(15, 30), SEVEN, Some("2026-10-26")),
            Step::WaitFor(Duration::from_secs((15 * 60 + 30) * 60))
        );
        // Sent today but the time has not even come (set by a manual `digest` run).
        assert_eq!(
            next_step(at(6, 0), SEVEN, Some("2026-10-26")),
            Step::WaitFor(Duration::from_secs(60 * 60 * 25))
        );
    }

    #[test]
    fn a_clock_set_back_does_not_send_twice() {
        assert_eq!(
            next_step(at(8, 0), SEVEN, Some("2026-10-27")),
            Step::WaitFor(Duration::from_secs(23 * 60 * 60))
        );
    }

    #[test]
    fn a_damaged_date_counts_as_never_sent() {
        assert_eq!(next_step(at(8, 0), SEVEN, Some("garbage")), Step::SendNow);
    }

    #[test]
    fn midnight_is_a_valid_time() {
        assert_eq!(next_step(at(0, 0), (0, 0), None), Step::SendNow);
    }
}
