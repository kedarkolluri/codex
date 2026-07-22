use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;

use super::default_project_root_markers;
use super::effective_project_root_markers;
use crate::ConfigLayerEntry;
use crate::ConfigLayerSource;
use crate::ConfigLayerStack;
use crate::ConfigRequirements;
use crate::ConfigRequirementsToml;

fn stack(layers: Vec<ConfigLayerEntry>) -> ConfigLayerStack {
    ConfigLayerStack::new(
        layers,
        ConfigRequirements::default(),
        ConfigRequirementsToml::default(),
    )
    .expect("test config layers should be ordered")
}

fn config(markers: &str) -> toml::Value {
    toml::from_str(&format!("project_root_markers = {markers}"))
        .expect("project-root marker config should parse")
}

fn test_path(name: &str) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(
        std::env::current_dir()
            .expect("current directory should be available")
            .join(name),
    )
    .expect("test path should be absolute")
}

#[test]
fn defaults_when_markers_are_unset() {
    assert_eq!(
        effective_project_root_markers(&ConfigLayerStack::default()),
        default_project_root_markers()
    );
}

#[test]
fn merges_enabled_non_project_layers_in_precedence_order() {
    let disabled_user = ConfigLayerEntry::new_disabled(
        ConfigLayerSource::User {
            file: test_path("disabled-user-config.toml"),
            profile: None,
        },
        config(r#"["disabled"]"#),
        "disabled for test",
    );
    let layers = vec![
        ConfigLayerEntry::new(
            ConfigLayerSource::Mdm {
                domain: "example".to_string(),
                key: "markers".to_string(),
            },
            config(r#"["managed"]"#),
        ),
        disabled_user,
        ConfigLayerEntry::new(
            ConfigLayerSource::SessionFlags,
            config(r#"["session", ".git"]"#),
        ),
    ];

    assert_eq!(
        effective_project_root_markers(&stack(layers)),
        vec!["session".to_string(), ".git".to_string()]
    );
}

#[test]
fn ignores_project_layers_when_resolving_their_boundary() {
    let layers = vec![
        ConfigLayerEntry::new(
            ConfigLayerSource::User {
                file: test_path("user-config.toml"),
                profile: None,
            },
            config(r#"["user-marker"]"#),
        ),
        ConfigLayerEntry::new(
            ConfigLayerSource::Project {
                dot_codex_folder: test_path("project/.codex"),
            },
            config(r#"["project-marker"]"#),
        ),
    ];

    assert_eq!(
        effective_project_root_markers(&stack(layers)),
        vec!["user-marker".to_string()]
    );
}

#[test]
fn preserves_an_explicit_empty_marker_list() {
    let layers = vec![ConfigLayerEntry::new(
        ConfigLayerSource::SessionFlags,
        config("[]"),
    )];

    assert_eq!(
        effective_project_root_markers(&stack(layers)),
        Vec::<String>::new()
    );
}

#[test]
fn invalid_markers_fall_back_to_the_default() {
    let invalid = toml::from_str(r#"project_root_markers = "not-a-list""#)
        .expect("invalid marker shape should still be valid TOML");
    let layers = vec![ConfigLayerEntry::new(
        ConfigLayerSource::SessionFlags,
        invalid,
    )];

    assert_eq!(
        effective_project_root_markers(&stack(layers)),
        default_project_root_markers()
    );
}
