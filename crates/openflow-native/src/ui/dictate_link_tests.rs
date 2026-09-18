use super::*;

#[test]
fn home_links_have_explicit_existing_destinations() {
    assert_eq!(home_link_destination(0), Some("onboarding"));
    assert_eq!(home_link_destination(1), Some("history"));
    for index in [2, 3] {
        let destination = home_link_destination(index).unwrap();
        assert!(crate::ui::settings::section_index(destination).is_some());
    }
    assert_eq!(home_link_destination(-1), None);
    assert_eq!(home_link_destination(HOME_LINKS.len() as isize), None);
    let unique: std::collections::HashSet<_> = HOME_LINKS.iter().map(|(_, route)| route).collect();
    assert_eq!(unique.len(), HOME_LINKS.len());
}

#[test]
fn recording_action_stays_in_the_initial_short_viewport() {
    let recording_bottom = MARGIN + HEADER_HEIGHT + GAP + 88.0 + 48.0;
    assert!(recording_bottom <= 410.0);
    let cancellation_bottom = MARGIN + HEADER_HEIGHT + GAP + 142.0 + 28.0;
    assert!(cancellation_bottom <= 410.0);
    assert!(
        document_height() > 410.0,
        "secondary details scroll rather than compress"
    );
}

#[test]
fn home_navigation_is_a_route_not_a_privacy_setting_change() {
    let source = include_str!("dictate.rs");
    let handler = source
        .split("fn open_home_link(")
        .nth(1)
        .unwrap()
        .split("/// The same entry point")
        .next()
        .unwrap();
    assert!(handler.contains("RecordingState::Idle"));
    assert!(handler.contains("EngineEvent::Navigate"));
    assert!(!handler.contains("settings.set"));
    assert!(!handler.contains("hotkey_pressed"));
}
