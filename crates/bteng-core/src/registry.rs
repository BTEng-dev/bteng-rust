//! Node type registry.
//!
//! Maps a type name to a manifest and a constructor. The XML front end resolves
//! `<GoToPose .../>` through here; so will live reconfiguration, when a patch
//! names node types it did not bring with it.
//!
//! Explicitly **not** a global singleton. Python BTEng's `NodeFactory` is one,
//! and had to replay a list of decorator registrations to survive its own
//! `reset_instance()`; two trees in one process silently shared it. A registry
//! here is a value you own, pass in, and can have two of.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

use crate::builtins::{ALIASES, CONSTRUCTIBLE};
use crate::manifest::NodeManifest;
use crate::node::NodeKind;

/// Builds a fresh instance of a node type.
///
/// A closure, not a bare `fn`, and that distinction was the first thing an
/// out-of-workspace extension crate ran into (M6). A device node has to be born
/// holding its connection; a ROS node holding its handle. A `fn` pointer cannot
/// capture, so the only ways to give it one are a leaked `static` or a
/// thread-local — both of which make the connection process-global and invisible
/// in the type. Letting the constructor be a closure costs one `Arc` clone per
/// node *construction*, which is build time, never tick time.
pub type NodeCtor = Arc<dyn Fn() -> NodeKind + Send + Sync>;

/// One registered node type.
#[derive(Clone)]
pub struct Registration {
    pub manifest: &'static NodeManifest,
    pub ctor: NodeCtor,
}

impl fmt::Debug for Registration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registration")
            .field("type_name", &self.manifest.type_name)
            .finish_non_exhaustive()
    }
}

/// Registration failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegistryError {
    /// The name is already taken. Silently replacing it would mean a tree's
    /// meaning depended on registration order.
    Duplicate { type_name: String },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RegistryError::Duplicate { type_name } => {
                write!(f, "node type {type_name:?} is already registered")
            }
        }
    }
}

impl core::error::Error for RegistryError {}

/// Name → node type.
#[derive(Debug, Default)]
pub struct NodeRegistry {
    entries: BTreeMap<String, Registration>,
}

impl NodeRegistry {
    /// An empty registry. Rarely what you want — see [`NodeRegistry::with_builtins`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every built-in, plus the BehaviorTree.CPP name aliases.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut reg = Self::new();
        for (manifest, ctor) in CONSTRUCTIBLE {
            let ctor = *ctor;
            reg.entries.insert(
                manifest.type_name.to_string(),
                Registration {
                    manifest,
                    ctor: Arc::new(ctor),
                },
            );
        }
        for (alias, target) in ALIASES {
            if let Some(reg_entry) = reg.entries.get(*target).cloned() {
                reg.entries.insert((*alias).to_string(), reg_entry);
            }
        }
        reg
    }

    /// Register a node type.
    ///
    /// # Errors
    /// [`RegistryError::Duplicate`] if the name is taken.
    pub fn register(
        &mut self,
        manifest: &'static NodeManifest,
        ctor: impl Fn() -> NodeKind + Send + Sync + 'static,
    ) -> Result<(), RegistryError> {
        self.register_as(manifest.type_name, manifest, ctor)
    }

    /// Register under a name other than the manifest's own — for a generic node
    /// instantiated at several types, or a project-local alias.
    ///
    /// # Errors
    /// [`RegistryError::Duplicate`] if the name is taken.
    pub fn register_as(
        &mut self,
        type_name: &str,
        manifest: &'static NodeManifest,
        ctor: impl Fn() -> NodeKind + Send + Sync + 'static,
    ) -> Result<(), RegistryError> {
        if self.entries.contains_key(type_name) {
            return Err(RegistryError::Duplicate {
                type_name: type_name.to_string(),
            });
        }
        self.entries.insert(
            type_name.to_string(),
            Registration {
                manifest,
                ctor: Arc::new(ctor),
            },
        );
        Ok(())
    }

    #[must_use]
    pub fn get(&self, type_name: &str) -> Option<Registration> {
        self.entries.get(type_name).cloned()
    }

    #[must_use]
    pub fn contains(&self, type_name: &str) -> bool {
        self.entries.contains_key(type_name)
    }

    /// Registered names, sorted.
    pub fn type_names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The registry as a BehaviorTree.CPP `<TreeNodesModel>` fragment.
    ///
    /// This is the machine-readable answer to "what nodes exist and what ports
    /// do they take". BTviz reads it to render a palette without loading a
    /// tree; Groot2 reads the same format.
    ///
    /// Deduplicated by **manifest identity**, not by name. An alias
    /// (`Selector` for `Fallback`) shares its target's manifest and is skipped,
    /// so a palette does not list one node type twice. A node generic over its
    /// port type has a *different* manifest per instantiation, so `ReadTagF64`
    /// and `ReadTagBool` are both listed — which is the point of registering
    /// them separately, and which an earlier name-based rule silently dropped.
    #[must_use]
    pub fn export_node_models_xml(&self) -> String {
        let mut out = String::from("<TreeNodesModel>\n");
        let mut seen: Vec<*const NodeManifest> = Vec::new();
        for (name, reg) in &self.entries {
            let ident = core::ptr::from_ref(reg.manifest);
            if seen.contains(&ident) {
                continue;
            }
            seen.push(ident);
            let tag = reg.manifest.category.xml_tag();
            out.push_str("  <");
            out.push_str(tag);
            out.push_str(" ID=\"");
            push_escaped(&mut out, name);
            out.push('"');
            if reg.manifest.ports.is_empty() {
                out.push_str("/>\n");
                continue;
            }
            out.push_str(">\n");
            for port in reg.manifest.ports {
                let dir = match port.direction {
                    crate::port::PortDirection::Input => "input_port",
                    crate::port::PortDirection::Output => "output_port",
                    crate::port::PortDirection::InOut => "inout_port",
                };
                out.push_str("    <");
                out.push_str(dir);
                out.push_str(" name=\"");
                push_escaped(&mut out, port.name);
                out.push('"');
                if let Some(default) = port.default {
                    out.push_str(" default=\"");
                    push_escaped(&mut out, default);
                    out.push('"');
                }
                out.push_str(" type=\"");
                push_escaped(&mut out, port.type_name());
                out.push('"');
                if port.description.is_empty() {
                    out.push_str("/>\n");
                } else {
                    out.push('>');
                    push_escaped(&mut out, port.description);
                    out.push_str("</");
                    out.push_str(dir);
                    out.push_str(">\n");
                }
            }
            out.push_str("  </");
            out.push_str(tag);
            out.push_str(">\n");
        }
        out.push_str("</TreeNodesModel>\n");
        out
    }
}

fn push_escaped(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
}
