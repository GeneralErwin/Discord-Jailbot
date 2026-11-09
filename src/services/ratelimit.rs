//! Outbound Discord API governor.
//!
//! Discord enforces per-route buckets *and* a global request ceiling. Twilight
//! already retries 429 responses, but a burst (for example repairing a jail
//! state across every channel of a busy server) still saturates the client's
//! HTTP queue, delays unrelated requests and can get the bot flagged.
//!
//! Every request that can appear in bulk goes through [`guard`], which reserves
//! a send slot before the request is polled. Slots are handed out in order, so
//! the bot paces itself instead of waiting to be throttled, and a real 429 arms
//! a shared cooldown that every lane respects.

use std::future::Future;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Semaphore, SemaphorePermit};
use twilight_http::error::{Error as HttpError, ErrorType};

/// How many requests may be in flight at the same time.
const MAX_IN_FLIGHT: usize = 8;

/// Minimum gap between any two outbound requests, across all lanes.
/// Discord's global ceiling is 50 requests per second; this stays well under it.
const GLOBAL_INTERVAL: Duration = Duration::from_millis(25);

/// First cooldown applied after a 429. Doubles for each following hit.
const BASE_COOLDOWN: Duration = Duration::from_secs(2);

/// A cooldown never grows beyond this.
const MAX_COOLDOWN: Duration = Duration::from_secs(30);

/// Request classes with their own minimum spacing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    /// Read-only lookups.
    Read,
    /// Interaction acknowledgements and responses.
    Interaction,
    /// Role, nickname and membership edits.
    Member,
    /// Channel permission overwrite edits.
    Overwrite,
    /// Anything that creates or edits a message.
    Message,
}

impl Lane {
    fn index(self) -> usize {
        match self {
            Self::Read => 0,
            Self::Interaction => 1,
            Self::Member => 2,
            Self::Overwrite => 3,
            Self::Message => 4,
        }
    }

    fn interval(self) -> Duration {
        match self {
            Self::Read => Duration::from_millis(30),
            Self::Interaction => Duration::from_millis(50),
            Self::Member => Duration::from_millis(120),
            Self::Overwrite => Duration::from_millis(150),
            Self::Message => Duration::from_millis(300),
        }
    }
}

struct State {
    /// Earliest instant at which the next request may be sent.
    global_next: Instant,
    /// Per-lane pacing, so slow lanes cannot starve fast ones.
    lane_next: [Instant; 5],
    /// While set, no request is sent at all.
    cooldown_until: Instant,
    /// Consecutive 429 hits; drives the exponential cooldown.
    penalty: u32,
}

struct Governor {
    state: Mutex<State>,
    in_flight: Semaphore,
}

fn governor() -> &'static Governor {
    static GOVERNOR: OnceLock<Governor> = OnceLock::new();

    GOVERNOR.get_or_init(|| {
        let now = Instant::now();

        Governor {
            state: Mutex::new(State {
                global_next: now,
                lane_next: [now; 5],
                cooldown_until: now,
                penalty: 0,
            }),
            in_flight: Semaphore::new(MAX_IN_FLIGHT),
        }
    })
}

/// Holds a concurrency slot for the lifetime of one request.
pub struct Permit {
    _slot: Option<SemaphorePermit<'static>>,
}

/// Runs one Discord request behind the governor.
///
/// The request future is only polled once a send slot has been reserved, so a
/// large loop of calls cannot burst past the global limit.
pub async fn guard<F, T>(lane: Lane, request: F) -> Result<T, HttpError>
where
    F: Future<Output = Result<T, HttpError>>,
{
    let permit = reserve(lane).await;
    let result = request.await;

    observe(&result).await;

    drop(permit);

    result
}

/// Reserves the next allowed send instant for a lane and waits for it.
async fn reserve(lane: Lane) -> Permit {
    let governor = governor();

    let slot = {
        let mut state = governor.state.lock().await;
        let now = Instant::now();

        let slot = now
            .max(state.global_next)
            .max(state.lane_next[lane.index()])
            .max(state.cooldown_until);

        state.global_next = slot + GLOBAL_INTERVAL;
        state.lane_next[lane.index()] = slot + lane.interval();

        slot
    };

    // Acquired after the slot is reserved, so waiters queue in send order.
    let permit = governor.in_flight.acquire().await.ok();

    let now = Instant::now();

    if slot > now {
        tokio::time::sleep(slot - now).await;
    }

    Permit { _slot: permit }
}

/// Feeds the result of a request back into the governor.
async fn observe<T>(result: &Result<T, HttpError>) {
    match result {
        Ok(_) => {
            let mut state = governor().state.lock().await;
            state.penalty = state.penalty.saturating_sub(1);
        }
        Err(error) => {
            if !is_throttled(error) {
                return;
            }

            let mut state = governor().state.lock().await;
            let now = Instant::now();

            state.penalty = (state.penalty + 1).min(5);

            let cooldown = (BASE_COOLDOWN * (1_u32 << state.penalty)).min(MAX_COOLDOWN);

            state.cooldown_until = state.cooldown_until.max(now + cooldown);

            tracing::warn!(
                cooldown_ms = cooldown.as_millis() as u64,
                level = state.penalty,
                "Discord rate limit reached; pausing outbound requests"
            );
        }
    }
}

fn is_throttled(error: &HttpError) -> bool {
    matches!(
        error.kind(),
        ErrorType::Response { status, .. } if *status == 429
    )
}
