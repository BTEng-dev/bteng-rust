//! Manifests, and getting the nodes into a registry.
//!
//! A node's manifest has to be `&'static`, and a *generic* node needs one per
//! type it is used at. Rust gives that for free: `Manifests::<f64>::READ` and
//! `Manifests::<bool>::READ` are separate associated constants with separate
//! port arrays, monomorphised at the use site. No macro, no registry of type
//! handlers.
//!
//! Constructors capture the device. That is the whole reason `NodeCtor` is a
//! closure — see the note on it in `bteng_core::registry`; writing this crate is
//! what established it had to be.

use std::sync::Arc;

use bteng_async::{AsyncAction, Spawner};
use bteng_core::{
    Arity, NodeCategory, NodeKind, NodeManifest, NodeRegistry, PortSpec, RegistryError, Waker,
    input, output,
};

use crate::device::Device;
use crate::nodes::{CallMethod, ReadTag, TagValue, WaitForTag, WriteTag};

/// Port declarations, generic over the tag's type.
struct Ports<T: TagValue>(core::marker::PhantomData<T>);

impl<T: TagValue> Ports<T> {
    const READ: &'static [PortSpec] = &[
        input::<String>("tag").described("device address to read"),
        output::<T>("value").described("where the reading goes"),
    ];
    const WRITE: &'static [PortSpec] = &[
        input::<String>("tag").described("device address to write"),
        input::<T>("value").described("what to write"),
    ];
    const WAIT: &'static [PortSpec] = &[
        input::<String>("tag").described("device address to watch"),
        input::<T>("expected").described("the value being waited for"),
        input::<f64>("tolerance")
            .with_default("0.0")
            .described("how close counts as equal"),
    ];
}

const CALL_PORTS: &[PortSpec] = &[
    input::<String>("method").described("method to invoke"),
    input::<f64>("arg").with_default("0.0"),
    output::<f64>("result"),
];

struct Manifests<T: TagValue>(core::marker::PhantomData<T>);

impl<T: TagValue> Manifests<T> {
    const READ: NodeManifest = NodeManifest::new(
        "ReadTag",
        NodeCategory::Action,
        Arity::Leaf,
        Ports::<T>::READ,
    )
    .described("Reads a device tag into a blackboard key.");
    const WRITE: NodeManifest = NodeManifest::new(
        "WriteTag",
        NodeCategory::Action,
        Arity::Leaf,
        Ports::<T>::WRITE,
    )
    .described("Writes a value to a device tag.");
    const WAIT: NodeManifest = NodeManifest::new(
        "WaitForTag",
        NodeCategory::Condition,
        Arity::Leaf,
        Ports::<T>::WAIT,
    )
    .described("Running until a device tag reaches a value.");
}

static CALL_MANIFEST: NodeManifest =
    NodeManifest::new("CallMethod", NodeCategory::Action, Arity::Leaf, CALL_PORTS)
        .described("Invokes a device method off the tick thread.");

/// Registers this crate's nodes against a device.
///
/// The device is captured here rather than being a port on every node: a
/// connection is a process-level thing, and threading it through the blackboard
/// would put a live client in a slot that anything could rebind.
pub struct DeviceNodes {
    device: Arc<dyn Device>,
    spawner: Option<Arc<dyn Spawner>>,
    waker: Option<Arc<dyn Waker>>,
}

impl std::fmt::Debug for DeviceNodes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeviceNodes")
            .field("connected", &self.device.is_connected())
            .field("async", &self.spawner.is_some())
            .finish_non_exhaustive()
    }
}

impl DeviceNodes {
    #[must_use]
    pub fn new(device: Arc<dyn Device>) -> Self {
        Self {
            device,
            spawner: None,
            waker: None,
        }
    }

    /// Supply somewhere to run blocking device calls.
    ///
    /// Without it, `CallMethod` is not registered at all — better than offering
    /// a node that would do a blocking round trip on the tick thread and quietly
    /// ruin every deadline in the tree.
    #[must_use]
    pub fn with_spawner(mut self, spawner: Arc<dyn Spawner>) -> Self {
        self.spawner = Some(spawner);
        self
    }

    /// Wake the scheduler when a device call settles.
    #[must_use]
    pub fn with_waker(mut self, waker: Arc<dyn Waker>) -> Self {
        self.waker = Some(waker);
        self
    }

    /// Register `ReadTagF64`, `WriteTagF64`, `WaitForTagF64`.
    ///
    /// # Errors
    /// [`RegistryError::Duplicate`] if a name is taken.
    pub fn register_f64(&self, registry: &mut NodeRegistry) -> Result<(), RegistryError> {
        self.register_for::<f64>(registry)
    }

    /// Register the `bool` flavour.
    ///
    /// # Errors
    /// [`RegistryError::Duplicate`] if a name is taken.
    pub fn register_bool(&self, registry: &mut NodeRegistry) -> Result<(), RegistryError> {
        self.register_for::<bool>(registry)
    }

    /// Register everything, including `CallMethod` when a spawner was given.
    ///
    /// # Errors
    /// [`RegistryError::Duplicate`] if a name is taken.
    pub fn register_all(&self, registry: &mut NodeRegistry) -> Result<(), RegistryError> {
        self.register_f64(registry)?;
        self.register_bool(registry)?;
        if self.spawner.is_some() {
            self.register_call(registry)?;
        }
        Ok(())
    }

    fn register_for<T: TagValue>(&self, registry: &mut NodeRegistry) -> Result<(), RegistryError> {
        let label = T::TYPE_LABEL;

        let device = Arc::clone(&self.device);
        registry.register_as(
            &format!("ReadTag{label}"),
            &Manifests::<T>::READ,
            move || NodeKind::Custom(Box::new(ReadTag::<T>::new(Arc::clone(&device)))),
        )?;

        let device = Arc::clone(&self.device);
        registry.register_as(
            &format!("WriteTag{label}"),
            &Manifests::<T>::WRITE,
            move || NodeKind::Custom(Box::new(WriteTag::<T>::new(Arc::clone(&device)))),
        )?;

        let device = Arc::clone(&self.device);
        registry.register_as(
            &format!("WaitForTag{label}"),
            &Manifests::<T>::WAIT,
            move || NodeKind::Custom(Box::new(WaitForTag::<T>::new(Arc::clone(&device)))),
        )?;
        Ok(())
    }

    fn register_call(&self, registry: &mut NodeRegistry) -> Result<(), RegistryError> {
        let device = Arc::clone(&self.device);
        let spawner = self.spawner.clone().expect("checked by the caller");
        let waker = self.waker.clone();
        registry.register_as("CallMethod", &CALL_MANIFEST, move || {
            let mut node =
                AsyncAction::new(CallMethod::new(Arc::clone(&device)), Arc::clone(&spawner));
            if let Some(w) = &waker {
                node = node.with_waker(Arc::clone(w));
            }
            NodeKind::Custom(Box::new(node))
        })
    }
}
