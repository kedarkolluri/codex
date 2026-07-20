use codex_features::FeatureToml;
use codex_features::FeaturesToml;

/// Rejects explicit dependency disables before feature normalization erases the
/// distinction between an unset dependency and one configured as `false`.
pub(super) fn validate_workflow_feature_dependencies(
    features: Option<&FeaturesToml>,
    workflow_enabled: bool,
) -> std::io::Result<()> {
    if !workflow_enabled {
        return Ok(());
    }

    let code_mode_disabled = features
        .and_then(|features| features.code_mode.as_ref())
        .and_then(FeatureToml::enabled)
        == Some(false);
    let multi_agent_v2_disabled = features
        .and_then(|features| features.multi_agent_v2.as_ref())
        .and_then(FeatureToml::enabled)
        == Some(false);
    for (dependency, disabled) in [
        ("code_mode", code_mode_disabled),
        ("multi_agent_v2", multi_agent_v2_disabled),
    ] {
        if disabled {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "`features.workflow` requires `features.{dependency}`; remove the explicit `enabled = false` override or enable `features.{dependency}`"
                ),
            ));
        }
    }
    Ok(())
}
