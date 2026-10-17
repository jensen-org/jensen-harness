//! Declarative workflows: a YAML spec, validated on a graph, run on dagrs.

pub mod error;
pub mod expr;
pub mod host;
pub mod nodes;
pub mod reducer;
pub mod runtime;
pub mod spec;
pub mod state;
pub mod validate;

pub use error::RunError;
pub use host::{Host, RoleBinding};
pub use runtime::{RunReport, RunStatus, Runner, StartRequest};
pub use spec::{NodeSpec, WorkflowSpec};
pub use validate::{SpecError, Workflow, json_schema, load};

pub use dagrs::{FileRunStore, MemoryRunStore, RunStore};
pub use tokio_util::sync::CancellationToken;
