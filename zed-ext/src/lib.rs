//! Zed extension shell for the lean-goalview DAP adapter: registers the
//! "lean-goalview" debug adapter and points it at the lean-goalview-dap
//! binary. The adapter shows the Lean goal state (fed by the lean-goalview
//! LSP proxy over its local socket) in Zed's native debug panel.

use zed_extension_api::{
    self as zed, DebugAdapterBinary, DebugConfig, DebugScenario, DebugTaskDefinition, Result,
    StartDebuggingRequestArguments, StartDebuggingRequestArgumentsRequest,
};

struct LeanGoalviewExtension;

fn adapter_path(worktree: &zed::Worktree) -> String {
    worktree.which("lean-goalview-dap").unwrap_or_else(|| {
        let home = worktree
            .shell_env()
            .into_iter()
            .find(|(k, _)| k == "HOME")
            .map(|(_, v)| v)
            .unwrap_or_default();
        format!("{home}/.local/bin/lean-goalview-dap")
    })
}

impl zed::Extension for LeanGoalviewExtension {
    fn new() -> Self {
        LeanGoalviewExtension
    }

    fn get_dap_binary(
        &mut self,
        _adapter_name: String,
        config: DebugTaskDefinition,
        user_provided_debug_adapter_path: Option<String>,
        worktree: &zed::Worktree,
    ) -> Result<DebugAdapterBinary> {
        let command = user_provided_debug_adapter_path.unwrap_or_else(|| adapter_path(worktree));
        Ok(DebugAdapterBinary {
            command: Some(command),
            arguments: vec![],
            envs: vec![],
            cwd: None,
            connection: None,
            request_args: StartDebuggingRequestArguments {
                configuration: config.config,
                request: StartDebuggingRequestArgumentsRequest::Launch,
            },
        })
    }

    fn dap_request_kind(
        &mut self,
        _adapter_name: String,
        _config: zed::serde_json::Value,
    ) -> Result<StartDebuggingRequestArgumentsRequest> {
        Ok(StartDebuggingRequestArgumentsRequest::Launch)
    }

    fn dap_config_to_scenario(&mut self, config: DebugConfig) -> Result<DebugScenario> {
        Ok(DebugScenario {
            label: config.label,
            adapter: config.adapter,
            build: None,
            config: "{}".to_string(),
            tcp_connection: None,
        })
    }
}

zed::register_extension!(LeanGoalviewExtension);
