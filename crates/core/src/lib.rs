//! airDroid's host side, independent of the UI toolkit: the VM, its disks,
//! guest memory, the session with the guest daemon and frame composition.
//! The air/OS app (`apps/airdroid-haiku`) and the Linux development tool
//! (`crates/devtool`) both build on it.

pub mod compose;
pub mod guestmem;
pub mod images;
pub mod qcow2;
pub mod session;
pub mod vm;

pub use airdroid_proto as proto;
