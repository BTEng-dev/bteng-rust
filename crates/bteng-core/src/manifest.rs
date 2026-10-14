//! What a node type declares about itself.
//!
//! One manifest per node *type*, held in a `static`. It is what the XML front
//! end validates against, what the registry hands out, what an exported node
//! model is generated from, and what BTviz will read to know a node's shape
//! without instantiating one.

use core::fmt;

use crate::port::PortSpec;

/// How many children a node kind requires. Checked by the builder, once.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Arity {
    /// Exactly zero — leaves.
    Leaf,
    /// Exactly one — decorators.
    One,
    /// One or more — most control nodes.
    AtLeastOne,
    /// A fixed count, e.g. `IfThenElse`.
    Exactly(u16),
    /// A closed range, e.g. `IfThenElse` with an optional else branch.
    Between(u16, u16),
}

impl Arity {
    #[must_use]
    pub const fn accepts(self, n: u16) -> bool {
        match self {
            Arity::Leaf => n == 0,
            Arity::One => n == 1,
            Arity::AtLeastOne => n >= 1,
            Arity::Exactly(k) => n == k,
            Arity::Between(lo, hi) => n >= lo && n <= hi,
        }
    }
}

impl fmt::Display for Arity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Arity::Leaf => f.write_str("no children"),
            Arity::One => f.write_str("exactly 1 child"),
            Arity::AtLeastOne => f.write_str("at least 1 child"),
            Arity::Exactly(k) => write!(f, "exactly {k} children"),
            Arity::Between(lo, hi) => write!(f, "between {lo} and {hi} children"),
        }
    }
}

/// The broad kind of a node. Drives XML element naming and BTviz rendering.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NodeCategory {
    Action,
    Condition,
    Control,
    Decorator,
    SubTree,
}

impl NodeCategory {
    /// The XML element name used when a tree is written back out.
    #[must_use]
    pub const fn xml_tag(self) -> &'static str {
        match self {
            NodeCategory::Action => "Action",
            NodeCategory::Condition => "Condition",
            NodeCategory::Control => "Control",
            NodeCategory::Decorator => "Decorator",
            NodeCategory::SubTree => "SubTree",
        }
    }
}

/// A node type's full self-description.
#[derive(Copy, Clone, Debug)]
pub struct NodeManifest {
    pub type_name: &'static str,
    pub category: NodeCategory,
    pub arity: Arity,
    pub ports: &'static [PortSpec],
    pub description: &'static str,
}

impl NodeManifest {
    #[must_use]
    pub const fn new(
        type_name: &'static str,
        category: NodeCategory,
        arity: Arity,
        ports: &'static [PortSpec],
    ) -> Self {
        Self {
            type_name,
            category,
            arity,
            ports,
            description: "",
        }
    }

    #[must_use]
    pub const fn described(mut self, description: &'static str) -> Self {
        self.description = description;
        self
    }

    /// Index of a port by name — for a node that prefers names to constants.
    ///
    /// Setup-time only: the tick path should use the index directly.
    #[must_use]
    pub fn port_index(&self, name: &str) -> Option<u16> {
        self.ports
            .iter()
            .position(|p| p.name == name)
            .map(|i| i as u16)
    }

    #[must_use]
    pub fn port(&self, index: u16) -> Option<&'static PortSpec> {
        self.ports.get(index as usize)
    }
}
