pub mod displays;
pub mod platform;
pub mod system;
pub mod transport;
pub use platform::{BluetoothProvider, ControllerProvider};
pub use transport::WindowsHid;

#[cfg(test)]
#[path = "../../core/tests/support/allocation_counter.rs"]
mod allocation_counter;
#[cfg(test)]
#[global_allocator]
static ALLOCATOR: allocation_counter::TrackingAllocator = allocation_counter::TrackingAllocator;
