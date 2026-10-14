//! Typed ports, bound at build time.
//!
//! A port is how a node reads and writes the world without knowing where the
//! value lives. In Python BTEng a port is a string looked up in a dict on every
//! access, holding `Any`. Here it is resolved exactly once — when the tree is
//! built — into a [`SlotId`], with the type checked at that moment. Ticking
//! never hashes a name and never discovers a type error.
//!
//! Literals get the same treatment. `<Wait msec="250"/>` does not become a
//! special "static parameter" path the way it does in BT.CPP and BTEng; the
//! literal is parsed at build time into an anonymous blackboard slot, so at tick
//! time there is exactly one way to read a port. One path is one set of bugs.

use alloc::string::{String, ToString};
use core::any::{Any, TypeId, type_name};
use core::fmt;

use crate::blackboard::{BbError, Blackboard};
use crate::id::SlotId;

/// Which way data flows through a port.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PortDirection {
    Input,
    Output,
    /// Read *and* written by the node.
    InOut,
}

impl PortDirection {
    #[must_use]
    pub const fn readable(self) -> bool {
        matches!(self, PortDirection::Input | PortDirection::InOut)
    }
    #[must_use]
    pub const fn writable(self) -> bool {
        matches!(self, PortDirection::Output | PortDirection::InOut)
    }
}

/// A value that can travel through a port.
///
/// The bounds are what binding needs: `Default` so an output port can create the
/// slot it will write, `Clone` so a value can be copied out of the blackboard,
/// and the parse/format pair so the same type works from XML and round-trips
/// into telemetry.
pub trait PortValue: Any + Send + Sync + Clone + Default + fmt::Debug {
    /// Parse the textual form used in XML attributes and defaults.
    ///
    /// # Errors
    /// When the text is not a valid value of this type.
    fn parse_port(text: &str) -> Result<Self, ParseError>;

    /// The textual form. Must round-trip through [`PortValue::parse_port`].
    fn format_port(&self) -> String;
}

/// A port literal could not be parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub type_name: &'static str,
    pub text: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} is not a valid {}", self.text, self.type_name)
    }
}

impl core::error::Error for ParseError {}

macro_rules! port_value_via_fromstr {
    ($($t:ty),* $(,)?) => {$(
        impl PortValue for $t {
            fn parse_port(text: &str) -> Result<Self, ParseError> {
                text.trim().parse::<$t>().map_err(|_| ParseError {
                    type_name: type_name::<$t>(),
                    text: text.to_string(),
                })
            }
            fn format_port(&self) -> String {
                alloc::format!("{self}")
            }
        }
    )*};
}

port_value_via_fromstr!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize, f32, f64);

impl PortValue for bool {
    /// Accepts the spellings BT.CPP XML uses in the wild, not just Rust's.
    fn parse_port(text: &str) -> Result<Self, ParseError> {
        match text.trim() {
            "1" | "true" | "TRUE" | "True" | "yes" | "on" => Ok(true),
            "0" | "false" | "FALSE" | "False" | "no" | "off" => Ok(false),
            other => Err(ParseError {
                type_name: "bool",
                text: other.to_string(),
            }),
        }
    }
    fn format_port(&self) -> String {
        if *self { "true" } else { "false" }.to_string()
    }
}

impl PortValue for String {
    fn parse_port(text: &str) -> Result<Self, ParseError> {
        Ok(text.to_string())
    }
    fn format_port(&self) -> String {
        self.clone()
    }
}

/// What went wrong reaching a port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PortError {
    /// A literal in the tree source did not parse as the port's type.
    Parse {
        port: &'static str,
        source: ParseError,
    },
    /// The port is bound to a slot holding a different type.
    ///
    /// Unreachable at tick time — binding checks this — so seeing it means a
    /// binding was built by hand.
    TypeMismatch {
        port: &'static str,
        expected: &'static str,
        slot: SlotId,
    },
    /// The tree source gave no value for a port with no default.
    Unbound {
        port: &'static str,
    },
    /// The node asked for a port index it does not have.
    NoSuchPort {
        index: u16,
        count: u16,
    },
    /// A write to a port whose direction is `Input`, or a read of an `Output`.
    WrongDirection {
        port: &'static str,
        direction: PortDirection,
    },
    Blackboard(BbError),
}

impl fmt::Display for PortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PortError::Parse { port, source } => write!(f, "port {port:?}: {source}"),
            PortError::TypeMismatch {
                port,
                expected,
                slot,
            } => write!(
                f,
                "port {port:?} expects {expected}, slot {slot:?} holds another type"
            ),
            PortError::Unbound { port } => {
                write!(f, "port {port:?} has no value and no default")
            }
            PortError::NoSuchPort { index, count } => {
                write!(f, "port index {index} out of range ({count} ports)")
            }
            PortError::WrongDirection { port, direction } => {
                write!(
                    f,
                    "port {port:?} is {direction:?} and cannot be used that way"
                )
            }
            PortError::Blackboard(e) => write!(f, "{e}"),
        }
    }
}

impl core::error::Error for PortError {}

impl From<BbError> for PortError {
    fn from(e: BbError) -> Self {
        PortError::Blackboard(e)
    }
}

// ── the type-erased half of a port declaration ───────────────────────────────
//
// A `PortSpec` has to do three things with a type it only knows statically at
// the declaration site: identify it, parse a literal into it, and create a slot
// for it. Function pointers monomorphised at declaration carry that across,
// which keeps `PortSpec` a plain `Copy` struct that lives in a `static` array —
// no registry of type handlers, no `dyn` gymnastics.

type TypeIdFn = fn() -> TypeId;
type TypeNameFn = fn() -> &'static str;
type LiteralFn = fn(&mut Blackboard, &str) -> Result<SlotId, ParseError>;
type EnsureFn = fn(&mut Blackboard, &str) -> Result<SlotId, BbError>;
type ReadTextFn = fn(&Blackboard, SlotId) -> Result<String, BbError>;
type SinkFn = fn(&mut Blackboard) -> SlotId;

fn type_id_of<T: PortValue>() -> TypeId {
    TypeId::of::<T>()
}

// `type_name` is not const-callable, so like the rest of the type-erased half
// it travels as a function pointer resolved at declaration.
//
// The module path is trimmed (`alloc::string::String` -> `String`): the path is
// a std implementation detail that `type_name` does not promise to keep, and it
// ends up in tooling output.
fn type_name_of<T: PortValue>() -> &'static str {
    let full = type_name::<T>();
    let head = full.find('<').unwrap_or(full.len());
    full[..head].rfind("::").map_or(full, |i| &full[i + 2..])
}

fn declare_literal<T: PortValue>(bb: &mut Blackboard, text: &str) -> Result<SlotId, ParseError> {
    Ok(bb.declare_anonymous(T::parse_port(text)?))
}

fn ensure_slot<T: PortValue>(bb: &mut Blackboard, name: &str) -> Result<SlotId, BbError> {
    match bb.resolve(name) {
        Some(id) => {
            // The slot already exists — the only question is whether the type
            // agrees. Answering it here is the whole point of binding early.
            bb.get::<T>(id).map(|_| id)
        }
        None => bb.declare(name, T::default()),
    }
}

fn read_text<T: PortValue>(bb: &Blackboard, id: SlotId) -> Result<String, BbError> {
    bb.get::<T>(id).map(PortValue::format_port)
}

fn sink<T: PortValue>(bb: &mut Blackboard) -> SlotId {
    bb.declare_anonymous(T::default())
}

/// A node type's declaration of one port.
///
/// Lives in a `static` array on the node's manifest — one per node *type*, not
/// per instance.
#[derive(Copy, Clone)]
pub struct PortSpec {
    pub name: &'static str,
    pub direction: PortDirection,
    pub description: &'static str,
    /// Applied when the tree source gives no value for this port.
    pub default: Option<&'static str>,
    type_id: TypeIdFn,
    type_name: TypeNameFn,
    literal: LiteralFn,
    ensure: EnsureFn,
    read_text: ReadTextFn,
    sink: SinkFn,
}

impl fmt::Debug for PortSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PortSpec")
            .field("name", &self.name)
            .field("direction", &self.direction)
            .field("type", &self.type_name())
            .field("default", &self.default)
            .finish()
    }
}

impl PortSpec {
    #[must_use]
    pub fn type_id(&self) -> TypeId {
        (self.type_id)()
    }

    /// The Rust type this port carries, for diagnostics and node-model export.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        (self.type_name)()
    }

    /// Parse `text` into a fresh anonymous slot.
    ///
    /// # Errors
    /// When `text` is not a valid value of this port's type.
    pub fn bind_literal(&self, bb: &mut Blackboard, text: &str) -> Result<SlotId, PortError> {
        (self.literal)(bb, text).map_err(|source| PortError::Parse {
            port: self.name,
            source,
        })
    }

    /// Resolve a `{key}` reference, creating the slot if the key is new.
    ///
    /// # Errors
    /// [`PortError::TypeMismatch`] when the key already holds another type.
    pub fn bind_key(&self, bb: &mut Blackboard, key: &str) -> Result<SlotId, PortError> {
        (self.ensure)(bb, key).map_err(|e| match e {
            BbError::TypeMismatch(slot) => PortError::TypeMismatch {
                port: self.name,
                expected: self.type_name(),
                slot,
            },
            other => PortError::Blackboard(other),
        })
    }

    /// A slot nobody reads, holding this type's default.
    ///
    /// An output port the tree source did not wire anywhere still has to point
    /// somewhere, or every write would need a branch on the tick path. It
    /// points here.
    pub fn bind_sink(&self, bb: &mut Blackboard) -> SlotId {
        (self.sink)(bb)
    }

    /// The port's current value as text. For telemetry and debugging only.
    ///
    /// # Errors
    /// When the slot is missing or holds another type.
    pub fn read_text(&self, bb: &Blackboard, slot: SlotId) -> Result<String, PortError> {
        (self.read_text)(bb, slot).map_err(PortError::Blackboard)
    }

    /// Attach a description. Shows up in exported node models and in BTviz.
    #[must_use]
    pub const fn described(mut self, description: &'static str) -> Self {
        self.description = description;
        self
    }

    /// Attach a default, applied when the tree source says nothing.
    #[must_use]
    pub const fn with_default(mut self, default: &'static str) -> Self {
        self.default = Some(default);
        self
    }
}

const fn spec<T: PortValue>(name: &'static str, direction: PortDirection) -> PortSpec {
    PortSpec {
        name,
        direction,
        description: "",
        default: None,
        type_id: type_id_of::<T>,
        type_name: type_name_of::<T>,
        literal: declare_literal::<T>,
        ensure: ensure_slot::<T>,
        read_text: read_text::<T>,
        sink: sink::<T>,
    }
}

/// Declare a port the node reads.
#[must_use]
pub const fn input<T: PortValue>(name: &'static str) -> PortSpec {
    spec::<T>(name, PortDirection::Input)
}

/// Declare a port the node writes.
#[must_use]
pub const fn output<T: PortValue>(name: &'static str) -> PortSpec {
    spec::<T>(name, PortDirection::Output)
}

/// Declare a port the node both reads and writes.
#[must_use]
pub const fn inout<T: PortValue>(name: &'static str) -> PortSpec {
    spec::<T>(name, PortDirection::InOut)
}

/// How a port got its slot. Kept for telemetry and for round-tripping a tree
/// back to XML; the tick path only ever sees the [`SlotId`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BindingSource {
    /// `port="{key}"` — a named blackboard key.
    Key,
    /// `port="value"` — a literal parsed into an anonymous slot.
    Literal,
    /// The port's declared default was used.
    Default,
    /// An unwired output port, pointed at a slot nobody reads.
    Sink,
}

/// One resolved port on one node instance.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub slot: SlotId,
    pub source: BindingSource,
}
