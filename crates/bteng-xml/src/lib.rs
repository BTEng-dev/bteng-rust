//! BehaviorTree.CPP-compatible XML front end.
//!
//! A tree written for BT.CPP loads here unchanged, and one written for Python
//! BTEng loads too — both root/tree element spellings are accepted, and the
//! `{key}` port syntax is identical.
//!
//! ```xml
//! <root BTCPP_format="4" main_tree_to_execute="main">
//!   <BehaviorTree ID="main">
//!     <Sequence name="mission">
//!       <BatteryAbove threshold="20.0"/>
//!       <Timeout msec="5000">
//!         <NavigateTo goal="{target}"/>
//!       </Timeout>
//!       <SubTree ID="dock" station="{target}"/>
//!     </Sequence>
//!   </BehaviorTree>
//!   <BehaviorTree ID="dock"> ... </BehaviorTree>
//! </root>
//! ```
//!
//! # Subtrees are inlined, not referenced
//!
//! `<SubTree ID="dock"/>` splices a copy of `dock` into the arena, wrapped in a
//! transparent `SubTree` marker node so the boundary survives into telemetry and
//! BTviz even though the nodes themselves are inlined.
//!
//! Isolation is by **key rewriting**, decided here at load time rather than by a
//! scope chain walked at every tick the way Python BTEng does it. Each subtree
//! instance gets a namespace, so `{count}` inside `dock` becomes the key
//! `dock#1/count` and two instances of the same subtree cannot collide. An
//! attribute on the `<SubTree>` element maps one of its keys outward:
//! `station="{target}"` makes the subtree's `station` resolve to the parent's
//! `target`, and `speed="2.0"` substitutes a literal. `_autoremap="true"` passes
//! every unmapped key straight through to the parent, matching BT.CPP.
//!
//! The built tree therefore contains no port names and no scopes at all — only
//! slot indices.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use bteng_core::{Blackboard, BuildError, NodeRegistry, TreeBuilder, TreeGraph};

/// Root element names accepted. `root` is BT.CPP; `BTEng` is the Python engine.
const ROOT_TAGS: &[&str] = &["root", "BTEng"];
/// Tree-definition element names accepted.
const TREE_TAGS: &[&str] = &["BehaviorTree", "Tree"];
/// Elements that are metadata, not trees.
const NON_TREE_TAGS: &[&str] = &["TreeNodesModel", "include"];
/// Attributes on a node element that are not ports.
const RESERVED_ATTRS: &[&str] = &["ID", "name", "_autoremap", "_description", "_uid"];
/// Category tags whose real node type comes from the `ID` attribute (BT.CPP v3
/// and Python BTEng style).
const WRAPPER_TAGS: &[&str] = &["Action", "Condition", "Control", "Decorator"];

/// How far subtree inlining may nest before we call it a cycle.
const MAX_SUBTREE_DEPTH: usize = 64;

/// Why an XML tree could not be loaded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum XmlError {
    /// The document is not well-formed XML.
    Malformed(String),
    /// The root element is not one this loader recognises.
    BadRoot { found: String },
    /// No tree elements at all.
    NoTrees,
    /// Two trees share an ID.
    DuplicateTree { id: String },
    /// A tree element with no `ID`.
    UnnamedTree,
    /// `main_tree_to_execute` names a tree that is not defined, or the document
    /// has several trees and does not say which one to run.
    NoSuchTree { id: String, available: Vec<String> },
    /// A tree element that does not contain exactly one node.
    TreeNotSingleRooted { id: String, found: usize },
    /// An element naming a node type that is not registered.
    UnknownNodeType { type_name: String, tree: String },
    /// A `<SubTree>` with no `ID`, or one naming a tree that is not defined.
    BadSubTree { tree: String, id: String },
    /// Subtree inlining nested past [`MAX_SUBTREE_DEPTH`], or a tree that
    /// includes itself.
    SubTreeCycle { path: Vec<String> },
    /// The tree parsed but did not survive validation.
    Build(BuildError),
}

impl fmt::Display for XmlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            XmlError::Malformed(e) => write!(f, "malformed XML: {e}"),
            XmlError::BadRoot { found } => {
                write!(
                    f,
                    "root element is <{found}>, expected one of {ROOT_TAGS:?}"
                )
            }
            XmlError::NoTrees => f.write_str("document defines no behavior trees"),
            XmlError::DuplicateTree { id } => write!(f, "two trees share the ID {id:?}"),
            XmlError::UnnamedTree => f.write_str("a tree element has no ID"),
            XmlError::NoSuchTree { id, available } => {
                if id.is_empty() {
                    write!(
                        f,
                        "document defines {available:?} but no main_tree_to_execute"
                    )
                } else {
                    write!(f, "no tree named {id:?}; document defines {available:?}")
                }
            }
            XmlError::TreeNotSingleRooted { id, found } => write!(
                f,
                "tree {id:?} contains {found} top-level nodes, expected exactly 1"
            ),
            XmlError::UnknownNodeType { type_name, tree } => {
                write!(
                    f,
                    "tree {tree:?}: node type {type_name:?} is not registered"
                )
            }
            XmlError::BadSubTree { tree, id } => {
                write!(f, "tree {tree:?}: <SubTree ID={id:?}> does not resolve")
            }
            XmlError::SubTreeCycle { path } => {
                write!(f, "subtree nesting looks cyclic: {}", path.join(" -> "))
            }
            XmlError::Build(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for XmlError {}

impl From<BuildError> for XmlError {
    fn from(e: BuildError) -> Self {
        XmlError::Build(e)
    }
}

/// What a subtree key resolves to in its parent's namespace.
#[derive(Clone, Debug)]
enum Mapped {
    /// Another key, already fully resolved.
    Key(String),
    /// A literal, substituted verbatim.
    Literal(String),
}

/// The naming environment a subtree's nodes are built in.
#[derive(Clone, Debug, Default)]
struct Scope {
    /// Key namespace for anything not explicitly remapped. Empty at the root.
    prefix: String,
    /// Explicit outward mappings from the `<SubTree>` element.
    remap: BTreeMap<String, Mapped>,
    /// `_autoremap`: unmapped keys pass through to the parent unchanged.
    autoremap: bool,
}

impl Scope {
    fn resolve_key(&self, key: &str) -> Mapped {
        if let Some(m) = self.remap.get(key) {
            return m.clone();
        }
        if self.autoremap || self.prefix.is_empty() {
            return Mapped::Key(key.to_string());
        }
        Mapped::Key(format!("{}/{}", self.prefix, key))
    }

    fn rewrite_value(&self, value: &str) -> String {
        match as_key(value) {
            None => value.to_string(),
            Some(key) => match self.resolve_key(key) {
                Mapped::Key(k) => format!("{{{k}}}"),
                Mapped::Literal(lit) => lit,
            },
        }
    }
}

fn as_key(value: &str) -> Option<&str> {
    let v = value.trim();
    v.strip_prefix('{')
        .and_then(|v| v.strip_suffix('}'))
        .map(str::trim)
        .filter(|k| !k.is_empty())
}

/// Loads trees from XML against a [`NodeRegistry`].
#[derive(Debug)]
pub struct XmlLoader<'r> {
    registry: &'r NodeRegistry,
}

impl<'r> XmlLoader<'r> {
    #[must_use]
    pub fn new(registry: &'r NodeRegistry) -> Self {
        Self { registry }
    }

    /// Load the document's main tree.
    ///
    /// The main tree is `main_tree_to_execute` if given, otherwise the only tree
    /// in the document.
    ///
    /// # Errors
    /// See [`XmlError`].
    pub fn load(&self, xml: &str, bb: &mut Blackboard) -> Result<TreeGraph, XmlError> {
        self.load_impl(xml, None, bb)
    }

    /// Load a named tree, ignoring `main_tree_to_execute`.
    ///
    /// # Errors
    /// See [`XmlError`].
    pub fn load_tree(
        &self,
        xml: &str,
        tree_id: &str,
        bb: &mut Blackboard,
    ) -> Result<TreeGraph, XmlError> {
        self.load_impl(xml, Some(tree_id), bb)
    }

    /// The tree IDs a document defines, in document order.
    ///
    /// # Errors
    /// See [`XmlError`].
    pub fn tree_ids(&self, xml: &str) -> Result<Vec<String>, XmlError> {
        let doc = parse(xml)?;
        let root = check_root(&doc)?;
        let mut ids = Vec::new();
        for child in root.children().filter(roxmltree::Node::is_element) {
            if TREE_TAGS.contains(&child.tag_name().name()) {
                ids.push(
                    child
                        .attribute("ID")
                        .ok_or(XmlError::UnnamedTree)?
                        .to_string(),
                );
            }
        }
        Ok(ids)
    }

    fn load_impl(
        &self,
        xml: &str,
        wanted: Option<&str>,
        bb: &mut Blackboard,
    ) -> Result<TreeGraph, XmlError> {
        let doc = parse(xml)?;
        let root = check_root(&doc)?;

        let mut trees: BTreeMap<String, roxmltree::Node<'_, '_>> = BTreeMap::new();
        let mut order: Vec<String> = Vec::new();
        for child in root.children().filter(roxmltree::Node::is_element) {
            let tag = child.tag_name().name();
            if NON_TREE_TAGS.contains(&tag) || !TREE_TAGS.contains(&tag) {
                continue;
            }
            let id = child.attribute("ID").ok_or(XmlError::UnnamedTree)?;
            if trees.insert(id.to_string(), child).is_some() {
                return Err(XmlError::DuplicateTree { id: id.to_string() });
            }
            order.push(id.to_string());
        }
        if trees.is_empty() {
            return Err(XmlError::NoTrees);
        }

        let main = match wanted.or_else(|| root.attribute("main_tree_to_execute")) {
            Some(id) => id.to_string(),
            None if order.len() == 1 => order[0].clone(),
            None => {
                return Err(XmlError::NoSuchTree {
                    id: String::new(),
                    available: order,
                });
            }
        };
        if !trees.contains_key(&main) {
            return Err(XmlError::NoSuchTree {
                id: main,
                available: order,
            });
        }

        let mut builder = TreeBuilder::new();
        let mut instances = 0usize;
        let mut stack = vec![main.clone()];
        let root_node = single_node_child(trees[&main], &main)?;
        let mut emit = Emit {
            registry: self.registry,
            trees: &trees,
            builder: &mut builder,
            instances: &mut instances,
            stack: &mut stack,
        };
        emit.node(root_node, &Scope::default(), &main)?;

        Ok(builder.build(bb)?)
    }
}

/// The recursive half of loading, with the wandering state gathered up so the
/// recursion does not need eight arguments.
struct Emit<'a, 'doc, 'input> {
    registry: &'a NodeRegistry,
    trees: &'a BTreeMap<String, roxmltree::Node<'doc, 'input>>,
    builder: &'a mut TreeBuilder,
    instances: &'a mut usize,
    /// Tree IDs currently being inlined — the cycle guard.
    stack: &'a mut Vec<String>,
}

impl<'doc, 'input> Emit<'_, 'doc, 'input> {
    fn node(
        &mut self,
        el: roxmltree::Node<'doc, 'input>,
        scope: &Scope,
        tree_id: &str,
    ) -> Result<(), XmlError> {
        if is_subtree_tag(el.tag_name().name()) {
            return self.subtree(el, scope, tree_id);
        }

        let type_name = node_type_name(el);
        let Some(reg) = self.registry.get(type_name) else {
            return Err(XmlError::UnknownNodeType {
                type_name: type_name.to_string(),
                tree: tree_id.to_string(),
            });
        };

        let instance_name = el.attribute("name").unwrap_or(type_name);
        let children: Vec<_> = el.children().filter(roxmltree::Node::is_element).collect();

        if children.is_empty() {
            self.builder
                .leaf_kind(instance_name, reg.manifest, (reg.ctor)());
        } else {
            self.builder
                .begin_kind(instance_name, reg.manifest, (reg.ctor)());
        }
        apply_ports(el, scope, self.builder);

        for child in children.iter().copied() {
            self.node(child, scope, tree_id)?;
        }
        if !children.is_empty() {
            self.builder.end();
        }
        Ok(())
    }

    fn subtree(
        &mut self,
        el: roxmltree::Node<'doc, 'input>,
        scope: &Scope,
        tree_id: &str,
    ) -> Result<(), XmlError> {
        let id = el.attribute("ID").unwrap_or_default();
        if id.is_empty() || !self.trees.contains_key(id) {
            return Err(XmlError::BadSubTree {
                tree: tree_id.to_string(),
                id: id.to_string(),
            });
        }

        if self.stack.len() >= MAX_SUBTREE_DEPTH || self.stack.iter().any(|t| t == id) {
            let mut path = self.stack.clone();
            path.push(id.to_string());
            return Err(XmlError::SubTreeCycle { path });
        }

        // Each instance gets its own namespace, so two copies of the same
        // subtree cannot write over each other's keys.
        *self.instances += 1;
        let inner = Scope {
            prefix: format!("{id}#{}", self.instances),
            remap: el
                .attributes()
                .filter(|a| !RESERVED_ATTRS.contains(&a.name()))
                .map(|a| {
                    // The right-hand side is written in the *parent's* scope, so
                    // it resolves there before being stored.
                    let mapped = match as_key(a.value()) {
                        Some(key) => scope.resolve_key(key),
                        None => Mapped::Literal(a.value().to_string()),
                    };
                    (a.name().to_string(), mapped)
                })
                .collect(),
            autoremap: el
                .attribute("_autoremap")
                .is_some_and(|v| matches!(v, "1" | "true" | "TRUE" | "True" | "yes" | "on")),
        };

        let instance_name = el.attribute("name").unwrap_or(id);
        let marker = self
            .registry
            .get("SubTree")
            .expect("SubTree is a built-in and is always registered");
        self.builder
            .begin_kind(instance_name, marker.manifest, (marker.ctor)());

        let root_node = single_node_child(self.trees[id], id)?;
        self.stack.push(id.to_string());
        self.node(root_node, &inner, id)?;
        self.stack.pop();

        self.builder.end();
        Ok(())
    }
}

/// Port attributes, rewritten through the scope.
fn apply_ports(el: roxmltree::Node<'_, '_>, scope: &Scope, builder: &mut TreeBuilder) {
    for attr in el.attributes() {
        if RESERVED_ATTRS.contains(&attr.name()) {
            continue;
        }
        builder.port(attr.name(), &scope.rewrite_value(attr.value()));
    }
}

/// The node type an element names.
///
/// BT.CPP v4 puts the type in the tag (`<NavigateTo/>`); v3 and Python BTEng put
/// it in `ID` under a category tag (`<Action ID="NavigateTo"/>`). Both are read,
/// because trees written either way are out there.
fn node_type_name<'a>(el: roxmltree::Node<'a, 'a>) -> &'a str {
    let tag = el.tag_name().name();
    if WRAPPER_TAGS.contains(&tag)
        && let Some(id) = el.attribute("ID")
    {
        return id;
    }
    tag
}

fn is_subtree_tag(tag: &str) -> bool {
    tag.eq_ignore_ascii_case("subtree")
}

fn parse(xml: &str) -> Result<roxmltree::Document<'_>, XmlError> {
    roxmltree::Document::parse(xml).map_err(|e| XmlError::Malformed(e.to_string()))
}

fn check_root<'a, 'input>(
    doc: &'a roxmltree::Document<'input>,
) -> Result<roxmltree::Node<'a, 'input>, XmlError> {
    let root = doc.root_element();
    let tag = root.tag_name().name();
    if !ROOT_TAGS.contains(&tag) {
        return Err(XmlError::BadRoot {
            found: tag.to_string(),
        });
    }
    Ok(root)
}

fn single_node_child<'a, 'input>(
    tree_el: roxmltree::Node<'a, 'input>,
    id: &str,
) -> Result<roxmltree::Node<'a, 'input>, XmlError> {
    let nodes: Vec<_> = tree_el
        .children()
        .filter(roxmltree::Node::is_element)
        .collect();
    match nodes.len() {
        1 => Ok(nodes[0]),
        found => Err(XmlError::TreeNotSingleRooted {
            id: id.to_string(),
            found,
        }),
    }
}

/// Node type names a document uses that the registry does not know.
///
/// Cheaper than [`XmlLoader::load`] when the question is only "what is missing?"
/// — a CLI can list every unregistered type in one pass instead of failing at
/// the first.
///
/// # Errors
/// See [`XmlError`].
pub fn unknown_node_types(xml: &str, registry: &NodeRegistry) -> Result<Vec<String>, XmlError> {
    let doc = parse(xml)?;
    let root = check_root(&doc)?;
    let mut unknown = BTreeSet::new();
    for tree in root.children().filter(roxmltree::Node::is_element) {
        if !TREE_TAGS.contains(&tree.tag_name().name()) {
            continue;
        }
        for el in tree.descendants().filter(roxmltree::Node::is_element) {
            // `descendants()` includes the tree element itself, which is not a
            // node type and must not be reported as a missing one.
            if el == tree || is_subtree_tag(el.tag_name().name()) {
                continue;
            }
            let name = node_type_name(el);
            if !registry.contains(name) {
                unknown.insert(name.to_string());
            }
        }
    }
    Ok(unknown.into_iter().collect())
}
