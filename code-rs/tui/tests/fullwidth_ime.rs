use code_tui::chatwidget::message::UserMessage;

#[test]
fn preserves_fullwidth_space_only_message() {
    let fullwidth_space = "\u{3000}".repeat(2);
    let msg = UserMessage::from(fullwidth_space.clone());
    assert_eq!(msg.display_text, fullwidth_space);
    assert_eq!(msg.ordered_items.len(), 1, "full-width space should be treated as content");
}

