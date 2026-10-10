//! The multi-device host's startup and liveness (multi-device spec §7, scenario 5; §4.6):
//! `Active=true` only after every grant, a refused subscription ends the session, and the current
//! statuses republished before `Active=true` after a reconnect. Each scenario has an outer
//! deadline and a progress assertion.

use std::sync::Arc;
use std::time::Duration;

use mqtt_bridge_conformance::host::*;
use mqtt_bridge_conformance::*;
use serde_json::json;

/// Each scenario's outer bound.
const DEADLINE: Duration = Duration::from_secs(60);

fn actives(h: &HostHarness) -> usize {
    h.published(&h.active_topic()).iter().filter(|r| &r.payload[..] == b"true").count()
}

/// A broker for several clients, capped.
async fn broker() -> Arc<FakeBroker> {
    let broker = Arc::new(FakeBroker::start_multi().await);
    broker.cap_connections(80);
    broker
}

/// `Active=true` comes only once every subscription requested is granted — an instance's requests
/// among them — and after the instance's current `Status`.
#[tokio::test(flavor = "multi_thread")]
async fn active_comes_only_after_every_grant_and_after_the_statuses() {
    within(DEADLINE, async {
        let broker = broker().await;
        let lamp = instance_name("Lamp");
        let bridge = instance_name("startup");
        broker.retain(
            &format!("{ROOT}/Config/{lamp}"),
            json!({"bridge": KIND, "via": bridge}).to_string(),
            &SendOptions::default().retained(),
        );
        broker.hold_subacks_matching(&format!("{ROOT}/Desired/{lamp}"));
        let h = HostHarness::join(broker, &bridge, Fleet::new(acknowledged()), |host| host).await;
        let t = h.topics(&lamp);
        assert!(h.eventually(BOUND, |h| h.subscribed(&t.command())).await, "the instance's requests were never subscribed");
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(actives(&h), 0, "Active=true was published with a grant pending");
        h.broker.release_subacks();
        assert!(h.eventually(BOUND, |h| actives(h) == 1).await, "Active=true never came once every grant was in");
        let labels = h.labels();
        let active = labels.iter().position(|l| *l == format!("pub {} true", h.active_topic())).expect("Active=true");
        let status =
            labels.iter().position(|l| l.starts_with(&format!("pub {} ", t.status()))).expect("the instance's Status was never published");
        assert!(status < active, "the instance's Status came after Active=true: {labels:#?}");
        let version = labels.iter().position(|l| l.starts_with(&format!("pub {ROOT}/Version/{} ", h.bridge))).expect("no Version");
        assert!(labels[version].ends_with(&format!("{KIND} conformance")), "the Version is not `<kind> <version>`: {}", labels[version]);
        assert!(h.stop().await);
    })
    .await
}

/// A refused `Config/+` subscription ends the session: the host reconnects, WARN
/// `subscription_refused`, and never says `Active=true`.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_config_subscription_ends_the_session() {
    within(DEADLINE, async {
        let broker = broker().await;
        broker.deny_subscriptions(&format!("{ROOT}/Config/+"));
        let h = HostHarness::join_unwaited(broker, &instance_name("refused"), Fleet::new(acknowledged()), |host| host);
        assert!(h.eventually(BOUND, |h| h.broker.connections() >= 3).await, "the session did not end on a refused Config/+");
        assert_eq!(actives(&h), 0, "Active=true was published without the Config/+ grant");
        assert!(!h.log.of_kind("subscription_refused").is_empty(), "{:#?}", h.log.records());
        assert!(h.stop().await);
    })
    .await
}

/// A refused instance subscription ends the session too.
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_instance_subscription_ends_the_session() {
    within(DEADLINE, async {
        let broker = broker().await;
        let lamp = instance_name("Lamp");
        let bridge = instance_name("refused-instance");
        broker.retain(
            &format!("{ROOT}/Config/{lamp}"),
            json!({"bridge": KIND, "via": bridge}).to_string(),
            &SendOptions::default().retained(),
        );
        broker.deny_subscriptions(&format!("{ROOT}/Command/{lamp}"));
        let h = HostHarness::join(broker, &bridge, Fleet::new(acknowledged()), |host| host).await;
        assert!(h.eventually(BOUND, |h| h.broker.connections() >= 3).await, "the session did not end on a refused instance subscription");
        assert_eq!(actives(&h), 0, "Active=true was published with an instance subscription refused");
        assert!(!h.log.of_kind("subscription_refused").is_empty(), "{:#?}", h.log.records());
        assert!(h.stop().await);
    })
    .await
}

/// An overdue SUBACK (10 s) ends the session — time alone never establishes a grant — and the next
/// session bootstraps again.
#[tokio::test(flavor = "multi_thread")]
async fn an_overdue_subscription_ends_the_session() {
    within(DEADLINE, async {
        let broker = broker().await;
        let lamp = instance_name("Lamp");
        let bridge = instance_name("overdue");
        broker.retain(
            &format!("{ROOT}/Config/{lamp}"),
            json!({"bridge": KIND, "via": bridge}).to_string(),
            &SendOptions::default().retained(),
        );
        // The instance's requests' SUBACK never comes on this connection (a new one answers again).
        broker.hold_subacks_matching(&format!("{ROOT}/Desired/{lamp}"));
        // A keep-alive long enough that the pump's own stall bound (twice it) cannot be what ends
        // the session first: only the lifecycle's grant bound can, within the 15 s below.
        let slow_pings = mqtt_bridge_kit::MqttTiming { keep_alive: Duration::from_secs(30), ..quick_mqtt() };
        let h = HostHarness::join(broker, &bridge, Fleet::new(acknowledged()), |host| host.mqtt_timing(slow_pings)).await;
        let started = std::time::Instant::now();
        assert!(h.eventually(BOUND, |h| h.subscribed(&h.topics(&lamp).command())).await, "S3 was never asked for");
        assert!(h.eventually(Duration::from_secs(15), |h| h.broker.connections() >= 2).await, "an overdue SUBACK did not end the session");
        let took = started.elapsed();
        assert!(took >= Duration::from_secs(9), "the session ended before the grant was overdue: {took:?}");
        assert!(
            h.log.of_kind("subscription_refused").iter().any(|r| r.field("overdue_ms").is_some()),
            "the overdue grant was not said: {:#?}",
            h.log.of_kind("subscription_refused")
        );
        assert_eq!(actives(&h), 0, "Active=true was published with a grant pending");
        h.broker.release_subacks();
        assert!(h.eventually(BOUND, |h| actives(h) == 1).await, "the next session never completed its bootstrap");
        assert!(h.stop().await);
    })
    .await
}

/// A due timer gets its service whatever the publication does: with a device reporting without
/// pause — the rotation always owed, no request outstanding — another instance's failed
/// construction is still retried at its pacing, and serves (R2 gate, Codex 1).
#[tokio::test(flavor = "multi_thread")]
async fn a_retry_comes_due_under_a_steady_publication() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let (chatty, failing) = (instance_name("Achatty"), instance_name("Bfailing"));
        fleet.prepare(&chatty, |d| d.set("t0", "n", json!(0)));
        fleet.script(&failing, &[Build::Unusable]);
        let retry = mqtt_bridge_kit::TimingOverrides { retry: Some(Duration::from_secs(1)), ..quick_timing() };
        let h = HostHarness::join(broker().await, &instance_name("retry-starved"), fleet, |host| host.timing(retry)).await;
        assert!(h.configure(&chatty, json!({})));
        assert!(h.eventually(BOUND, |h| h.status(&chatty).as_deref() == Some("connected")).await, "the chatty device never connected");
        h.broker.delay_acks(Duration::from_millis(10));
        let device = h.fleet.device(&chatty).expect("built");
        let flood = tokio::spawn(async move {
            let mut n = 0u64;
            loop {
                for t in 0..1200 {
                    let mut values = mqtt_bridge_kit::PropertyMap::new();
                    values.insert("n".into(), json!(n));
                    device.report(&format!("t{t}"), values);
                }
                n += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(h.configure(&failing, json!({})));
        let retried = h.eventually(Duration::from_secs(8), |h| h.fleet.builds(&failing) >= 2).await;
        flood.abort();
        h.broker.delay_acks(Duration::ZERO);
        assert!(
            retried,
            "a failed construction's retry never came due while the publication was owed: builds {}",
            h.fleet.builds(&failing)
        );
        assert!(h.eventually(BOUND, |h| h.status(&failing).as_deref() == Some("connected")).await, "the retried instance never served");
        assert!(h.stop().await);
    })
    .await
}

/// A due timer gets its service whatever the publication does: with a device reporting without
/// pause — the rotation always owed — another instance's overdue SUBACK still ends the session at
/// its 10 s bound (R2 gate, Codex 1).
#[tokio::test(flavor = "multi_thread")]
async fn an_overdue_grant_ends_the_session_under_a_steady_publication() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let (chatty, quiet) = (instance_name("Achatty"), instance_name("Bquiet"));
        fleet.prepare(&chatty, |d| d.set("t0", "n", json!(0)));
        // A keep-alive long enough that the pump's own stall bound cannot end the session first.
        let slow_pings = mqtt_bridge_kit::MqttTiming { keep_alive: Duration::from_secs(30), ..quick_mqtt() };
        let h = HostHarness::join(broker().await, &instance_name("starved"), fleet, |host| host.mqtt_timing(slow_pings)).await;
        assert!(h.configure(&chatty, json!({})));
        assert!(h.eventually(BOUND, |h| h.status(&chatty).as_deref() == Some("connected")).await, "the chatty device never connected");
        h.broker.delay_acks(Duration::from_millis(10));
        let device = h.fleet.device(&chatty).expect("built");
        let flood = tokio::spawn(async move {
            let mut n = 0u64;
            loop {
                for t in 0..1200 {
                    let mut values = mqtt_bridge_kit::PropertyMap::new();
                    values.insert("n".into(), json!(n));
                    device.report(&format!("t{t}"), values);
                }
                n += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        // The quiet instance's requests' SUBACK never comes on this connection.
        h.broker.hold_subacks_matching(&format!("{ROOT}/Desired/{quiet}"));
        let connections = h.broker.connections();
        assert!(h.configure(&quiet, json!({})));
        assert!(h.eventually(BOUND, |h| h.subscribed(&h.topics(&quiet).command())).await, "S3 was never asked for");
        assert!(
            h.eventually(Duration::from_secs(16), |h| h
                .log
                .of_kind("subscription_refused")
                .iter()
                .any(|r| r.field("overdue_ms").is_some()))
                .await,
            "an overdue grant never ended the session while the publication was owed"
        );
        assert!(h.eventually(BOUND, |h| h.broker.connections() > connections).await, "the session never came again");
        flood.abort();
        h.broker.delay_acks(Duration::ZERO);
        h.broker.release_subacks();
        assert!(h.eventually(BOUND, |h| h.status(&quiet).as_deref() == Some("connected")).await, "the quiet instance never connected");
        assert!(h.stop().await);
    })
    .await
}

/// After a reconnect, every owned instance's current `Status` is published again before
/// `Active=true`.
#[tokio::test(flavor = "multi_thread")]
async fn the_statuses_are_republished_before_active_after_a_reconnect() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let (a, b) = (instance_name("Aone"), instance_name("Btwo"));
        let h = HostHarness::start("republish", fleet).await;
        assert!(h.configure(&a, json!({})) && h.configure(&b, json!({})));
        assert!(
            h.eventually(BOUND, |h| h.status(&a).as_deref() == Some("connected") && h.status(&b).as_deref() == Some("connected")).await,
            "the instances never connected"
        );
        assert!(h.eventually(BOUND, |h| actives(h) == 1).await);
        let mark = h.labels().len();
        h.broker.drop_connection();
        assert!(h.eventually(BOUND, |h| actives(h) == 2).await, "Active=true never came after the reconnect");
        let labels = h.labels()[mark..].to_vec();
        let active = labels.iter().position(|l| *l == format!("pub {} true", h.active_topic())).expect("Active=true");
        for instance in [&a, &b] {
            let status = format!("pub {} connected", h.topics(instance).status());
            let at =
                labels.iter().position(|l| *l == status).unwrap_or_else(|| panic!("{instance}'s Status was not republished: {labels:#?}"));
            assert!(at < active, "{instance}'s Status came after Active=true: {labels:#?}");
        }
        assert!(h.stop().await);
    })
    .await
}

/// A Config change whose stop spans a reconnect: the instance's current lifecycle Status —
/// `connecting`, its successor coming — is published on the new connection before `Active=true`,
/// so the old device's retained `connected` does not stand while it is replaced (§4.6; R2 gate,
/// Claude minor 4).
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_spanning_a_reconnect_has_its_status_republished_before_active() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        // The old device's stop takes a while: its disconnect blocks the worker for 3 s.
        fleet.prepare(&lamp, |d| d.answer(Op::Disconnect, None, Answer::Block(Duration::from_millis(3000))));
        let h = HostHarness::start("stop-reconnect", fleet).await;
        assert!(h.configure(&lamp, json!({"address": "a"})));
        assert!(h.eventually(BOUND, |h| h.status(&lamp).as_deref() == Some("connected")).await);
        assert!(h.eventually(BOUND, |h| actives(h) == 1).await);
        let old = h.fleet.device(&lamp).expect("built");
        assert!(h.configure(&lamp, json!({"address": "b"})));
        assert!(h.eventually(BOUND, |_| !old.calls_of(Op::Disconnect).is_empty()).await, "the change was never committed");
        let mark = h.labels().len();
        h.broker.drop_connection();
        assert!(h.eventually(BOUND, |h| actives(h) == 2).await, "Active=true never came after the reconnect");
        let labels = h.labels()[mark..].to_vec();
        let active = labels.iter().position(|l| *l == format!("pub {} true", h.active_topic())).expect("Active=true");
        let status = labels.iter().position(|l| l.starts_with(&format!("pub {} ", h.topics(&lamp).status())));
        assert!(status.is_some_and(|at| at < active), "the stopping instance's Status was not republished before Active=true: {labels:#?}");
        assert!(h.eventually(BOUND, |h| h.status(&lamp).as_deref() == Some("connected")).await, "the successor never connected");
        assert!(h.stop().await);
    })
    .await
}

/// The statuses said at a read-back's grant go out through the publication's rotation, within the
/// packet-id budget, and `Active=true` still waits for them (§4.3, §4.6): with the barrier already
/// acknowledged, a stopping instance's read-back granted last brings every grant in at once —
/// `Active=true` comes only once its `connecting` is out (the R2 confirming reviews: the Status left
/// the direct path, which had said it at once).
#[tokio::test(flavor = "multi_thread")]
async fn active_waits_for_the_statuses_said_through_the_rotation() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        fleet.prepare(&lamp, |d| d.answer(Op::Disconnect, None, Answer::Block(Duration::from_millis(3000))));
        let h = HostHarness::start("status-rotation", fleet).await;
        assert!(h.configure(&lamp, json!({"address": "a"})));
        assert!(h.eventually(BOUND, |h| h.status(&lamp).as_deref() == Some("connected")).await);
        assert!(h.eventually(BOUND, |h| actives(h) == 1).await);
        let old = h.fleet.device(&lamp).expect("built");
        assert!(h.configure(&lamp, json!({"address": "b"})));
        assert!(h.eventually(BOUND, |_| !old.calls_of(Op::Disconnect).is_empty()).await, "the change was never committed");
        // On the next connection the read-back's SUBACK comes after the barrier's.
        h.broker.hold_subacks_matching(&format!("{ROOT}/State/{lamp}"));
        let (before, mark) = (h.broker.accepted(), h.labels().len());
        h.broker.drop_connection();
        assert!(h.eventually(BOUND, |h| h.broker.accepted() > before).await, "no reconnect");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(actives(&h), 1, "Active=true came with the read-back's grant pending");
        h.broker.release_subacks();
        assert!(h.eventually(BOUND, |h| actives(h) == 2).await, "Active=true never came after the reconnect");
        let labels = h.labels()[mark..].to_vec();
        let active = labels.iter().position(|l| *l == format!("pub {} true", h.active_topic())).expect("Active=true");
        let status = labels.iter().position(|l| *l == format!("pub {} connecting", h.topics(&lamp).status()));
        assert!(status.is_some_and(|at| at < active), "Active=true went before the Status said through the rotation: {labels:#?}");
        assert!(h.stop().await);
    })
    .await
}
