use std::path::Path;

use app_test_support::McpProcess;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_chat_completions_server;
use app_test_support::to_response;
use codex_app_server_protocol::AddConversationListenerParams;
use codex_app_server_protocol::AddConversationSubscriptionResponse;
use codex_app_server_protocol::InputItem;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::NewConversationParams;
use codex_app_server_protocol::NewConversationResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SendUserMessageParams;
use codex_app_server_protocol::SendUserMessageResponse;
use codex_protocol::ConversationId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::test]
async fn test_send_message_success() {
    // Spin up a mock completions server that immediately ends the Codex turn.
    // Two Codex turns hit the mock model (session start + send-user-message). Provide two SSE responses.
    let responses = vec![
        create_final_assistant_message_sse_response("Done").expect("build mock assistant message"),
        create_final_assistant_message_sse_response("Done").expect("build mock assistant message"),
    ];
    let server = create_mock_chat_completions_server(responses).await;

    // Create a temporary Codex home with config pointing at the mock server.
    let codex_home = TempDir::new().expect("create temp dir");
    create_config_toml(codex_home.path(), &server.uri()).expect("write config.toml");

    // Start MCP server process and initialize.
    let mut mcp = McpProcess::new(codex_home.path())
        .await
        .expect("spawn mcp process");
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize())
        .await
        .expect("init timed out")
        .expect("init failed");

    // Start a conversation using the new wire API.
    let new_conv_id = mcp
        .send_new_conversation_request(NewConversationParams::default())
        .await
        .expect("send newConversation");
    let new_conv_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(new_conv_id)),
    )
    .await
    .expect("newConversation timeout")
    .expect("newConversation resp");
    let NewConversationResponse {
        conversation_id, ..
    } = to_response::<_>(new_conv_resp).expect("deserialize newConversation response");

    // 2) addConversationListener
    let add_listener_id = mcp
        .send_add_conversation_listener_request(AddConversationListenerParams {
            conversation_id,
            experimental_raw_events: false,
        })
        .await
        .expect("send addConversationListener");
    let add_listener_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(add_listener_id)),
    )
    .await
    .expect("addConversationListener timeout")
    .expect("addConversationListener resp");
    let AddConversationSubscriptionResponse { subscription_id: _ } =
        to_response::<_>(add_listener_resp).expect("deserialize addConversationListener response");

    // Now exercise sendUserMessage twice.
    send_message("Hello", conversation_id, &mut mcp).await;
    send_message("Hello again", conversation_id, &mut mcp).await;
}

#[expect(clippy::expect_used)]
async fn send_message(message: &str, conversation_id: ConversationId, mcp: &mut McpProcess) {
    // Now exercise sendUserMessage.
    let send_id = mcp
        .send_send_user_message_request(SendUserMessageParams {
            conversation_id,
            items: vec![InputItem::Text {
                text: message.to_string(),
            }],
        })
        .await
        .expect("send sendUserMessage");

    let response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(send_id)),
    )
    .await
    .expect("sendUserMessage response timeout")
    .expect("sendUserMessage response error");

    let _ok: SendUserMessageResponse = to_response::<SendUserMessageResponse>(response)
        .expect("deserialize sendUserMessage response");

    // Verify the task_finished notification is received.
    // Note this also ensures that the final request to the server was made.
    let task_finished_notification: JSONRPCNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("codex/event/task_complete"),
    )
    .await
    .expect("task_finished_notification timeout")
    .expect("task_finished_notification resp");
    let serde_json::Value::Object(map) = task_finished_notification
        .params
        .expect("notification should have params")
    else {
        panic!("task_finished_notification should have params");
    };
    assert_eq!(
        map.get("conversationId")
            .expect("should have conversationId"),
        &serde_json::Value::String(conversation_id.to_string())
    );

    let raw_attempt = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        mcp.read_stream_until_notification_message("codex/event/raw_response_item"),
    )
    .await;
    assert!(
        raw_attempt.is_err(),
        "unexpected raw item notification when not opted in"
    );
}

#[tokio::test]
async fn test_send_message_raw_notifications_opt_in() {
    let responses = vec![
        create_final_assistant_message_sse_response("Done").expect("build mock assistant message"),
    ];
    let server = create_mock_chat_completions_server(responses).await;

    let codex_home = TempDir::new().expect("create temp dir");
    create_config_toml(codex_home.path(), &server.uri()).expect("write config.toml");

    let mut mcp = McpProcess::new(codex_home.path())
        .await
        .expect("spawn mcp process");
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize())
        .await
        .expect("init timed out")
        .expect("init failed");

    let new_conv_id = mcp
        .send_new_conversation_request(NewConversationParams::default())
        .await
        .expect("send newConversation");
    let new_conv_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(new_conv_id)),
    )
    .await
    .expect("newConversation timeout")
    .expect("newConversation resp");
    let NewConversationResponse {
        conversation_id, ..
    } = to_response::<_>(new_conv_resp).expect("deserialize newConversation response");

    let add_listener_id = mcp
        .send_add_conversation_listener_request(AddConversationListenerParams {
            conversation_id,
            experimental_raw_events: true,
        })
        .await
        .expect("send addConversationListener");
    let add_listener_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(add_listener_id)),
    )
    .await
    .expect("addConversationListener timeout")
    .expect("addConversationListener resp");
    let AddConversationSubscriptionResponse { subscription_id: _ } =
        to_response::<_>(add_listener_resp).expect("deserialize addConversationListener response");

    let send_id = mcp
        .send_send_user_message_request(SendUserMessageParams {
            conversation_id,
            items: vec![InputItem::Text {
                text: "Hello".to_string(),
            }],
        })
        .await
        .expect("send sendUserMessage");

    let instructions = read_raw_response_item(&mut mcp, conversation_id).await;
    assert_instructions_message(&instructions);

    let environment = read_raw_response_item(&mut mcp, conversation_id).await;
    assert_environment_message(&environment);

    let response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(send_id)),
    )
    .await
    .expect("sendUserMessage response timeout")
    .expect("sendUserMessage response error");
    let _ok: SendUserMessageResponse = to_response::<SendUserMessageResponse>(response)
        .expect("deserialize sendUserMessage response");

    let user_message = read_raw_response_item(&mut mcp, conversation_id).await;
    assert_user_message(&user_message, "Hello");

    let assistant_message = read_raw_response_item(&mut mcp, conversation_id).await;
    assert_assistant_message(&assistant_message, "Done");

    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        mcp.read_stream_until_notification_message("codex/event/task_complete"),
    )
    .await;
}

#[tokio::test]
async fn test_send_message_session_not_found() {
    // Start MCP without creating a Codex session
    let codex_home = TempDir::new().expect("tempdir");
    let mut mcp = McpProcess::new(codex_home.path()).await.expect("spawn");
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize())
        .await
        .expect("timeout")
        .expect("init");

    let unknown = ConversationId::new();
    let req_id = mcp
        .send_send_user_message_request(SendUserMessageParams {
            conversation_id: unknown,
            items: vec![InputItem::Text {
                text: "ping".to_string(),
            }],
        })
        .await
        .expect("send sendUserMessage");

    // Expect an error response for unknown conversation.
    let err = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(req_id)),
    )
    .await
    .expect("timeout")
    .expect("error");
    assert_eq!(err.id, RequestId::Integer(req_id));
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn create_config_toml(codex_home: &Path, server_uri: &str) -> std::io::Result<()> {
    let config_toml = codex_home.join("config.toml");
    std::fs::write(
        config_toml,
        format!(
            r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "danger-full-access"

model_provider = "mock_provider"

[model_providers.mock_provider]
name = "Mock provider for test"
base_url = "{server_uri}/v1"
wire_api = "chat"
request_max_retries = 0
stream_max_retries = 0
"#
        ),
    )
}

#[expect(clippy::expect_used)]
async fn read_raw_response_item(
    mcp: &mut McpProcess,
    conversation_id: ConversationId,
) -> ResponseItem {
    let raw_notification: JSONRPCNotification = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("codex/event/raw_response_item"),
    )
    .await
    .expect("codex/event/raw_response_item notification timeout")
    .expect("codex/event/raw_response_item notification resp");

    let serde_json::Value::Object(params) = raw_notification
        .params
        .expect("codex/event/raw_response_item should have params")
    else {
        panic!("codex/event/raw_response_item should have params");
    };

    let conversation_id_value = params
        .get("conversationId")
        .and_then(|value| value.as_str())
        .expect("raw response item should include conversationId");

    assert_eq!(
        conversation_id_value,
        conversation_id.to_string(),
        "raw response item conversation mismatch"
    );

    let msg_value = params
        .get("msg")
        .cloned()
        .expect("raw response item should include msg payload");

    serde_json::from_value(msg_value).expect("deserialize raw response item")
}

fn assert_instructions_message(item: &ResponseItem) {
    match item {
        ResponseItem::Message { role, content, .. } => {
            assert_eq!(role, "user");
            let texts = content_texts(content);
            assert!(
                texts
                    .iter()
                    .any(|text| text.contains("<user_instructions>")),
                "expected instructions message, got {texts:?}"
            );
        }
        other => panic!("expected instructions message, got {other:?}"),
    }
}

fn assert_environment_message(item: &ResponseItem) {
    match item {
        ResponseItem::Message { role, content, .. } => {
            assert_eq!(role, "user");
            let texts = content_texts(content);
            assert!(
                texts
                    .iter()
                    .any(|text| text.contains("<environment_context>")),
                "expected environment context message, got {texts:?}"
            );
        }
        other => panic!("expected environment message, got {other:?}"),
    }
}

fn assert_user_message(item: &ResponseItem, expected_text: &str) {
    match item {
        ResponseItem::Message { role, content, .. } => {
            assert_eq!(role, "user");
            let texts = content_texts(content);
            assert_eq!(texts, vec![expected_text]);
        }
        other => panic!("expected user message, got {other:?}"),
    }
}

fn assert_assistant_message(item: &ResponseItem, expected_text: &str) {
    match item {
        ResponseItem::Message { role, content, .. } => {
            assert_eq!(role, "assistant");
            let texts = content_texts(content);
            assert_eq!(texts, vec![expected_text]);
        }
        other => panic!("expected assistant message, got {other:?}"),
    }
}

fn content_texts(content: &[ContentItem]) -> Vec<&str> {
    content
        .iter()
        .filter_map(|item| match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect()
}
