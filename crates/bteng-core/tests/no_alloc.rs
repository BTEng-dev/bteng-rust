//! The M0 exit criterion that matters most: **zero allocations per tick**.
//!
//! DR-4 keeps the hard/soft real-time decision open, and this is the single
//! constraint that keeps it open. Allocation discipline adopted late is
//! allocation discipline never adopted, so it is a test, not a note in a doc.
//!
//! The counter is thread-local with a `const` initializer — a global atomic
//! would count allocations made by the test harness's own threads, and a lazily
//! initialized TLS would allocate from inside the allocator.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use std::alloc::System;

use bteng_core::{Blackboard, Builtin, Ctx, SimClock, Status, TreeBuilder};

thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // `try_with` because TLS may already be destroyed during teardown.
        let _ = COUNTING.try_with(|on| {
            if on.get() {
                let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
            }
        });
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = COUNTING.try_with(|on| {
            if on.get() {
                let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
            }
        });
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: Counting = Counting;

/// Run `f` with allocation counting on, and report how many it made.
fn count_allocs<R>(f: impl FnOnce() -> R) -> (R, u64) {
    ALLOCS.with(|c| c.set(0));
    COUNTING.with(|c| c.set(true));
    let out = f();
    COUNTING.with(|c| c.set(false));
    (out, ALLOCS.with(Cell::get))
}

fn running_leaf(_ctx: &mut Ctx<'_>) -> Status {
    Status::Running
}

fn ok_leaf(_ctx: &mut Ctx<'_>) -> Status {
    Status::Success
}

fn always(_ctx: &Ctx<'_>) -> bool {
    true
}

/// A tree that never settles, so every tick walks the same full path.
///
/// Root sequence over `branches` sub-sequences; every branch succeeds except
/// the last, whose action stays `Running`. A tree that completed on tick one
/// would measure the halt path, not the tick path.
fn build_wide_tree(branches: usize) -> (bteng_core::Tree, Blackboard) {
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::sequence());
    for i in 0..branches {
        b.begin("branch", Builtin::sequence());
        b.leaf("guard", Builtin::condition(always));
        b.leaf(
            "work",
            Builtin::action(if i == branches - 1 {
                running_leaf
            } else {
                ok_leaf
            }),
        );
        b.end();
    }
    b.end();
    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).expect("tree builds");
    (bteng_core::Tree::new(graph), bb)
}

#[test]
fn steady_state_tick_allocates_nothing() {
    let (mut tree, mut bb) = build_wide_tree(16);
    let goal = bb.declare("goal", 0u64).unwrap();
    let clock = SimClock::new();
    let mut ctx = Ctx::new(&mut bb, &clock);

    // Warm up outside the counter: the first tick is allowed to touch anything
    // lazily initialized. Steady state is what the claim is about.
    for _ in 0..8 {
        tree.tick(&mut ctx);
    }

    let (_, allocs) = count_allocs(|| {
        for _ in 0..1000 {
            let s = tree.tick(&mut ctx);
            assert_eq!(s, Status::Running);
        }
    });

    assert_eq!(allocs, 0, "steady-state tick allocated {allocs} times");

    // A blackboard write must be allocation-free too — that is the reason
    // `set` assigns through `downcast_mut` instead of replacing the Box.
    let (_, allocs) = count_allocs(|| {
        for i in 0..1000u64 {
            ctx.bb.set(goal, i).unwrap();
        }
    });
    assert_eq!(allocs, 0, "blackboard writes allocated {allocs} times");
}

#[test]
fn halt_allocates_nothing() {
    let (mut tree, mut bb) = build_wide_tree(8);
    let clock = SimClock::new();
    let mut ctx = Ctx::new(&mut bb, &clock);

    tree.tick(&mut ctx);

    let (_, allocs) = count_allocs(|| {
        for _ in 0..100 {
            tree.halt(&mut ctx);
            tree.tick(&mut ctx);
        }
    });
    assert_eq!(allocs, 0, "halt/tick cycle allocated {allocs} times");
}
