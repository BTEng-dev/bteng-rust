//! The allocation discipline extends to the scheduler loop.
//!
//! A tick that allocates is a tick with an unbounded tail, and the scheduler
//! runs one per region per period — it is the last place a hidden malloc should
//! be allowed to live (DR-4).

use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use std::alloc::System;

use bteng_core::{Blackboard, Builtin, Ctx, Status, Tree, TreeBuilder};
use bteng_sched::{Pacer, Scheduler, VirtualTime};

thread_local! {
    static ALLOCS: Cell<u64> = const { Cell::new(0) };
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
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

fn count_allocs<R>(f: impl FnOnce() -> R) -> (R, u64) {
    ALLOCS.with(|c| c.set(0));
    COUNTING.with(|c| c.set(true));
    let out = f();
    COUNTING.with(|c| c.set(false));
    (out, ALLOCS.with(Cell::get))
}

fn running(_c: &mut Ctx<'_>) -> Status {
    Status::Running
}

#[test]
fn stepping_the_scheduler_allocates_nothing() {
    let mut b = TreeBuilder::new();
    b.begin("root", Builtin::parallel())
        .begin("fast", Builtin::region())
        .port("hz", "500.0")
        .leaf("fast_work", Builtin::action(running))
        .end()
        .begin("slow", Builtin::region())
        .port("hz", "20.0")
        .leaf("slow_work", Builtin::action(running))
        .end()
        .leaf("root_work", Builtin::action(running))
        .end();

    let mut bb = Blackboard::new();
    let graph = b.build(&mut bb).unwrap();
    let mut sched = Scheduler::new(Tree::new(graph), &bb, 10.0).unwrap();
    let pacer = VirtualTime::new();
    sched.arm(pacer.now());

    // Warm up outside the counter — the deadline heap is sized at construction,
    // but the first steps are allowed to touch anything lazily initialised.
    for _ in 0..64 {
        sched.step(&mut bb, &pacer);
    }

    let (_, allocs) = count_allocs(|| {
        for _ in 0..5000 {
            sched.step(&mut bb, &pacer);
        }
    });
    // The heap pops exactly one entry and pushes exactly one back, so its
    // capacity never moves.
    assert_eq!(allocs, 0, "scheduler stepping allocated {allocs} times");
}
