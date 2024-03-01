//! Kube_test provides a fake kubernetes service that can be used to test a kubernetes controller.
//! The Service class provides a [tower::Service] that can be used with a kubernetes Client to
//! behave sufficiently like a kubernetes controller to simplify testing controller reconcile loops.

#![allow(unreachable_pub)]

// Workaround for "unused crate" lint false positives.
use workspace_hack as _;

mod call;
mod error;
mod handler;
mod object_map;
mod request;
mod resource_handler;
mod service;
mod status;

pub use call::Call;
pub use error::{Error, Result};
pub use handler::{AsHandler, Handler};
pub use resource_handler::ResourceHandler;
pub use service::Service;
