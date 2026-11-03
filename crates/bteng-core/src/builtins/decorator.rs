//! Decorator tick logic.
//!
//! Every decorator passes `Skipped` through: a child that did not run has no
//! outcome to force, invert, repeat or retry. A decorator that keeps progress
//! (`Repeat`, `RetryUntilSuccessful`, `Timeout`, `Delay`) treats a skipped child
//! as the end of its activation and clears that progress, exactly as it does on
//! `Success`/`Failure` — a skipped node is not `Running`, so `halt()` would
//! never get the chance to clear it later.

use super::{msec_to_nanos, now_of, ports};
use crate::clock::Nanos;
use crate::node::Cursor;
use crate::status::Status;

/// `ForceSuccess`/`ForceFailure`: a fixed status for a child that completed.
///
/// `Running` and `Skipped` pass through, as in BT.CPP: forcing a skipped child
/// would make up an outcome for work that never ran.
pub(super) fn force(cur: &mut Cursor<'_, '_>, forced: Status) -> Status {
    match cur.tick_child(0) {
        s @ (Status::Running | Status::Skipped) => s,
        Status::Success | Status::Failure => forced,
    }
}

pub(super) fn keep_running(cur: &mut Cursor<'_, '_>) -> Status {
    match cur.tick_child(0) {
        Status::Failure => Status::Failure,
        Status::Skipped => Status::Skipped,
        Status::Running | Status::Success => Status::Running,
    }
}

/// Repeat the child until it has succeeded `num_cycles` times.
///
/// **One cycle per tick**, unlike BT.CPP, which loops inside a single tick until
/// the child returns RUNNING. Looping there makes the cost of one tick depend on
/// how fast the child settles, and a child that settles instantly makes it
/// unbounded — which DR-4 rules out. The behaviour a tree observes is the same;
/// it just takes as many ticks as cycles.
pub(super) fn repeat(completed: &mut u32, cur: &mut Cursor<'_, '_>) -> Status {
    let num_cycles = cur.get_or::<i32>(ports::REPEAT_NUM_CYCLES, 1);
    let forever = num_cycles < 0;

    if !forever && *completed >= num_cycles.unsigned_abs() {
        *completed = 0;
        return Status::Success;
    }

    match cur.tick_child(0) {
        Status::Running => Status::Running,
        Status::Failure => {
            *completed = 0;
            Status::Failure
        }
        Status::Skipped => {
            // BT.CPP keeps its counter here; this engine cannot, because a
            // skipped Repeat is not Running and so is never `halt()`ed — a kept
            // counter would leak into the next activation.
            *completed = 0;
            cur.halt_child(0);
            Status::Skipped
        }
        Status::Success => {
            *completed += 1;
            // Return the child to Idle so the next cycle starts it fresh.
            cur.halt_child(0);
            if !forever && *completed >= num_cycles.unsigned_abs() {
                *completed = 0;
                Status::Success
            } else {
                Status::Running
            }
        }
    }
}

/// Retry the child after a failure, up to `num_attempts` times.
///
/// One attempt per tick, for the reason given on [`repeat`].
pub(super) fn retry(failures: &mut u32, cur: &mut Cursor<'_, '_>) -> Status {
    let num_attempts = cur.get_or::<i32>(ports::RETRY_NUM_ATTEMPTS, 1);
    let forever = num_attempts < 0;

    match cur.tick_child(0) {
        Status::Running => Status::Running,
        Status::Success => {
            *failures = 0;
            Status::Success
        }
        Status::Skipped => {
            *failures = 0;
            cur.halt_child(0);
            Status::Skipped
        }
        Status::Failure => {
            *failures += 1;
            cur.halt_child(0);
            if !forever && *failures >= num_attempts.unsigned_abs() {
                *failures = 0;
                Status::Failure
            } else {
                Status::Running
            }
        }
    }
}

/// Run the child to completion once; afterwards, never tick it again.
///
/// After the one run, as in BT.CPP: `then_skip="true"` (the default) returns
/// `Skipped`, so a parent passes over the spent node; `then_skip="false"`
/// replays the status the child completed with. Only `Success`/`Failure` count
/// as the one run — a child that was itself skipped has not run yet.
pub(super) fn run_once(done: &mut bool, cached: &mut Status, cur: &mut Cursor<'_, '_>) -> Status {
    if *done {
        return if cur.get_or::<bool>(ports::RUN_ONCE_THEN_SKIP, true) {
            Status::Skipped
        } else {
            *cached
        };
    }

    let status = cur.tick_child(0);
    if status.is_completed() {
        *done = true;
        *cached = status;
    }
    status
}

/// Fail and halt the child if it is still running after `msec`.
pub(super) fn timeout(deadline: &mut Nanos, armed: &mut bool, cur: &mut Cursor<'_, '_>) -> Status {
    if !*armed {
        // u64::MAX on a misread port means "never fire", which is the safe
        // direction: a misconfigured timeout must not manufacture a Failure the
        // tree would act on.
        let msec = cur.get_or::<u64>(ports::TIMEOUT_MSEC, u64::MAX);
        *deadline = now_of(cur).saturating_add(msec_to_nanos(msec));
        *armed = true;
    }

    let status = cur.tick_child(0);
    if status.is_settled() {
        *armed = false;
        return status;
    }

    if now_of(cur) >= *deadline {
        cur.halt_child(0);
        *armed = false;
        return Status::Failure;
    }
    Status::Running
}

/// Wait `delay_msec` before first ticking the child.
pub(super) fn delay(fire_at: &mut Nanos, armed: &mut bool, cur: &mut Cursor<'_, '_>) -> Status {
    if !*armed {
        let msec = cur.get_or::<u64>(ports::DELAY_MSEC, 0);
        *fire_at = now_of(cur).saturating_add(msec_to_nanos(msec));
        *armed = true;
    }

    if now_of(cur) < *fire_at {
        return Status::Running;
    }

    let status = cur.tick_child(0);
    if status.is_settled() {
        *armed = false;
    }
    status
}
