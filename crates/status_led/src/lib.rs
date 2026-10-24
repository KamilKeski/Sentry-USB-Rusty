//! Advisory LED status. Never controls storage or USB presentation.
#[cfg(target_os = "linux")]
pub mod driver;
pub mod model;
#[cfg(target_os = "linux")]
pub mod observe;
#[cfg(target_os = "linux")]
mod runtime;
#[cfg(target_os = "linux")]
pub use runtime::{Controller, MediaTransition, restore};

#[cfg(not(target_os = "linux"))]
mod unsupported {
    #[derive(Clone)]
    pub struct Controller;
    impl Controller {
        pub fn start(_: impl Fn() -> bool + Send + 'static) -> Option<Self> {
            None
        }
        pub fn ready(&self) {}
        pub fn fault(&self) {}
        pub fn stop(&self) {}
    }
    pub struct MediaTransition;
    impl MediaTransition {
        pub fn begin() -> Self {
            Self
        }
    }
    pub fn restore() {}
}
#[cfg(not(target_os = "linux"))]
pub use unsupported::*;
