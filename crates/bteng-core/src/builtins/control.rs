//! Control-node tick logic.

use super::{MAX_PARALLEL_CHILDREN, ports, threshold};
use crate::node::Cursor;
use crate::status::Status;

/// Children left to right, remembering where it got to.
pub(super) fn sequence(cursor: &mut u16, cur: &mut Cursor<'_, '_>) -> Status {
    let n = cur.child_count();
    while *cursor < n {
        match cur.tick_child(*cursor) {
            Status::Running => return Status::Running,
            Status::Failure => {
                *cursor = 0;
                cur.halt_all_children();
                return Status::Failure;
            }
            Status::Success => *cursor += 1,
        }
    }
    *cursor = 0;
    cur.halt_all_children();
    Status::Success
}

/// As [`sequence`], but a `Failure` does not rewind.
///
/// The next activation retries the child that failed rather than restarting the
/// whole sequence — which is the entire reason this node exists, so the cursor
/// survives and the children are left alone.
pub(super) fn sequence_with_memory(cursor: &mut u16, cur: &mut Cursor<'_, '_>) -> Status {
    let n = cur.child_count();
    while *cursor < n {
        match cur.tick_child(*cursor) {
            Status::Running => return Status::Running,
            Status::Failure => return Status::Failure,
            Status::Success => *cursor += 1,
        }
    }
    *cursor = 0;
    cur.halt_all_children();
    Status::Success
}

/// Re-evaluates every child from the first, every tick.
pub(super) fn reactive_sequence(cur: &mut Cursor<'_, '_>) -> Status {
    let n = cur.child_count();
    for i in 0..n {
        match cur.tick_child(i) {
            Status::Running => {
                // Everything after the running child goes back to Idle, so that
                // when an earlier condition flips there is no half-finished
                // work left behind it.
                cur.halt_children_from(i + 1);
                return Status::Running;
            }
            Status::Failure => {
                cur.halt_all_children();
                return Status::Failure;
            }
            Status::Success => {}
        }
    }
    cur.halt_all_children();
    Status::Success
}

/// Children left to right until one succeeds, remembering where it got to.
pub(super) fn fallback(cursor: &mut u16, cur: &mut Cursor<'_, '_>) -> Status {
    let n = cur.child_count();
    while *cursor < n {
        match cur.tick_child(*cursor) {
            Status::Running => return Status::Running,
            Status::Success => {
                *cursor = 0;
                cur.halt_all_children();
                return Status::Success;
            }
            Status::Failure => *cursor += 1,
        }
    }
    *cursor = 0;
    cur.halt_all_children();
    Status::Failure
}

/// Re-evaluates every child from the first, every tick.
pub(super) fn reactive_fallback(cur: &mut Cursor<'_, '_>) -> Status {
    let n = cur.child_count();
    for i in 0..n {
        match cur.tick_child(i) {
            Status::Running => {
                cur.halt_children_from(i + 1);
                return Status::Running;
            }
            Status::Success => {
                cur.halt_all_children();
                return Status::Success;
            }
            Status::Failure => {}
        }
    }
    cur.halt_all_children();
    Status::Failure
}

/// All unsettled children each tick, interleaved on this thread.
pub(super) fn parallel(settled: &mut u64, succeeded: &mut u64, cur: &mut Cursor<'_, '_>) -> Status {
    let n = cur.child_count();
    if n > MAX_PARALLEL_CHILDREN {
        debug_assert!(
            false,
            "Parallel with {n} children exceeds the {MAX_PARALLEL_CHILDREN}-bit completion set"
        );
        return Status::Failure;
    }

    let succ_needed = threshold(cur, ports::PARALLEL_SUCCESS_COUNT, -1, n);
    let fail_needed = threshold(cur, ports::PARALLEL_FAILURE_COUNT, 1, n).max(1);

    for i in 0..n {
        let bit = 1u64 << i;
        if *settled & bit != 0 {
            continue;
        }
        match cur.tick_child(i) {
            Status::Running => {}
            Status::Success => {
                *settled |= bit;
                *succeeded |= bit;
            }
            Status::Failure => *settled |= bit,
        }
    }

    let successes = succeeded.count_ones() as u16;
    let failures = (*settled & !*succeeded).count_ones() as u16;

    if successes >= succ_needed {
        *settled = 0;
        *succeeded = 0;
        cur.halt_all_children();
        return Status::Success;
    }
    if failures >= fail_needed {
        *settled = 0;
        *succeeded = 0;
        cur.halt_all_children();
        return Status::Failure;
    }
    Status::Running
}

const BRANCH_UNDECIDED: u8 = 0;

/// `children[0]` decides; the decision sticks until the chosen branch settles.
pub(super) fn if_then_else(branch: &mut u8, cur: &mut Cursor<'_, '_>) -> Status {
    let has_else = cur.child_count() > 2;

    if *branch == BRANCH_UNDECIDED {
        match cur.tick_child(0) {
            Status::Running => return Status::Running,
            Status::Success => *branch = 1,
            Status::Failure => {
                if !has_else {
                    return Status::Failure;
                }
                *branch = 2;
            }
        }
    }

    let status = cur.tick_child(u16::from(*branch));
    if status.is_settled() {
        *branch = BRANCH_UNDECIDED;
        cur.halt_all_children();
    }
    status
}

/// Like [`if_then_else`] but the condition is re-evaluated every tick.
pub(super) fn while_do_else(branch: &mut u8, cur: &mut Cursor<'_, '_>) -> Status {
    let has_else = cur.child_count() > 2;

    let wanted = match cur.tick_child(0) {
        Status::Running => {
            // The condition itself is undecided; nothing may run under it.
            cur.halt_children_from(1);
            *branch = BRANCH_UNDECIDED;
            return Status::Running;
        }
        Status::Success => 1u8,
        Status::Failure => {
            if !has_else {
                cur.halt_children_from(1);
                *branch = BRANCH_UNDECIDED;
                return Status::Failure;
            }
            2u8
        }
    };

    // The condition flipped under a running branch: stop that branch before
    // starting the other one, or two branches are live at once.
    if *branch != BRANCH_UNDECIDED && *branch != wanted {
        cur.halt_child(u16::from(*branch));
    }
    *branch = wanted;

    let status = cur.tick_child(u16::from(wanted));
    if status.is_settled() {
        *branch = BRANCH_UNDECIDED;
        cur.halt_children_from(1);
    }
    status
}
