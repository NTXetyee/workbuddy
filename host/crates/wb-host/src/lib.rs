//! `wb-host` library: CLI parsing, the capture/encode pipeline and the iPad session (SPEC 3.5).
//! `main.rs` wires these together; the integration tests drive `net` directly.

pub mod cli;
pub mod net;
pub mod pipeline;
pub mod status;
