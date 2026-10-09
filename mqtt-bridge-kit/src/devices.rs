//! The device runtimes of a host (multi-device spec §4.1): a driver built by a factory, its type
//! erased, and started as R1's worker.

use crate::driver::{DeviceDriver, DriverInfo};

/// A driver built by the factory, its type erased: the host is not generic over it.
pub(crate) trait Startable: Send + 'static {
    /// The driver's `info()`.
    fn info(&self) -> DriverInfo;
}

/// A built driver of type `D`.
pub(crate) struct Built<D>(pub D);

impl<D: DeviceDriver> Startable for Built<D> {
    fn info(&self) -> DriverInfo {
        self.0.info()
    }
}
