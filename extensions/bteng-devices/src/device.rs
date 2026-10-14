//! The transport abstraction, and a mock for testing off the hardware.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Why a device operation failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceError {
    NotConnected,
    NoSuchTag(String),
    /// The tag exists but holds something else. Named rather than coerced: a
    /// silent conversion between a setpoint and a status word is not a service
    /// anyone wants.
    TypeMismatch {
        tag: String,
        expected: &'static str,
    },
    Timeout,
    Transport(String),
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceError::NotConnected => f.write_str("device is not connected"),
            DeviceError::NoSuchTag(t) => write!(f, "no tag {t:?}"),
            DeviceError::TypeMismatch { tag, expected } => {
                write!(f, "tag {tag:?} is not a {expected}")
            }
            DeviceError::Timeout => f.write_str("device timed out"),
            DeviceError::Transport(m) => write!(f, "transport: {m}"),
        }
    }
}

impl std::error::Error for DeviceError {}

/// Anything that holds named values and can be asked to do things.
///
/// `Send + Sync` because the async nodes hand it to a worker thread, and every
/// method takes `&self` because a client is shared, never owned by one node.
pub trait Device: Send + Sync + fmt::Debug {
    fn connect(&self) -> Result<(), DeviceError>;
    fn disconnect(&self);
    fn is_connected(&self) -> bool;

    fn read_f64(&self, tag: &str) -> Result<f64, DeviceError>;
    fn write_f64(&self, tag: &str, value: f64) -> Result<(), DeviceError>;
    fn read_bool(&self, tag: &str) -> Result<bool, DeviceError>;
    fn write_bool(&self, tag: &str, value: bool) -> Result<(), DeviceError>;

    /// Ask the device to do something. Blocking — the async nodes run it off the
    /// tick thread, which is why it is allowed to be.
    fn call(&self, method: &str, arg: f64) -> Result<f64, DeviceError>;
}

/// An in-memory device.
///
/// Not a stub: it models latency, disconnection and failure injection, because
/// the interesting behaviour of a device node is what it does when the device
/// misbehaves, and that is exactly what hardware will not do on demand.
#[derive(Debug)]
pub struct MockDevice {
    connected: AtomicBool,
    numbers: Mutex<BTreeMap<String, f64>>,
    flags: Mutex<BTreeMap<String, bool>>,
    /// How long every operation takes.
    latency: Mutex<Duration>,
    /// Fail the next N operations.
    fail_next: AtomicU32,
    reads: AtomicU32,
    writes: AtomicU32,
    calls: AtomicU32,
}

impl Default for MockDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl MockDevice {
    #[must_use]
    pub fn new() -> Self {
        Self {
            connected: AtomicBool::new(false),
            numbers: Mutex::new(BTreeMap::new()),
            flags: Mutex::new(BTreeMap::new()),
            latency: Mutex::new(Duration::ZERO),
            fail_next: AtomicU32::new(0),
            reads: AtomicU32::new(0),
            writes: AtomicU32::new(0),
            calls: AtomicU32::new(0),
        }
    }

    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    pub fn set_f64(&self, tag: &str, value: f64) {
        self.numbers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(tag.to_string(), value);
    }

    pub fn set_bool(&self, tag: &str, value: bool) {
        self.flags
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(tag.to_string(), value);
    }

    pub fn set_latency(&self, latency: Duration) {
        *self.latency.lock().unwrap_or_else(|p| p.into_inner()) = latency;
    }

    /// Make the next `n` operations fail with a transport error.
    pub fn fail_next(&self, n: u32) {
        self.fail_next.store(n, Ordering::SeqCst);
    }

    #[must_use]
    pub fn read_count(&self) -> u32 {
        self.reads.load(Ordering::SeqCst)
    }
    #[must_use]
    pub fn write_count(&self) -> u32 {
        self.writes.load(Ordering::SeqCst)
    }
    #[must_use]
    pub fn call_count(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }

    fn gate(&self) -> Result<(), DeviceError> {
        if !self.is_connected() {
            return Err(DeviceError::NotConnected);
        }
        let latency = *self.latency.lock().unwrap_or_else(|p| p.into_inner());
        if !latency.is_zero() {
            std::thread::sleep(latency);
        }
        // `fetch_update` is deprecated on newer stable in favour of `try_update`, which does not
        // exist at our MSRV (1.85). Keep the old name until the MSRV moves.
        #[allow(deprecated)]
        let injected = self
            .fail_next
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if injected {
            return Err(DeviceError::Transport("injected failure".into()));
        }
        Ok(())
    }
}

impl Device for MockDevice {
    fn connect(&self) -> Result<(), DeviceError> {
        self.connected.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn disconnect(&self) {
        self.connected.store(false, Ordering::SeqCst);
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    fn read_f64(&self, tag: &str) -> Result<f64, DeviceError> {
        self.gate()?;
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.numbers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(tag)
            .copied()
            .ok_or_else(|| DeviceError::NoSuchTag(tag.to_string()))
    }

    fn write_f64(&self, tag: &str, value: f64) -> Result<(), DeviceError> {
        self.gate()?;
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.numbers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(tag.to_string(), value);
        Ok(())
    }

    fn read_bool(&self, tag: &str) -> Result<bool, DeviceError> {
        self.gate()?;
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.flags
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(tag)
            .copied()
            .ok_or_else(|| DeviceError::NoSuchTag(tag.to_string()))
    }

    fn write_bool(&self, tag: &str, value: bool) -> Result<(), DeviceError> {
        self.gate()?;
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.flags
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(tag.to_string(), value);
        Ok(())
    }

    fn call(&self, method: &str, arg: f64) -> Result<f64, DeviceError> {
        self.gate()?;
        self.calls.fetch_add(1, Ordering::SeqCst);
        match method {
            "double" => Ok(arg * 2.0),
            "fail" => Err(DeviceError::Transport("method refused".into())),
            other => Err(DeviceError::NoSuchTag(other.to_string())),
        }
    }
}
