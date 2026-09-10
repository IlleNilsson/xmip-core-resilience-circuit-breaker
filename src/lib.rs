#![forbid(unsafe_code)]

//! The circuit breaker guard — a technology of `xmip-core-resilience`
//! (ADR-0048).
//!
//! Consecutive failures up to a threshold open the circuit; while it is open
//! every attempt is refused without being made. Once the open period has
//! passed the circuit is half-open: one trial attempt is let through, and a
//! success closes the circuit while a failure opens it again for another
//! period. A success at any time forgets the failures counted so far. The
//! guard keeps its state behind a lock, so one breaker may stand in front of
//! many callers.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use resilience::{Attempt, Decision, Guard};

/// Where the circuit stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Attempts go; failures are counted.
    Closed,
    /// Attempts are refused until the open period has passed.
    Open,
    /// The open period has passed; one trial attempt decides.
    HalfOpen,
}

#[derive(Debug)]
struct Inner {
    failures: u32,
    opened_at: Option<Instant>,
    trial_in_flight: bool,
}

/// The circuit breaker guard.
#[derive(Debug)]
pub struct CircuitBreaker {
    failure_threshold: u32,
    open_for: Duration,
    inner: Mutex<Inner>,
}

impl CircuitBreaker {
    /// Open after `failure_threshold` consecutive failures, for `open_for`.
    /// A threshold of zero is taken as one.
    #[must_use]
    pub fn new(failure_threshold: u32, open_for: Duration) -> Self {
        Self {
            failure_threshold: failure_threshold.max(1),
            open_for,
            inner: Mutex::new(Inner {
                failures: 0,
                opened_at: None,
                trial_in_flight: false,
            }),
        }
    }

    /// Where the circuit stands now.
    #[must_use]
    pub fn state(&self) -> State {
        Self::state_of(&self.lock(), self.open_for)
    }

    /// Consecutive failures counted while closed.
    #[must_use]
    pub fn failures(&self) -> u32 {
        self.lock().failures
    }

    fn state_of(inner: &Inner, open_for: Duration) -> State {
        match inner.opened_at {
            None => State::Closed,
            Some(opened_at) if opened_at.elapsed() >= open_for => State::HalfOpen,
            Some(_) => State::Open,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Guard for CircuitBreaker {
    fn technology(&self) -> &'static str {
        "circuit-breaker"
    }

    fn before(&self, _: u32) -> Decision {
        let mut inner = self.lock();
        match Self::state_of(&inner, self.open_for) {
            State::Closed => Decision::Proceed,
            State::Open => {
                let remaining = inner.opened_at.map_or(Duration::ZERO, |opened_at| {
                    self.open_for.saturating_sub(opened_at.elapsed())
                });
                Decision::Refuse(format!(
                    "the circuit is open for {}ms more",
                    remaining.as_millis()
                ))
            }
            State::HalfOpen if inner.trial_in_flight => {
                Decision::Refuse("the circuit is half-open and a trial is in flight".into())
            }
            State::HalfOpen => {
                inner.trial_in_flight = true;
                Decision::Proceed
            }
        }
    }

    fn after(&self, attempt: &Attempt) -> Decision {
        let mut inner = self.lock();
        if attempt.succeeded() {
            inner.failures = 0;
            inner.opened_at = None;
            inner.trial_in_flight = false;
        } else if inner.opened_at.is_some() {
            inner.opened_at = Some(Instant::now());
            inner.trial_in_flight = false;
        } else {
            inner.failures += 1;
            if inner.failures >= self.failure_threshold {
                inner.failures = 0;
                inner.opened_at = Some(Instant::now());
            }
        }
        Decision::Proceed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use resilience::{Failure, Guarded, execute};
    use std::cell::Cell;

    fn failed() -> Attempt {
        Attempt {
            number: 1,
            elapsed: Duration::ZERO,
            failure: Some(Failure::retryable("again")),
        }
    }

    fn succeeded() -> Attempt {
        Attempt {
            number: 1,
            elapsed: Duration::ZERO,
            failure: None,
        }
    }

    #[test]
    fn failures_up_to_the_threshold_open_the_circuit_and_attempts_are_refused() {
        let breaker = CircuitBreaker::new(2, Duration::from_secs(60));
        assert_eq!(breaker.technology(), "circuit-breaker");
        assert_eq!(breaker.state(), State::Closed);
        assert_eq!(breaker.before(1), Decision::Proceed);
        assert_eq!(breaker.after(&failed()), Decision::Proceed);
        assert_eq!(breaker.failures(), 1);
        assert_eq!(breaker.state(), State::Closed);
        assert_eq!(breaker.after(&failed()), Decision::Proceed);
        assert_eq!(breaker.state(), State::Open);
        match breaker.before(3) {
            Decision::Refuse(reason) => assert!(reason.contains("open"), "{reason}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_success_forgets_the_failures_counted_so_far() {
        let breaker = CircuitBreaker::new(3, Duration::from_secs(60));
        breaker.after(&failed());
        breaker.after(&failed());
        assert_eq!(breaker.failures(), 2);
        breaker.after(&succeeded());
        assert_eq!(breaker.failures(), 0);
        breaker.after(&failed());
        breaker.after(&failed());
        assert_eq!(breaker.state(), State::Closed);
    }

    #[test]
    fn after_the_open_period_one_trial_goes_and_its_outcome_decides() {
        let breaker = CircuitBreaker::new(1, Duration::from_millis(2));
        breaker.after(&failed());
        assert_eq!(breaker.state(), State::Open);
        std::thread::sleep(Duration::from_millis(4));
        assert_eq!(breaker.state(), State::HalfOpen);
        assert_eq!(breaker.before(1), Decision::Proceed, "the trial goes");
        assert!(
            matches!(breaker.before(1), Decision::Refuse(_)),
            "one at a time"
        );
        breaker.after(&failed());
        assert_eq!(breaker.state(), State::Open, "a failed trial re-opens");
        std::thread::sleep(Duration::from_millis(4));
        assert_eq!(breaker.before(1), Decision::Proceed);
        breaker.after(&succeeded());
        assert_eq!(breaker.state(), State::Closed, "a successful trial closes");
        assert_eq!(breaker.before(1), Decision::Proceed);
    }

    #[test]
    fn under_execute_an_open_circuit_refuses_without_running_the_operation() {
        let breaker = CircuitBreaker::new(1, Duration::from_secs(60));
        let guards: [&dyn Guard; 1] = [&breaker];
        let calls = Cell::new(0);
        let first: Result<Guarded<()>, Failure> = execute(&guards, || {
            calls.set(calls.get() + 1);
            Err(Failure::permanent("broken"))
        });
        assert_eq!(first, Err(Failure::permanent("broken")));
        assert_eq!(calls.get(), 1);
        let second = execute(&guards, || {
            calls.set(calls.get() + 1);
            Ok(())
        });
        assert!(matches!(second, Ok(Guarded::Refused(_))), "{second:?}");
        assert_eq!(calls.get(), 1, "refused before any attempt");
    }

    /// Tries again on a retryable failure, as the retry technology does.
    struct Again(u32);

    impl Guard for Again {
        fn technology(&self) -> &'static str {
            "retry"
        }

        fn before(&self, _: u32) -> Decision {
            Decision::Proceed
        }

        fn after(&self, attempt: &Attempt) -> Decision {
            match &attempt.failure {
                Some(failure) if failure.is_retryable() && attempt.number < self.0 => {
                    Decision::Wait(Duration::ZERO)
                }
                _ => Decision::Proceed,
            }
        }
    }

    #[test]
    fn a_breaker_ahead_of_retry_stops_the_retries_once_it_opens() {
        let breaker = CircuitBreaker::new(2, Duration::from_secs(60));
        let guards: [&dyn Guard; 2] = [&breaker, &Again(5)];
        let calls = Cell::new(0);
        let outcome: Result<Guarded<()>, Failure> = execute(&guards, || {
            calls.set(calls.get() + 1);
            Err(Failure::retryable("again"))
        });
        assert!(matches!(outcome, Ok(Guarded::Refused(_))), "{outcome:?}");
        assert_eq!(
            calls.get(),
            2,
            "two failures opened it; the third was refused"
        );
        assert_eq!(breaker.state(), State::Open);
    }
}
