//! Preconditions: `_failureIf`, `_successIf`, `_skipIf`, `_while`.
//!
//! Checked by [`crate::tick_node`] *before* a node's `tick()`. If one decides,
//! `tick()` is not called:
//!
//! ```text
//! if the node is not Running:
//!     _failureIf true  → Failure
//!     _successIf true  → Success
//!     _skipIf    true  → Skipped
//! if _while is present and false:
//!     if Running: halt the node
//!     → Skipped
//! otherwise: tick() as usual
//! ```
//!
//! Only `_while` is evaluated while the node is `Running`, so a `_skipIf` that
//! turns true halfway through an action does not interrupt it.
//!
//! A precondition is always a [`Compiled`] expression, from XML or from the
//! builder alike: there is deliberately no Rust-function form, because a tree
//! must stay XML-expressible (DR-5). It lives in a box built once by the builder; the check is a branch, an index and, if the
//! node has preconditions, a bounded allocation-free evaluation.

use alloc::boxed::Box;

use crate::expr::Compiled;
use crate::node::Ctx;
use crate::status::Status;

/// Which precondition.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Precondition {
    /// True → `Failure`, without ticking. Not checked while `Running`.
    FailureIf,
    /// True → `Success`, without ticking. Not checked while `Running`.
    SuccessIf,
    /// True → `Skipped`, without ticking. Not checked while `Running`.
    SkipIf,
    /// False → `Skipped`, halting the node first if it was `Running`. Checked
    /// on every tick.
    While,
}

impl Precondition {
    /// In evaluation order.
    pub const ALL: [Precondition; 4] = [
        Precondition::FailureIf,
        Precondition::SuccessIf,
        Precondition::SkipIf,
        Precondition::While,
    ];

    /// The XML attribute name, as BehaviorTree.CPP spells it.
    #[must_use]
    pub const fn attribute(self) -> &'static str {
        match self {
            Precondition::FailureIf => "_failureIf",
            Precondition::SuccessIf => "_successIf",
            Precondition::SkipIf => "_skipIf",
            Precondition::While => "_while",
        }
    }

    /// The precondition an XML attribute names, if it names one.
    #[must_use]
    pub fn from_attribute(name: &str) -> Option<Precondition> {
        Precondition::ALL
            .into_iter()
            .find(|p| p.attribute() == name)
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// One installed precondition: a compiled expression.
#[derive(Clone, Debug)]
pub(crate) struct Guard(pub(crate) Compiled);

impl Guard {
    /// `None` if the expression could not be read — see [`Compiled::eval`].
    fn holds(&self, ctx: &Ctx<'_>) -> Option<bool> {
        self.0.eval(ctx.bb)
    }
}

/// Every precondition one node carries, with the text each was written as.
#[derive(Clone, Debug, Default)]
pub struct Preconditions {
    guards: [Option<Guard>; 4],
    sources: [Option<Box<str>>; 4],
}

impl Preconditions {
    pub(crate) fn set(&mut self, kind: Precondition, guard: Guard, source: Box<str>) {
        self.guards[kind.index()] = Some(guard);
        self.sources[kind.index()] = Some(source);
    }

    /// The text of a precondition, if the node has it.
    #[must_use]
    pub fn source(&self, kind: Precondition) -> Option<&str> {
        self.sources[kind.index()].as_deref()
    }

    /// Every precondition present, in evaluation order, with its text.
    pub fn iter(&self) -> impl Iterator<Item = (Precondition, &str)> {
        Precondition::ALL
            .into_iter()
            .filter_map(|k| self.source(k).map(|s| (k, s)))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.guards.iter().all(Option::is_none)
    }

    /// Decide whether the node runs. `None` means "tick it as usual".
    ///
    /// An expression that cannot be read does not decide anything. That is
    /// only possible when the tree is ticked against a blackboard other than
    /// the one it was built with, and a misconfiguration must not turn into a
    /// status the tree would act on — the same rule `Cursor::get_or` follows.
    pub(crate) fn check(&self, running: bool, ctx: &Ctx<'_>) -> Option<(Status, Precondition)> {
        let fires = |kind: Precondition, want: bool| -> bool {
            self.guards[kind.index()].as_ref().is_some_and(|g| {
                let v = g.holds(ctx);
                debug_assert!(
                    v.is_some(),
                    "{} could not be read: tree ticked against another blackboard?",
                    kind.attribute()
                );
                v == Some(want)
            })
        };
        if !running {
            if fires(Precondition::FailureIf, true) {
                return Some((Status::Failure, Precondition::FailureIf));
            }
            if fires(Precondition::SuccessIf, true) {
                return Some((Status::Success, Precondition::SuccessIf));
            }
            if fires(Precondition::SkipIf, true) {
                return Some((Status::Skipped, Precondition::SkipIf));
            }
        }
        if fires(Precondition::While, false) {
            return Some((Status::Skipped, Precondition::While));
        }
        None
    }
}
