use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use crate::managed_agents::{
    cache_agent_models_result, normalize_agent_models, run_agent_models_process,
    AgentModelsResponse,
};

pub(super) async fn run_agent_models_command(
    resolved_acp: PathBuf,
    agent_command: String,
    agent_args: Vec<String>,
    persisted_model: Option<String>,
    merged_env: BTreeMap<String, String>,
) -> Result<AgentModelsResponse, String> {
    let command_for_cache = agent_command.clone();
    let args_for_cache = agent_args.clone();
    let env_for_cache = merged_env.clone();
    let raw = tokio::task::spawn_blocking(move || {
        run_agent_models_process(
            &resolved_acp,
            &agent_command,
            &agent_args,
            &merged_env,
            Duration::from_secs(15),
        )
    })
    .await
    .map_err(|error| format!("model discovery task failed: {error}"))??;

    let response = normalize_agent_models(&raw, persisted_model);
    cache_agent_models_result(
        &command_for_cache,
        &args_for_cache,
        &env_for_cache,
        &response,
    );
    Ok(response)
}
