#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]

#[path = "../common/mod.rs"]
mod common;

mod protocol_frame;

mod workflow_dagrs_contract;
mod workflow_spec;

mod core_agent;
mod core_compact;
mod core_ledger;
mod core_pricing;
mod core_providers_anthropic;
mod core_providers_config;
mod core_providers_genai;
mod core_recovery;
mod core_sandbox;
mod core_sandbox_bubblewrap;
mod core_sandbox_seatbelt;
mod core_sandbox_seatbelt_denies;
mod core_secrets;
mod core_store;
mod core_telemetry;
mod core_tools_confinement;
mod core_tools_edit;
mod core_tools_glob;
mod core_tools_grep;
mod core_tools_read;
mod core_tools_shell;

mod cli_binary_crash_resume;
mod cli_binary_e2e;
mod cli_daemon_round_trip;
mod cli_rpc;
mod cli_socket;
