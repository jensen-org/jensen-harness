//! `jensen-harness`'s own library half, so its internals (the JSON-RPC
//! handler, the engine) can be driven directly by tests, the same way a
//! production directory never carries its own tests: every test for this
//! crate lives in the workspace's single `tests/` directory, exercising
//! this library the way an external dependent would.

pub mod engine;
pub mod rpc;
pub mod socket;
pub mod workflow;
