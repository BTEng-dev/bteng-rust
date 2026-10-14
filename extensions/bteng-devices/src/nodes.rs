//! The nodes themselves.

use std::sync::Arc;

use bteng_async::{AsyncWork, Job, Outcome};
use bteng_core::{Ctx, Cursor, Node, PortValue, SetupError, Status};

use crate::device::{Device, DeviceError};

/// Port indices. Positions in the manifests in `registration`.
pub(crate) const P_TAG: u16 = 0;
pub(crate) const P_VALUE: u16 = 1;
/// `WaitForTag` adds a comparison.
pub(crate) const P_EXPECTED: u16 = 1;
pub(crate) const P_TOLERANCE: u16 = 2;
/// `CallMethod`.
pub(crate) const P_METHOD: u16 = 0;
pub(crate) const P_ARG: u16 = 1;
pub(crate) const P_RESULT: u16 = 2;

/// How this crate reads and writes one value type through a [`Device`].
///
/// One trait keeps `ReadTag<T>` and `WriteTag<T>` generic without either of them
/// knowing what a tag is made of.
pub trait TagValue: PortValue + PartialEq {
    fn read(device: &dyn Device, tag: &str) -> Result<Self, DeviceError>;
    fn write(device: &dyn Device, tag: &str, value: Self) -> Result<(), DeviceError>;
    /// Close enough to count as equal. Exact for discrete types; a tolerance for
    /// floats, because `==` on a sensor reading is a bug waiting for a shift
    /// change to find it.
    fn close_enough(a: &Self, b: &Self, tolerance: f64) -> bool;
    const TYPE_LABEL: &'static str;
}

impl TagValue for f64 {
    fn read(device: &dyn Device, tag: &str) -> Result<Self, DeviceError> {
        device.read_f64(tag)
    }
    fn write(device: &dyn Device, tag: &str, value: Self) -> Result<(), DeviceError> {
        device.write_f64(tag, value)
    }
    fn close_enough(a: &Self, b: &Self, tolerance: f64) -> bool {
        (a - b).abs() <= tolerance
    }
    const TYPE_LABEL: &'static str = "F64";
}

impl TagValue for bool {
    fn read(device: &dyn Device, tag: &str) -> Result<Self, DeviceError> {
        device.read_bool(tag)
    }
    fn write(device: &dyn Device, tag: &str, value: Self) -> Result<(), DeviceError> {
        device.write_bool(tag, value)
    }
    fn close_enough(a: &Self, b: &Self, _tolerance: f64) -> bool {
        a == b
    }
    const TYPE_LABEL: &'static str = "Bool";
}

/// Read a tag into a blackboard key. Synchronous — for devices whose reads are
/// a cache lookup rather than a round trip.
#[derive(Debug)]
pub struct ReadTag<T: TagValue> {
    device: Arc<dyn Device>,
    _marker: core::marker::PhantomData<fn() -> T>,
}

impl<T: TagValue> ReadTag<T> {
    #[must_use]
    pub fn new(device: Arc<dyn Device>) -> Self {
        Self {
            device,
            _marker: core::marker::PhantomData,
        }
    }
}

impl<T: TagValue> Node for ReadTag<T> {
    fn tick(&mut self, cur: &mut Cursor<'_, '_>) -> Status {
        let Ok(tag) = cur.get::<String>(P_TAG) else {
            return Status::Failure;
        };
        match T::read(self.device.as_ref(), &tag) {
            Ok(v) => {
                if cur.set(P_VALUE, v).is_err() {
                    return Status::Failure;
                }
                Status::Success
            }
            Err(_) => Status::Failure,
        }
    }

    fn setup(&mut self, _ctx: &mut Ctx<'_>) -> Result<(), SetupError> {
        // The lifecycle hook earns its keep here: connecting on the first tick
        // instead would put a round trip inside a tick.
        self.device
            .connect()
            .map_err(|e| SetupError::new(e.to_string()))
    }

    fn shutdown(&mut self, _ctx: &mut Ctx<'_>) {
        self.device.disconnect();
    }

    fn type_name(&self) -> &'static str {
        "ReadTag"
    }
}

/// Write a value to a tag.
#[derive(Debug)]
pub struct WriteTag<T: TagValue> {
    device: Arc<dyn Device>,
    _marker: core::marker::PhantomData<fn() -> T>,
}

impl<T: TagValue> WriteTag<T> {
    #[must_use]
    pub fn new(device: Arc<dyn Device>) -> Self {
        Self {
            device,
            _marker: core::marker::PhantomData,
        }
    }
}

impl<T: TagValue> Node for WriteTag<T> {
    fn tick(&mut self, cur: &mut Cursor<'_, '_>) -> Status {
        let (Ok(tag), Ok(value)) = (cur.get::<String>(P_TAG), cur.get::<T>(P_VALUE)) else {
            return Status::Failure;
        };
        match T::write(self.device.as_ref(), &tag, value) {
            Ok(()) => Status::Success,
            Err(_) => Status::Failure,
        }
    }

    fn setup(&mut self, _ctx: &mut Ctx<'_>) -> Result<(), SetupError> {
        self.device
            .connect()
            .map_err(|e| SetupError::new(e.to_string()))
    }

    fn type_name(&self) -> &'static str {
        "WriteTag"
    }
}

/// `Running` until a tag reaches a value.
///
/// The canonical industrial condition — "wait for the press to reach depth" —
/// and the reason a region wants `mode="on_change"`: this node is worth ticking
/// when the tag moves, not a thousand times a second while it does not.
#[derive(Debug)]
pub struct WaitForTag<T: TagValue> {
    device: Arc<dyn Device>,
    _marker: core::marker::PhantomData<fn() -> T>,
}

impl<T: TagValue> WaitForTag<T> {
    #[must_use]
    pub fn new(device: Arc<dyn Device>) -> Self {
        Self {
            device,
            _marker: core::marker::PhantomData,
        }
    }
}

impl<T: TagValue> Node for WaitForTag<T> {
    fn tick(&mut self, cur: &mut Cursor<'_, '_>) -> Status {
        let (Ok(tag), Ok(expected)) = (cur.get::<String>(P_TAG), cur.get::<T>(P_EXPECTED)) else {
            return Status::Failure;
        };
        let tolerance = cur.get_or::<f64>(P_TOLERANCE, 0.0);
        match T::read(self.device.as_ref(), &tag) {
            // A device error is a Failure, not a Running: quietly waiting
            // forever on a tag that does not exist is the worst of the three
            // possible answers.
            Err(_) => Status::Failure,
            Ok(v) if T::close_enough(&v, &expected, tolerance) => Status::Success,
            Ok(_) => Status::Running,
        }
    }

    fn setup(&mut self, _ctx: &mut Ctx<'_>) -> Result<(), SetupError> {
        self.device
            .connect()
            .map_err(|e| SetupError::new(e.to_string()))
    }

    fn type_name(&self) -> &'static str {
        "WaitForTag"
    }
}

/// Call a device method off the tick thread.
///
/// The [`AsyncWork`] split does the work: `start` reads ports on the tick
/// thread and hands over a closure holding only values, so a blocking round trip
/// cannot reach the tree from a worker.
#[derive(Debug)]
pub struct CallMethod {
    device: Arc<dyn Device>,
    result: Arc<std::sync::Mutex<Option<Result<f64, DeviceError>>>>,
}

impl CallMethod {
    #[must_use]
    pub fn new(device: Arc<dyn Device>) -> Self {
        Self {
            device,
            result: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

impl AsyncWork for CallMethod {
    fn start(&mut self, cur: &mut Cursor<'_, '_>) -> Job {
        let method = cur.get::<String>(P_METHOD).unwrap_or_default();
        let arg = cur.get_or::<f64>(P_ARG, 0.0);
        let device = Arc::clone(&self.device);
        let slot = Arc::clone(&self.result);
        Box::new(move |token| {
            if token.is_cancelled() {
                return Status::Failure;
            }
            let outcome = device.call(&method, arg);
            let ok = outcome.is_ok();
            *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(outcome);
            if ok { Status::Success } else { Status::Failure }
        })
    }

    fn finish(&mut self, cur: &mut Cursor<'_, '_>, outcome: Outcome) -> Status {
        let taken = self.result.lock().unwrap_or_else(|p| p.into_inner()).take();
        match (outcome, taken) {
            (Outcome::Settled(Status::Success), Some(Ok(v))) => {
                if cur.set(P_RESULT, v).is_err() {
                    return Status::Failure;
                }
                Status::Success
            }
            _ => Status::Failure,
        }
    }

    fn type_name(&self) -> &'static str {
        "CallMethod"
    }
}
