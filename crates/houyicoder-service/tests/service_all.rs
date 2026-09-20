//! Consolidated service contract tests. Each module is the original test
//! file included verbatim via path attributes; aggregating into one binary
//! cuts nextest discovery spawn count without dropping any test. ACP
//! feature-gated tests and the live reward_bench stay separate binaries.

mod common;

#[path = "acp_contract.rs"]
mod acp_contract;
#[path = "ask_deadlock.rs"]
mod ask_deadlock;
#[path = "client_server_contract.rs"]
mod client_server_contract;
#[path = "composition_isolation.rs"]
mod composition_isolation;
#[path = "concurrent_mode_switch.rs"]
mod concurrent_mode_switch;
#[path = "delegation_prompt_displacement.rs"]
mod delegation_prompt_displacement;
#[path = "diagnostics_reload.rs"]
mod diagnostics_reload;
#[path = "event_delivery.rs"]
mod event_delivery;
#[path = "hook_real_spawn.rs"]
mod hook_real_spawn;
#[path = "lifecycle_control_lease.rs"]
mod lifecycle_control_lease;
#[path = "memory_commands.rs"]
mod memory_commands;
#[path = "model_contract.rs"]
mod model_contract;
#[path = "model_switch_concurrent.rs"]
mod model_switch_concurrent;
#[path = "model_switch_window.rs"]
mod model_switch_window;
#[path = "permission_contract.rs"]
mod permission_contract;
#[path = "server_contract.rs"]
mod server_contract;
#[path = "session_reset_contract.rs"]
mod session_reset_contract;
#[path = "skill_contract.rs"]
mod skill_contract;
#[path = "status_contract.rs"]
mod status_contract;
#[path = "tools_contract.rs"]
mod tools_contract;
#[path = "trajectory_contract.rs"]
mod trajectory_contract;
