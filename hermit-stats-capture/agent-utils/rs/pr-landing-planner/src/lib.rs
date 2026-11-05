//! Pure core and host adapters for `pr-landing-planner`.
//!
//! The planner is advisory: no API in this crate arms, refires, lands, or merges a pull request.

#![forbid(unsafe_code)]

pub mod classify;
pub mod cli;
pub mod collect;
pub mod context;
pub mod emit;
pub mod fixture;
pub mod graph;
pub mod host;
pub mod mechanism;
pub mod model;
pub mod plan;
pub mod priority;

pub use classify::{classify_pr, classify_state, ClassifyConfig, FlakySignature};
pub use collect::{collect_graph, CollectOptions};
pub use fixture::FakeHost;
pub use host::{GitHubHost, VcsHost};
pub use model::{CollectedGraph, PlanResult, PrAction, PrNode, ReviewBinding};
pub use plan::{assemble_result, compute_plan};

/// Package version reported by the command-line interface.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
