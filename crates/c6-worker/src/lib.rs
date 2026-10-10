//! The carbon6 worker (`c6w`): a headless daemon that attaches to the head and
//! runs agent sessions on this machine.

pub mod conn;
pub mod enroll;
pub mod identity;

pub use conn::ConnState;
pub use identity::Identity;

pub(crate) fn hostname() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}
