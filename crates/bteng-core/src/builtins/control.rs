//! Control-node tick logic.
//!
//! # Skipped children
//!
//! A child that returns `Skipped` did not run, so it counts as neither success
//! nor failure: the sequences and fallbacks move on to the next child, and
//! `Parallel` leaves it out of the child count before applying thresholds. If
//! **every** child was skipped, the parent is `Skipped` too — that is what lets
//! a whole branch be skipped by preconditions on its leaves.
//!
//! "Every child" is read from the children's recorded states at the moment the
//! parent finishes, so a child skipped on an earlier tick of the same
//! activation counts. The check is O(children) and runs only when the parent
//! settles.

use super::{MAX_PARALLEL_CHILDREN, ports, threshold};
use crate::node::Cursor;
use crate::status::{NodeState, Status};

/// Did every child of this node end up `Skipped`? Call before halting them.
fn all_skipped(cur: &Cursor<'_, '_>) -> bool {
    (0..cur.child_count()).all(|i| cur.child_state(i) == NodeState::Skipped)
}

/// `Skipped` if every child was skipped, else `otherwise`.
fn unless_all_skipped(cur: &Cursor<'_, '_>, otherwise: Status) -> Status {
    if all_skipped(cur) {
        Status::Skipped
    } else {
        otherwise
    }
}

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
            Status::Success | Status::Skipped => *cursor += 1,
        }
    }
    let status = unless_all_skipped(cur, Status::Success);
    *cursor = 0;
    cur.halt_all_children();
    status
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
            Status::Success | Status::Skipped => *cursor += 1,
        }
    }
    let status = unless_all_skipped(cur, Status::Success);
    *cursor = 0;
    cur.halt_all_children();
    status
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
            Status::Success | Status::Skipped => {}
        }
    }
    let status = unless_all_skipped(cur, Status::Success);
    cur.halt_all_children();
    status
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
            Status::Failure | Status::Skipped => *cursor += 1,
        }
    }
    let status = unless_all_skipped(cur, Status::Failure);
    *cursor = 0;
    cur.halt_all_children();
    status
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
            Status::Failure | Status::Skipped => {}
        }
    }
    let status = unless_all_skipped(cur, Status::Failure);
    cur.halt_all_children();
    status
}

/// All unsettled children each tick, interleaved on this thread.
///
/// A skipped child is not settled: it is ticked again next tick, so a child
/// whose `_skipIf` turns false joins in (as in BT.CPP). Children skipped *this*
/// tick are taken out of the child count before the thresholds are applied —
/// a deliberate simplification of BT.CPP, whose answer depends on whether a
/// threshold was given explicitly. Every child skipped → `Skipped`.
pub(super) fn parallel(settled: &mut u64, succeeded: &mut u64, cur: &mut Cursor<'_, '_>) -> Status {
    let n = cur.child_count();
    if n > MAX_PARALLEL_CHILDREN {
        debug_assert!(
            false,
            "Parallel with {n} children exceeds the {MAX_PARALLEL_CHILDREN}-bit completion set"
        );
        return Status::Failure;
    }

    let mut skipped = 0u64;
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
            Status::Skipped => skipped |= bit,
        }
    }

    // `skipped` is at most `n` bits, so this cannot underflow.
    let counted = n - skipped.count_ones() as u16;
    if counted == 0 {
        *settled = 0;
        *succeeded = 0;
        cur.halt_all_children();
        return Status::Skipped;
    }

    let succ_needed = threshold(cur, ports::PARALLEL_SUCCESS_COUNT, -1, counted);
    let fail_needed = threshold(cur, ports::PARALLEL_FAILURE_COUNT, 1, counted).max(1);

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
            // No condition, no choice: nothing under this node ran either.
            // (BT.CPP throws here; a skipped condition is BTEng's to define.)
            Status::Skipped => {
                cur.halt_all_children();
                return Status::Skipped;
            }
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
        Status::Skipped => {
            // The condition did not run, so neither branch may.
            cur.halt_children_from(1);
            *branch = BRANCH_UNDECIDED;
            return Status::Skipped;
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
