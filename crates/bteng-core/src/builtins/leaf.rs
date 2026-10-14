//! Leaf tick logic and the Rust-function leaf types.

use super::{msec_to_nanos, now_of, ports};
use crate::clock::Nanos;
use crate::node::{Ctx, Cursor};
use crate::status::Status;

/// A leaf backed by a Rust function.
///
/// A plain `fn` pointer, so it costs no allocation and no vtable. Anything that
/// needs state implements [`crate::Node`] instead.
pub type ActionFn = fn(&mut Ctx<'_>) -> Status;

/// A leaf predicate. Side-effect free by BT convention, hence `&Ctx`.
pub type ConditionFn = fn(&Ctx<'_>) -> bool;

/// `Running` for `msec`, then `Success` — without blocking the tick thread.
pub(super) fn sleep(fire_at: &mut Nanos, armed: &mut bool, cur: &mut Cursor<'_, '_>) -> Status {
    if !*armed {
        let msec = cur.get_or::<u64>(ports::SLEEP_MSEC, 0);
        *fire_at = now_of(cur).saturating_add(msec_to_nanos(msec));
        *armed = true;
    }

    if now_of(cur) < *fire_at {
        return Status::Running;
    }
    *armed = false;
    Status::Success
}
