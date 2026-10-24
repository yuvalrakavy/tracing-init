//! Existing consumers can construct and destructure report snapshots without runtime metadata.
use mqtt_bridge_kit::reports::{Changes, Model};

#[test]
fn report_snapshot_shapes_remain_source_compatible() {
    let changes = Changes { answer_errors: vec![], replies: vec![], status: None, states: vec![], errors: vec![], events: vec![] };
    let Changes { answer_errors, replies, status, states, errors, events } = changes;
    assert!(answer_errors.is_empty() && replies.is_empty() && states.is_empty());
    assert!(status.is_none() && errors.is_empty() && events.is_empty());
    let model = Model { status: None, states: vec![] };
    let Model { status, states } = model;
    assert!(status.is_none() && states.is_empty());
}
