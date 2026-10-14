# Multi-rate ticking

A `Region` is a scheduling boundary: the parent reads the verdict it published, while the scheduler
ticks its subtree on its own clock. A 100 Hz region under a 10 Hz parent runs at 100 Hz.

```xml
<Parallel>
  <!-- its own steady clock -->
  <Region hz="100" overrun="skip">
    <FollowTrajectory/>
  </Region>

  <!-- asleep until something it reads changes; at most every 100 ms,
       at least every 500 ms so its own timeouts still fire -->
  <Region hz="2" mode="on_change" min_period_ms="100">
    <ReplanIfBlocked obstacle="{nearest_obstacle}"/>
  </Region>
</Parallel>
```

## Event-driven regions

A region's read set is the input slots of its own subtree, and dirtiness is a version comparison —
no callbacks, no locks, no allocation. A write to a slot some other region reads does not wake this
one.

## Absolute deadlines

Deadlines are absolute, so a slow tick costs its own phase and nothing after it. A `sleep(interval)`
loop drifts by every overrun; this does not.

Measured on an unpinned x86 desktop: 5.8 µs mean lateness at 1 kHz with a 200 µs spin tail, 0.04%
lost ticks. Full table and caveats in [M2 notes](m2-notes.md).

## Async leaves

A completed async leaf wakes the scheduler instead of waiting for the next tick, and halting one never
blocks the tick thread. See [M3–M5 notes](m3-m5-notes.md).

## Limits

Regions are single-threaded, deliberately (see [M2 notes](m2-notes.md)). Real multicore regions are
planned, and `loom` tests are planned to come first — see [Status](status.md).
