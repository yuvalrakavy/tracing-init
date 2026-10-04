//! The worker's decisions that need no device: the outage conflict rule, and the outage episode.

use serde_json::json;

use super::*;
use crate::test_support::Capture;

fn request(value: Value, conflict: Conflict) -> StateValue {
    StateValue { target: "u1".into(), property: "temp".into(), value, deadline: None, conflict, check: Default::default() }
}

/// The outage conflict rule (Store kit spec §4.5), every row: unchanged → applied; changed and
/// `DeviceWins` → superseded; changed and `RequestWins` → applied; no "before" value → applied;
/// the device already at the requested value → nothing to write; the device's value unknown →
/// applied under `RequestWins`, and under `DeviceWins` not applied on a guess (review M6).
#[test]
fn the_outage_conflict_rule() {
    let (b, c) = (json!(20), json!(18));
    for conflict in [Conflict::DeviceWins, Conflict::RequestWins] {
        assert_eq!(resolve(&request(json!(22), conflict), Some(&b), Some(&b)), Resolution::Apply, "unchanged, {conflict:?}");
        assert_eq!(resolve(&request(json!(22), conflict), Some(&c), None), Resolution::Apply, "no before value, {conflict:?}");
        assert_eq!(resolve(&request(json!(18), conflict), Some(&c), Some(&b)), Resolution::Satisfied, "already there, {conflict:?}");
    }
    assert_eq!(resolve(&request(json!(22), Conflict::RequestWins), None, Some(&b)), Resolution::Apply, "current unknown, RequestWins");
    assert_eq!(resolve(&request(json!(22), Conflict::DeviceWins), None, Some(&b)), Resolution::Unknown, "current unknown, DeviceWins");
    assert_eq!(
        resolve(&request(json!(22), Conflict::DeviceWins), None, None),
        Resolution::Unknown,
        "current unknown, no before, DeviceWins"
    );
    assert_eq!(resolve(&request(json!(22), Conflict::DeviceWins), Some(&c), Some(&b)), Resolution::Superseded);
    assert_eq!(resolve(&request(json!(22), Conflict::RequestWins), Some(&c), Some(&b)), Resolution::Apply);
}

/// A device outage is one episode: INFO `device_connection_lost` on its first failure, DEBUG
/// after, one WARN `device_unreachable` past its threshold, INFO `device_recovered` with its
/// counts.
#[test]
fn a_device_outage_is_one_episode() {
    let log = Capture::start();
    let mut outage = DeviceOutage::new(Duration::from_millis(50));
    let error = LinkError::new("refused");
    outage.failed(&error);
    outage.failed(&error);
    std::thread::sleep(Duration::from_millis(60));
    outage.failed(&error);
    outage.failed(&error);
    outage.recovered(2, 1, 0);
    let kinds: Vec<(tracing::Level, String)> =
        log.records().into_iter().filter(|r| !r.kind.is_empty()).map(|r| (r.level, r.kind)).collect();
    assert_eq!(
        kinds,
        vec![
            (tracing::Level::INFO, "device_connection_lost".to_owned()),
            (tracing::Level::WARN, "device_unreachable".to_owned()),
            (tracing::Level::INFO, "device_recovered".to_owned()),
        ]
    );
    let recovered = log.of_kind("device_recovered");
    assert_eq!(
        (recovered[0].field("attempts"), recovered[0].field("applied"), recovered[0].field("refused")),
        (Some("4"), Some("2"), Some("1"))
    );
}

/// An operation that has completed wins over a link loss noticed at the same moment: its result is
/// what the device did, and the loss is seen at the worker's next step. Raced unbiased, a helper's
/// `link_lost` turned a completed `Ok` into a `Link` error half the time (the mqtt_ac v2 build).
#[tokio::test]
async fn an_operation_that_completed_wins_over_a_link_lost_at_the_same_moment() {
    let ctx = LinkContext::new(1, "i", Reports::new(), Arc::new(crate::link::Abandoned::default()), Arc::new(OpClock::new()));
    let handle = ctx.handle();
    handle.link_lost("lost meanwhile");
    for _ in 0..64 {
        let result = op(&handle, Duration::from_secs(5), "apply", async { Ok::<u32, OpError>(7) }).await;
        assert_eq!(result, Ok(7), "a completed operation was turned into a lost link");
    }
    assert!(handle.is_lost(), "the loss itself was forgotten");
}
