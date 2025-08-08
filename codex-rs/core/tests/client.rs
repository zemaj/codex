#![allow(clippy::expect_used, clippy::unwrap_used)]

use codex_core::Codex;
use codex_core::CodexSpawnOk;
use codex_core::ModelProviderInfo;
use codex_core::WireApi;
use codex_core::built_in_model_providers;
use codex_core::protocol::EventMsg;
use codex_core::protocol::InputItem;
use codex_core::protocol::Op;
use codex_core::protocol::SessionConfiguredEvent;
use codex_core::spawn::CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR;
use codex_login::CodexAuth;
use core_test_support::load_default_config_for_test;
use core_test_support::load_sse_fixture_with_id;
use core_test_support::wait_for_event;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header_regex;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::matchers::query_param;

/// Build minimal SSE stream with completed marker using the JSON fixture.
fn sse_completed(id: &str) -> String {
    load_sse_fixture_with_id("tests/fixtures/completed_template.json", id)
}

fn assert_message_role(request_body: &serde_json::Value, role: &str) {
    assert_eq!(request_body["role"].as_str().unwrap(), role);
}

fn assert_message_starts_with(request_body: &serde_json::Value, text: &str) {
    let content = request_body["content"][0]["text"]
        .as_str()
        .expect("invalid message content");

    assert!(
        content.starts_with(text),
        "expected message content '{content}' to start with '{text}'"
    );
}

fn assert_message_ends_with(request_body: &serde_json::Value, text: &str) {
    let content = request_body["content"][0]["text"]
        .as_str()
        .expect("invalid message content");

    assert!(
        content.ends_with(text),
        "expected message content '{content}' to end with '{text}'"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn includes_session_id_and_model_headers_in_request() {
    #![allow(clippy::unwrap_used)]

    if std::env::var(CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
        println!(
            "Skipping test because it cannot execute when network is disabled in a Codex sandbox."
        );
        return;
    }

    // Mock server
    let server = MockServer::start().await;

    // First request – must NOT include `previous_response_id`.
    let first = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(sse_completed("resp1"), "text/event-stream");

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(first)
        .expect(1)
        .mount(&server)
        .await;

    let model_provider = ModelProviderInfo {
        base_url: Some(format!("{}/v1", server.uri())),
        ..built_in_model_providers()["openai"].clone()
    };

    // Init session
    let codex_home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider = model_provider;

    let ctrl_c = std::sync::Arc::new(tokio::sync::Notify::new());
    let CodexSpawnOk { codex, .. } = Codex::spawn(
        config,
        Some(CodexAuth::from_api_key("Test API Key")),
        ctrl_c.clone(),
    )
    .await
    .unwrap();

    codex
        .submit(Op::UserInput {
            items: vec![InputItem::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    let EventMsg::SessionConfigured(SessionConfiguredEvent { session_id, .. }) =
        wait_for_event(&codex, |ev| matches!(ev, EventMsg::SessionConfigured(_))).await
    else {
        unreachable!()
    };

    let current_session_id = Some(session_id.to_string());
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    // get request from the server
    let request = &server.received_requests().await.unwrap()[0];
    let request_session_id = request.headers.get("session_id").unwrap();
    let request_authorization = request.headers.get("authorization").unwrap();
    let request_originator = request.headers.get("originator").unwrap();

    assert!(current_session_id.is_some());
    assert_eq!(
        request_session_id.to_str().unwrap(),
        current_session_id.as_ref().unwrap()
    );
    assert_eq!(request_originator.to_str().unwrap(), "codex_cli_rs");
    assert_eq!(
        request_authorization.to_str().unwrap(),
        "Bearer Test API Key"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn includes_base_instructions_override_in_request() {
    #![allow(clippy::unwrap_used)]

    // Mock server
    let server = MockServer::start().await;

    // First request – must NOT include `previous_response_id`.
    let first = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(sse_completed("resp1"), "text/event-stream");

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(first)
        .expect(1)
        .mount(&server)
        .await;

    let model_provider = ModelProviderInfo {
        base_url: Some(format!("{}/v1", server.uri())),
        ..built_in_model_providers()["openai"].clone()
    };
    let codex_home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&codex_home);

    config.base_instructions = Some("test instructions".to_string());
    config.model_provider = model_provider;

    let ctrl_c = std::sync::Arc::new(tokio::sync::Notify::new());
    let CodexSpawnOk { codex, .. } = Codex::spawn(
        config,
        Some(CodexAuth::from_api_key("Test API Key")),
        ctrl_c.clone(),
    )
    .await
    .unwrap();

    codex
        .submit(Op::UserInput {
            items: vec![InputItem::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    let request = &server.received_requests().await.unwrap()[0];
    let request_body = request.body_json::<serde_json::Value>().unwrap();

    assert!(
        request_body["instructions"]
            .as_str()
            .unwrap()
            .contains("test instructions")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn originator_config_override_is_used() {
    #![allow(clippy::unwrap_used)]

    // Mock server
    let server = MockServer::start().await;

    let first = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(sse_completed("resp1"), "text/event-stream");

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(first)
        .expect(1)
        .mount(&server)
        .await;

    let model_provider = ModelProviderInfo {
        base_url: Some(format!("{}/v1", server.uri())),
        ..built_in_model_providers()["openai"].clone()
    };

    let codex_home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider = model_provider;
    config.internal_originator = Some("my_override".to_string());

    let ctrl_c = std::sync::Arc::new(tokio::sync::Notify::new());
    let CodexSpawnOk { codex, .. } = Codex::spawn(
        config,
        Some(CodexAuth::from_api_key("Test API Key")),
        ctrl_c.clone(),
    )
    .await
    .unwrap();

    codex
        .submit(Op::UserInput {
            items: vec![InputItem::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    let request = &server.received_requests().await.unwrap()[0];
    let request_originator = request.headers.get("originator").unwrap();
    assert_eq!(request_originator.to_str().unwrap(), "my_override");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chatgpt_auth_sends_correct_request() {
    #![allow(clippy::unwrap_used)]

    if std::env::var(CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR).is_ok() {
        println!(
            "Skipping test because it cannot execute when network is disabled in a Codex sandbox."
        );
        return;
    }

    // Mock server
    let server = MockServer::start().await;

    // First request – must NOT include `previous_response_id`.
    let first = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(sse_completed("resp1"), "text/event-stream");

    Mock::given(method("POST"))
        .and(path("/api/codex/responses"))
        .respond_with(first)
        .expect(1)
        .mount(&server)
        .await;

    let model_provider = ModelProviderInfo {
        base_url: Some(format!("{}/api/codex", server.uri())),
        ..built_in_model_providers()["openai"].clone()
    };

    // Init session
    let codex_home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider = model_provider;
    let ctrl_c = std::sync::Arc::new(tokio::sync::Notify::new());
    let CodexSpawnOk { codex, .. } =
        Codex::spawn(config, Some(create_dummy_codex_auth()), ctrl_c.clone())
            .await
            .unwrap();

    codex
        .submit(Op::UserInput {
            items: vec![InputItem::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    let EventMsg::SessionConfigured(SessionConfiguredEvent { session_id, .. }) =
        wait_for_event(&codex, |ev| matches!(ev, EventMsg::SessionConfigured(_))).await
    else {
        unreachable!()
    };

    let current_session_id = Some(session_id.to_string());
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    // get request from the server
    let request = &server.received_requests().await.unwrap()[0];
    let request_session_id = request.headers.get("session_id").unwrap();
    let request_authorization = request.headers.get("authorization").unwrap();
    let request_originator = request.headers.get("originator").unwrap();
    let request_chatgpt_account_id = request.headers.get("chatgpt-account-id").unwrap();
    let request_body = request.body_json::<serde_json::Value>().unwrap();

    assert!(current_session_id.is_some());
    assert_eq!(
        request_session_id.to_str().unwrap(),
        current_session_id.as_ref().unwrap()
    );
    assert_eq!(request_originator.to_str().unwrap(), "codex_cli_rs");
    assert_eq!(
        request_authorization.to_str().unwrap(),
        "Bearer Access Token"
    );
    assert_eq!(request_chatgpt_account_id.to_str().unwrap(), "account_id");
    assert!(!request_body["store"].as_bool().unwrap());
    assert!(request_body["stream"].as_bool().unwrap());
    assert_eq!(
        request_body["include"][0].as_str().unwrap(),
        "reasoning.encrypted_content"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn includes_user_instructions_message_in_request() {
    #![allow(clippy::unwrap_used)]

    let server = MockServer::start().await;

    let first = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(sse_completed("resp1"), "text/event-stream");

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(first)
        .expect(1)
        .mount(&server)
        .await;

    let model_provider = ModelProviderInfo {
        base_url: Some(format!("{}/v1", server.uri())),
        ..built_in_model_providers()["openai"].clone()
    };

    let codex_home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider = model_provider;
    config.user_instructions = Some("be nice".to_string());

    let ctrl_c = std::sync::Arc::new(tokio::sync::Notify::new());
    let CodexSpawnOk { codex, .. } = Codex::spawn(
        config,
        Some(CodexAuth::from_api_key("Test API Key")),
        ctrl_c.clone(),
    )
    .await
    .unwrap();

    codex
        .submit(Op::UserInput {
            items: vec![InputItem::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    let request = &server.received_requests().await.unwrap()[0];
    let request_body = request.body_json::<serde_json::Value>().unwrap();

    assert!(
        !request_body["instructions"]
            .as_str()
            .unwrap()
            .contains("be nice")
    );
    assert_message_role(&request_body["input"][0], "user");
    assert_message_starts_with(&request_body["input"][0], "<environment_context>\n\n");
    assert_message_ends_with(&request_body["input"][0], "</environment_context>");
    assert_message_role(&request_body["input"][1], "user");
    assert_message_starts_with(&request_body["input"][1], "<user_instructions>\n\n");
    assert_message_ends_with(&request_body["input"][1], "</user_instructions>");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prefixes_context_and_instructions_once_and_consistently_across_requests() {
    #![allow(clippy::unwrap_used)]

    // Mock server that will accept two requests
    let server = MockServer::start().await;

    let sse = sse_completed("resp");
    let template = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(sse, "text/event-stream");

    // Expect two POSTs to /v1/responses
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(template)
        .expect(2)
        .mount(&server)
        .await;

    let model_provider = ModelProviderInfo {
        base_url: Some(format!("{}/v1", server.uri())),
        ..built_in_model_providers()["openai"].clone()
    };

    // Init session with explicit user_instructions to validate consistency
    let codex_home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider = model_provider;
    config.user_instructions = Some("be consistent and helpful".to_string());

    let ctrl_c = std::sync::Arc::new(tokio::sync::Notify::new());
    let CodexSpawnOk { codex, .. } = Codex::spawn(
        config,
        Some(CodexAuth::from_api_key("Test API Key")),
        ctrl_c.clone(),
    )
    .await
    .unwrap();

    // First user input → first request
    codex
        .submit(Op::UserInput {
            items: vec![InputItem::Text {
                text: "hello 1".into(),
            }],
        })
        .await
        .unwrap();
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    // Second user input → second request
    codex
        .submit(Op::UserInput {
            items: vec![InputItem::Text {
                text: "hello 2".into(),
            }],
        })
        .await
        .unwrap();
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;

    // Collect both requests and inspect their bodies
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "expected two POST requests");

    let body1 = requests[0].body_json::<serde_json::Value>().unwrap();
    let body2 = requests[1].body_json::<serde_json::Value>().unwrap();

    // Build deep-equal expected prefix messages and validate with assert_eq!
    let cwd = std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let expected_env_text = format!(
        "<environment_context>\n\nCurrent working directory: {}\nApproval policy: on-request\nSandbox policy: read-only\nNetwork access: restricted\n\n</environment_context>",
        cwd
    );
    let expected_ui_text = "<user_instructions>\n\nbe consistent and helpful\n\n</user_instructions>";

    let expected_env_msg = serde_json::json!({
        "type": "message",
        "id": serde_json::Value::Null,
        "role": "user",
        "content": [ { "type": "input_text", "text": expected_env_text } ]
    });
    let expected_ui_msg = serde_json::json!({
        "type": "message",
        "id": serde_json::Value::Null,
        "role": "user",
        "content": [ { "type": "input_text", "text": expected_ui_text } ]
    });

    assert_eq!(body1["input"][0], expected_env_msg);
    assert_eq!(body1["input"][1], expected_ui_msg);
    assert_eq!(body2["input"][0], expected_env_msg);
    assert_eq!(body2["input"][1], expected_ui_msg);

    // Ensure they are prefixed exactly once: no other items in the conversation contain the tags
    let contains_tag = |v: &serde_json::Value, tag: &str| -> bool {
        v["content"][0]["text"]
            .as_str()
            .map(|t| t.contains(tag))
            .unwrap_or(false)
    };

    let mut extra_env_tags = 0usize;
    let mut extra_ui_tags = 0usize;
    for v in body1["input"].as_array().unwrap().iter().skip(2) {
        if contains_tag(v, "<environment_context>") || contains_tag(v, "</environment_context>") {
            extra_env_tags += 1;
        }
        if contains_tag(v, "<user_instructions>") || contains_tag(v, "</user_instructions>") {
            extra_ui_tags += 1;
        }
    }
    assert_eq!(
        extra_env_tags, 0,
        "environment_context appeared more than once in request 1"
    );
    assert_eq!(
        extra_ui_tags, 0,
        "user_instructions appeared more than once in request 1"
    );

    let mut extra_env_tags2 = 0usize;
    let mut extra_ui_tags2 = 0usize;
    for v in body2["input"].as_array().unwrap().iter().skip(2) {
        if contains_tag(v, "<environment_context>") || contains_tag(v, "</environment_context>") {
            extra_env_tags2 += 1;
        }
        if contains_tag(v, "<user_instructions>") || contains_tag(v, "</user_instructions>") {
            extra_ui_tags2 += 1;
        }
    }
    assert_eq!(
        extra_env_tags2, 0,
        "environment_context appeared more than once in request 2"
    );
    assert_eq!(
        extra_ui_tags2, 0,
        "user_instructions appeared more than once in request 2"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn azure_overrides_assign_properties_used_for_responses_url() {
    #![allow(clippy::unwrap_used)]

    let existing_env_var_with_random_value = if cfg!(windows) { "USERNAME" } else { "USER" };

    // Mock server
    let server = MockServer::start().await;

    // First request – must NOT include `previous_response_id`.
    let first = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(sse_completed("resp1"), "text/event-stream");

    // Expect POST to /openai/responses with api-version query param
    Mock::given(method("POST"))
        .and(path("/openai/responses"))
        .and(query_param("api-version", "2025-04-01-preview"))
        .and(header_regex("Custom-Header", "Value"))
        .and(header_regex(
            "Authorization",
            format!(
                "Bearer {}",
                std::env::var(existing_env_var_with_random_value).unwrap()
            )
            .as_str(),
        ))
        .respond_with(first)
        .expect(1)
        .mount(&server)
        .await;

    let provider = ModelProviderInfo {
        name: "custom".to_string(),
        base_url: Some(format!("{}/openai", server.uri())),
        // Reuse the existing environment variable to avoid using unsafe code
        env_key: Some(existing_env_var_with_random_value.to_string()),
        query_params: Some(std::collections::HashMap::from([(
            "api-version".to_string(),
            "2025-04-01-preview".to_string(),
        )])),
        env_key_instructions: None,
        wire_api: WireApi::Responses,
        http_headers: Some(std::collections::HashMap::from([(
            "Custom-Header".to_string(),
            "Value".to_string(),
        )])),
        env_http_headers: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        requires_openai_auth: false,
    };

    // Init session
    let codex_home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider = provider;

    let ctrl_c = std::sync::Arc::new(tokio::sync::Notify::new());
    let CodexSpawnOk { codex, .. } = Codex::spawn(config, None, ctrl_c.clone()).await.unwrap();

    codex
        .submit(Op::UserInput {
            items: vec![InputItem::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn env_var_overrides_loaded_auth() {
    #![allow(clippy::unwrap_used)]

    let existing_env_var_with_random_value = if cfg!(windows) { "USERNAME" } else { "USER" };

    // Mock server
    let server = MockServer::start().await;

    // First request – must NOT include `previous_response_id`.
    let first = ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(sse_completed("resp1"), "text/event-stream");

    // Expect POST to /openai/responses with api-version query param
    Mock::given(method("POST"))
        .and(path("/openai/responses"))
        .and(query_param("api-version", "2025-04-01-preview"))
        .and(header_regex("Custom-Header", "Value"))
        .and(header_regex(
            "Authorization",
            format!(
                "Bearer {}",
                std::env::var(existing_env_var_with_random_value).unwrap()
            )
            .as_str(),
        ))
        .respond_with(first)
        .expect(1)
        .mount(&server)
        .await;

    let provider = ModelProviderInfo {
        name: "custom".to_string(),
        base_url: Some(format!("{}/openai", server.uri())),
        // Reuse the existing environment variable to avoid using unsafe code
        env_key: Some(existing_env_var_with_random_value.to_string()),
        query_params: Some(std::collections::HashMap::from([(
            "api-version".to_string(),
            "2025-04-01-preview".to_string(),
        )])),
        env_key_instructions: None,
        wire_api: WireApi::Responses,
        http_headers: Some(std::collections::HashMap::from([(
            "Custom-Header".to_string(),
            "Value".to_string(),
        )])),
        env_http_headers: None,
        request_max_retries: None,
        stream_max_retries: None,
        stream_idle_timeout_ms: None,
        requires_openai_auth: false,
    };

    // Init session
    let codex_home = TempDir::new().unwrap();
    let mut config = load_default_config_for_test(&codex_home);
    config.model_provider = provider;

    let ctrl_c = std::sync::Arc::new(tokio::sync::Notify::new());
    let CodexSpawnOk { codex, .. } =
        Codex::spawn(config, Some(create_dummy_codex_auth()), ctrl_c.clone())
            .await
            .unwrap();

    codex
        .submit(Op::UserInput {
            items: vec![InputItem::Text {
                text: "hello".into(),
            }],
        })
        .await
        .unwrap();

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TaskComplete(_))).await;
}

fn create_dummy_codex_auth() -> CodexAuth {
    CodexAuth::create_dummy_chatgpt_auth_for_testing()
}
