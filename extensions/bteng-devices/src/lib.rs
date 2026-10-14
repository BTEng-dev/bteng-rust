//! Device I/O nodes for BTEng.
//!
//! # What this crate is for
//!
//! Two things at once.
//!
//! It is a **working extension**: `ReadTag`, `WriteTag`, `WaitForTag` and
//! `CallMethod` are the shape every industrial integration takes — OPC UA, ROS 2
//! parameters, Modbus, a robot's own IO map. Point [`Device`] at a real client
//! and the nodes are done.
//!
//! It is also the **proof for DR-6**. This crate is not a member of the
//! BTEngRust workspace. It builds against `bteng-core`, `bteng-async` and
//! `bteng-xml` exactly as a third-party crate would, so "the extension surface
//! is sufficient without core changes" is something the compiler checks rather
//! than something the roadmap asserts. If any of this had needed a `pub(crate)`
//! opened up, it would not compile.
//!
//! # The transport is a trait
//!
//! [`Device`] abstracts the wire, and [`MockDevice`] implements it. Python
//! BTEng's ROS 2 package does the same thing with `FakeRosNode`, for the same
//! reason: the tree logic has to be testable off the robot, and a node that can
//! only be exercised against live hardware is a node nobody exercises.
//!
//! # Generic over the value type
//!
//! `ReadTag<f64>` and `ReadTag<String>` are different node types with different
//! manifests, registered under different names. This is also the answer to the
//! `SetBlackboard` gap M1 left open: a node generic over its port type is
//! registered once per type it is used at.
//!
//! ```
//! # use bteng_core::NodeRegistry;
//! # use bteng_devices::{MockDevice, DeviceNodes};
//! # use std::sync::Arc;
//! let device = Arc::new(MockDevice::new());
//! let mut registry = NodeRegistry::with_builtins();
//! DeviceNodes::new(device).register_f64(&mut registry).unwrap();
//! assert!(registry.contains("ReadTagF64"));
//! ```

mod device;
mod nodes;
mod registration;

pub use device::{Device, DeviceError, MockDevice};
pub use nodes::{CallMethod, ReadTag, WaitForTag, WriteTag};
pub use registration::DeviceNodes;
