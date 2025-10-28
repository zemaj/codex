use code_core::sanitize_tool_call_arguments;
use serde_json::Value;

#[test]
fn valid_object_left_intact() {
    let input = r#"{"key": "value"}"#;
    let sanitized = sanitize_tool_call_arguments(input);
    assert_eq!(sanitized, input);
}

#[test]
fn strips_markdown_fence_with_language() {
    let input = "```json\n{\n  \"answer\": 42\n}\n```";
    let sanitized = sanitize_tool_call_arguments(input);
    let value: Value = serde_json::from_str(&sanitized).unwrap();
    assert_eq!(value["answer"], 42);
}

#[test]
fn extracts_json_after_prose() {
    let input = "Here you go:\n[{'oops': 'not json'}]{\"name\":\"ok\"}";
    // Replace single quotes to keep it invalid except for the object portion.
    let input = input.replace("'", "\"");
    let sanitized = sanitize_tool_call_arguments(&input);
    let value: Value = serde_json::from_str(&sanitized).unwrap();
    assert_eq!(value["name"], "ok");
}

#[test]
fn handles_arrays() {
    let input = "```\n[ {\"tool\": 1}, {\"tool\": 2} ]\n```";
    let sanitized = sanitize_tool_call_arguments(input);
    let value: Value = serde_json::from_str(&sanitized).unwrap();
    assert!(value.is_array());
    assert_eq!(value.as_array().unwrap().len(), 2);
}

#[test]
fn preserves_braces_inside_strings() {
    let input = r#"{"msg": "brace } inside string"}"#;
    let sanitized = sanitize_tool_call_arguments(input);
    let value: Value = serde_json::from_str(&sanitized).unwrap();
    assert_eq!(value["msg"], "brace } inside string");
}

#[test]
fn returns_trimmed_on_failure() {
    let input = "```json\n{ broken json\n```";
    let sanitized = sanitize_tool_call_arguments(input);
    assert!(!sanitized.contains("```"));
    assert!(!sanitized.is_empty());
}

#[test]
fn whitespace_only_yields_empty() {
    let sanitized = sanitize_tool_call_arguments("   \n\t");
    assert!(sanitized.is_empty());
}
