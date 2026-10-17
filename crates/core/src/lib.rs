//! jensen-harness's engine: the agent loop, the provider layer, the sandbox
//! trait and the built-in tools. Nothing in this crate depends on Jensen or
//! any other product; it calls model provider APIs directly and confines
//! its own tool calls itself.

pub mod agent;
pub mod budget;
pub mod compact;
pub mod providers;
pub mod sandbox;
pub mod secrets;
pub mod tools;
pub mod toolset;
