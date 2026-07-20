//! Provider/router-aware execution fingerprints for workflow prefix replay.
//!
//! Per-call replay keys already include explicit `agent()` model/effort/role
//! options. They cannot, however, distinguish omitted options that inherit a
//! different provider, model, service tier, or changed role layer on resume.
//! This module produces one non-secret run-level digest over those inherited
//! execution facts. A mismatch disables the whole replay prefix.

use std::collections::BTreeMap;

use codex_model_provider_info::ModelProviderInfo;
use codex_workflow_journal::execution_fingerprint;
use serde_json::Value;
use serde_json::json;
use url::Url;

use super::ExecContext;
use crate::agent::role::apply_workflow_role_to_config;
use crate::agent::role::available_role_names;
use crate::config::Config;

/// Compute the opaque fingerprint persisted in workflow run metadata.
pub(crate) async fn for_exec(exec: &ExecContext) -> String {
    let turn = exec.turn.as_ref();
    let base_instructions = exec.session.get_base_instructions().await;
    let mut roles = BTreeMap::new();

    for role_name in available_role_names(&turn.config) {
        let mut role_config = turn.config.as_ref().clone();
        let role = match apply_workflow_role_to_config(&mut role_config, Some(&role_name)).await {
            Ok(()) => safe_config_identity(&role_config),
            Err(_) => json!({ "status": "unavailable" }),
        };
        roles.insert(role_name, role);
    }

    execution_fingerprint(&json!({
        "authMode": exec.session.services.auth_manager.auth_mode(),
        "baseInstructions": base_instructions.text,
        "collaborationMode": turn.collaboration_mode,
        "effectiveReasoningEffort": turn.effective_reasoning_effort(),
        "modelInfo": turn.model_info,
        "root": safe_config_identity(&turn.config),
        "roles": roles,
    }))
}

fn safe_config_identity(config: &Config) -> Value {
    json!({
        "baseInstructions": config.base_instructions,
        "developerInstructions": config.developer_instructions,
        "model": config.model,
        "modelCatalog": config.model_catalog,
        "provider": safe_provider_identity(&config.model_provider_id, &config.model_provider),
        "reasoningEffort": config.model_reasoning_effort,
        "serviceTier": config.service_tier,
    })
}

fn safe_provider_identity(provider_id: &str, provider: &ModelProviderInfo) -> Value {
    let mut http_header_names = provider
        .http_headers
        .as_ref()
        .into_iter()
        .flat_map(|headers| headers.keys())
        .map(|name| name.to_ascii_lowercase())
        .collect::<Vec<_>>();
    http_header_names.sort_unstable();
    http_header_names.dedup();

    let mut env_header_names = provider
        .env_http_headers
        .as_ref()
        .into_iter()
        .flat_map(|headers| headers.keys())
        .map(|name| name.to_ascii_lowercase())
        .collect::<Vec<_>>();
    env_header_names.sort_unstable();
    env_header_names.dedup();

    let mut query_param_names = provider
        .query_params
        .as_ref()
        .into_iter()
        .flat_map(|params| params.keys())
        .cloned()
        .collect::<Vec<_>>();
    query_param_names.sort_unstable();
    query_param_names.dedup();

    json!({
        "authCommandConfigured": provider.auth.is_some(),
        "awsProfile": provider.aws.as_ref().and_then(|aws| aws.profile.as_deref()),
        "awsRegion": provider.aws.as_ref().and_then(|aws| aws.region.as_deref()),
        "baseUrl": sanitized_base_url(provider.base_url.as_deref()),
        "bearerTokenConfigured": provider.experimental_bearer_token.is_some(),
        "envHeaderNames": env_header_names,
        "envKeyName": provider.env_key,
        "httpHeaderNames": http_header_names,
        "id": provider_id,
        "name": provider.name,
        "queryParamNames": query_param_names,
        "requiresOpenAiAuth": provider.requires_openai_auth,
        "supportsWebsockets": provider.supports_websockets,
        "wireApi": provider.wire_api,
    })
}

fn sanitized_base_url(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    let mut url = Url::parse(raw).ok()?;
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    Some(url.to_string())
}

#[cfg(test)]
#[path = "workflow_replay_fingerprint_tests.rs"]
mod tests;
