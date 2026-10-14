//! The slot blackboard.
//!
//! Python BTEng's blackboard is a string-keyed dict behind an `RLock`, with a
//! parent scope chain walked on every read and a history record allocated on
//! every write. Every one of those costs is paid per access, per tick.
//!
//! Here, the name lookup happens exactly once — when a port is bound at build
//! time — and produces a [`SlotId`]. After that, reading is an index plus a
//! `TypeId` compare, and writing is an in-place assignment through a
//! `downcast_mut`. **No allocation on the tick path**, which the `no_alloc`
//! test enforces rather than merely hopes for.
//!
//! Each slot carries a `version` bumped on every write. Nothing in M0 reads it,
//! but it is the mechanism the whole event-driven design rests on: a node
//! records the versions it read, and the scheduler compares them instead of
//! dispatching callbacks to every subscriber on every write.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::any::{Any, TypeId};
use core::fmt;

use crate::id::SlotId;

/// Why a blackboard operation could not be performed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BbError {
    /// The slot index does not exist in this blackboard.
    NoSuchSlot(SlotId),
    /// The slot exists but holds a different type than the caller asked for.
    ///
    /// On the tick path this should be unreachable: binding checks the type at
    /// build time. Seeing it means a binding was constructed by hand.
    TypeMismatch(SlotId),
    /// `declare` was called twice for the same name.
    DuplicateName,
}

impl fmt::Display for BbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BbError::NoSuchSlot(id) => write!(f, "no blackboard slot {id:?}"),
            BbError::TypeMismatch(id) => write!(f, "type mismatch on blackboard slot {id:?}"),
            BbError::DuplicateName => write!(f, "blackboard name already declared"),
        }
    }
}

// `core::error::Error` — available without `std`, so the trait is implemented
// unconditionally rather than only in hosted builds.
impl core::error::Error for BbError {}

struct Slot {
    type_id: TypeId,
    value: Box<dyn Any + Send + Sync>,
    version: u64,
}

impl fmt::Debug for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Slot")
            .field("type_id", &self.type_id)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

/// A flat arena of typed, versioned slots.
///
/// Single-writer by design: the owning region holds it `&mut` for the duration
/// of a tick. Cross-region sharing will be explicit (M2), never an ambient lock
/// — that is what makes multi-rate regions deterministic instead of merely
/// thread-safe.
#[derive(Debug, Default)]
pub struct Blackboard {
    slots: Vec<Slot>,
    /// Build-time only. Never touched during a tick, which is why a `BTreeMap`
    /// (no `std`, no hasher) is the right trade here.
    index: BTreeMap<String, SlotId>,
}

impl Blackboard {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a named slot holding `init`. Build time only — this allocates.
    ///
    /// # Errors
    /// [`BbError::DuplicateName`] if `name` was already declared.
    pub fn declare<T: Any + Send + Sync>(
        &mut self,
        name: &str,
        init: T,
    ) -> Result<SlotId, BbError> {
        if self.index.contains_key(name) {
            return Err(BbError::DuplicateName);
        }
        let id = SlotId(self.slots.len() as u32);
        self.slots.push(Slot {
            type_id: TypeId::of::<T>(),
            value: Box::new(init),
            version: 0,
        });
        self.index.insert(name.to_string(), id);
        Ok(id)
    }

    /// Create an unnamed slot. Build time only — this allocates.
    ///
    /// Used for port literals: `<Wait msec="250"/>` parses `250` into a slot of
    /// its own rather than becoming a second kind of port that the tick path
    /// would have to branch on.
    pub fn declare_anonymous<T: Any + Send + Sync>(&mut self, init: T) -> SlotId {
        let id = SlotId(self.slots.len() as u32);
        self.slots.push(Slot {
            type_id: TypeId::of::<T>(),
            value: Box::new(init),
            version: 0,
        });
        id
    }

    /// Look a name up. Build time only.
    #[must_use]
    pub fn resolve(&self, name: &str) -> Option<SlotId> {
        self.index.get(name).copied()
    }

    /// Every declared name, with its slot. Build time and tooling only.
    pub fn names(&self) -> impl Iterator<Item = (&str, SlotId)> {
        self.index.iter().map(|(k, v)| (k.as_str(), *v))
    }

    /// Resolve a name *and* check that the slot holds `T`.
    ///
    /// This is the call a port binding should make: pay the type check once,
    /// here, so the tick path never has to care.
    ///
    /// # Errors
    /// [`BbError::TypeMismatch`] if the slot holds another type.
    pub fn bind<T: Any + Send + Sync>(&self, name: &str) -> Option<Result<SlotId, BbError>> {
        let id = self.resolve(name)?;
        let slot = &self.slots[id.index()];
        Some(if slot.type_id == TypeId::of::<T>() {
            Ok(id)
        } else {
            Err(BbError::TypeMismatch(id))
        })
    }

    /// Read a slot.
    ///
    /// # Errors
    /// [`BbError::NoSuchSlot`] or [`BbError::TypeMismatch`].
    pub fn get<T: Any + Send + Sync>(&self, id: SlotId) -> Result<&T, BbError> {
        let slot = self.slots.get(id.index()).ok_or(BbError::NoSuchSlot(id))?;
        slot.value
            .downcast_ref::<T>()
            .ok_or(BbError::TypeMismatch(id))
    }

    /// Overwrite a slot **in place** and bump its version.
    ///
    /// In place is the whole point: assigning through `downcast_mut` reuses the
    /// existing box, so a write costs no allocation. Replacing the `Box` would
    /// have been the obvious implementation and would have put a malloc on the
    /// hot path of every tick that publishes a value.
    ///
    /// # Errors
    /// [`BbError::NoSuchSlot`] or [`BbError::TypeMismatch`].
    pub fn set<T: Any + Send + Sync>(&mut self, id: SlotId, value: T) -> Result<(), BbError> {
        let slot = self
            .slots
            .get_mut(id.index())
            .ok_or(BbError::NoSuchSlot(id))?;
        let dst = slot
            .value
            .downcast_mut::<T>()
            .ok_or(BbError::TypeMismatch(id))?;
        *dst = value;
        slot.version = slot.version.wrapping_add(1);
        Ok(())
    }

    /// Mutable access without replacing the value — for read-modify-write.
    ///
    /// Bumps the version unconditionally, because it cannot know whether the
    /// caller actually changed anything.
    ///
    /// # Errors
    /// [`BbError::NoSuchSlot`] or [`BbError::TypeMismatch`].
    pub fn get_mut<T: Any + Send + Sync>(&mut self, id: SlotId) -> Result<&mut T, BbError> {
        let slot = self
            .slots
            .get_mut(id.index())
            .ok_or(BbError::NoSuchSlot(id))?;
        slot.version = slot.version.wrapping_add(1);
        slot.value
            .downcast_mut::<T>()
            .ok_or(BbError::TypeMismatch(id))
    }

    /// Current version of a slot. Bumped on every write.
    #[must_use]
    pub fn version(&self, id: SlotId) -> Option<u64> {
        self.slots.get(id.index()).map(|s| s.version)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}
