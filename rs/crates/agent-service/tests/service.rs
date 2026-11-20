//! Go `tests/service`: one test binary, a module per Go file.

#[path = "service/common.rs"]
mod common;

#[path = "service/adopt_test.rs"]
mod adopt_test;
#[path = "service/agent_service_test.rs"]
mod agent_service_test;
#[path = "service/agy_held_test.rs"]
mod agy_held_test;
#[path = "service/agy_spawn_test.rs"]
mod agy_spawn_test;
#[path = "service/allow_test.rs"]
mod allow_test;
#[path = "service/caller_test.rs"]
mod caller_test;
#[path = "service/claude_hook_test.rs"]
mod claude_hook_test;
#[path = "service/dialog_test.rs"]
mod dialog_test;
#[path = "service/events_test.rs"]
mod events_test;
#[path = "service/handover_test.rs"]
mod handover_test;
#[path = "service/held_test.rs"]
mod held_test;
#[path = "service/model_test.rs"]
mod model_test;
#[path = "service/prompts_test.rs"]
mod prompts_test;
#[path = "service/resume_test.rs"]
mod resume_test;
#[path = "service/rpc_spawn_test.rs"]
mod rpc_spawn_test;
#[path = "service/rpc_test.rs"]
mod rpc_test;
#[path = "service/status_review_test.rs"]
mod status_review_test;
#[path = "service/status_test.rs"]
mod status_test;
#[path = "service/submit_test.rs"]
mod submit_test;
#[path = "service/trust_test.rs"]
mod trust_test;
