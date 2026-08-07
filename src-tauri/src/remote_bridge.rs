//! Desktop alias of the outbound relay client. The implementation
//! moved to `moon_remote::relay` (shared with the headless binary);
//! this module keeps the `crate::remote_bridge::*` paths the
//! commands + state use stable.

pub use moon_remote::relay::*;
