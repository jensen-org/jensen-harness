//! Declarative workflows: a YAML spec, validated on a graph, run on dagrs.

pub mod reducer;
pub mod spec;
pub mod state;
pub mod validate;

pub use spec::{NodeSpec, WorkflowSpec};
pub use validate::{SpecError, Workflow, json_schema, load};
