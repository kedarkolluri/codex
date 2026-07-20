use std::collections::HashMap;

use codex_model_provider_info::ModelProviderInfo;
use pretty_assertions::assert_eq;

use super::safe_provider_identity;

#[test]
fn provider_identity_omits_secret_and_routing_values() {
    let provider = ModelProviderInfo {
        name: "router".to_string(),
        base_url: Some("https://user:password@router.example/v1?api_key=query-secret".to_string()),
        env_key: Some("ROUTER_API_KEY".to_string()),
        experimental_bearer_token: Some("bearer-secret".to_string()),
        query_params: Some(HashMap::from([(
            "api_key".to_string(),
            "query-secret".to_string(),
        )])),
        http_headers: Some(HashMap::from([(
            "X-Router-Key".to_string(),
            "header-secret".to_string(),
        )])),
        env_http_headers: Some(HashMap::from([(
            "X-Env-Key".to_string(),
            "ROUTER_SECRET".to_string(),
        )])),
        ..Default::default()
    };

    let identity = safe_provider_identity("headroom", &provider);
    let encoded = serde_json::to_string(&identity).expect("serialize identity");
    assert!(!encoded.contains("password"));
    assert!(!encoded.contains("query-secret"));
    assert!(!encoded.contains("bearer-secret"));
    assert!(!encoded.contains("header-secret"));
    assert!(!encoded.contains("ROUTER_SECRET"));
    assert!(encoded.contains("router.example"));
    assert!(encoded.contains("x-router-key"));
    assert!(encoded.contains("api_key"));
}

#[test]
fn secret_rotation_does_not_change_identity_but_route_shape_does() {
    let mut first = ModelProviderInfo {
        base_url: Some("https://router.example/v1".to_string()),
        experimental_bearer_token: Some("first".to_string()),
        http_headers: Some(HashMap::from([(
            "X-Router-Key".to_string(),
            "first".to_string(),
        )])),
        ..Default::default()
    };
    let mut second = first.clone();
    second.experimental_bearer_token = Some("second".to_string());
    second
        .http_headers
        .as_mut()
        .expect("headers")
        .insert("X-Router-Key".to_string(), "second".to_string());

    assert_eq!(
        safe_provider_identity("headroom", &first),
        safe_provider_identity("headroom", &second)
    );

    first.base_url = Some("https://other-router.example/v1".to_string());
    assert_ne!(
        safe_provider_identity("headroom", &first),
        safe_provider_identity("headroom", &second)
    );
}
