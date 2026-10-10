//! The multi-device host's failure domain, D4 = (a) (multi-device spec §7, scenario 4; §4.1): a
//! device whose worker is blocked, whose blocking budget stays exhausted, or whose factory does not
//! return ends the process with a failure exit — R1's dead-worker path — and the restart restores
//! every device's Status. Each scenario has an outer deadline and a progress assertion.
//!
//! Timing-sensitive (watchdog bounds): run this binary on its own.

use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mqtt_bridge_conformance::host::*;
use mqtt_bridge_conformance::*;
use serde_json::json;

/// Each scenario's outer bound.
const DEADLINE: Duration = Duration::from_secs(60);

/// The host's first dead part, within `bound`.
async fn died_within(h: &mut HostHarness, bound: Duration) -> Option<mqtt_bridge_kit::Died> {
    tokio::time::timeout(bound, h.running().ended()).await.ok()
}

/// A blocked device worker ends the process; the restart — a new process with the same bridge id
/// — publishes every device's Status again, the other device's included, so none is left stale.
#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_worker_ends_the_process_and_the_restart_restores_every_status() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let (calm, stuck) = (instance_name("Calm"), instance_name("Stuck"));
        fleet.prepare(&stuck, |d| d.answer(Op::Apply, None, Answer::Block(Duration::from_secs(8))));
        let mut h = HostHarness::start("blocked", fleet).await;
        assert!(h.configure(&calm, json!({})) && h.configure(&stuck, json!({})));
        assert!(
            h.eventually(bound(), |h| h.status(&calm).as_deref() == Some("connected") && h.status(&stuck).as_deref() == Some("connected"))
                .await,
            "the devices never connected"
        );
        assert!(h.desire(&stuck, "t1", "power", json!(true), Req::default()));
        let started = Instant::now();
        let died = died_within(&mut h, Duration::from_secs(10)).await.expect("a blocked worker did not end the process");
        assert_eq!(died.part, "device");
        assert!(died.cause.contains("blocked"), "the death's cause: {}", died.cause);
        assert!(started.elapsed() < Duration::from_secs(6), "the verdict took {:?}", started.elapsed());
        assert!(h.stop_host().await, "the host did not stop in time after the death");
        // The supervisor's restart: the same bridge id, the same broker.
        let marks: Vec<usize> = [&calm, &stuck].iter().map(|i| h.statuses(i).len()).collect();
        let fleet = Fleet::new(acknowledged());
        let restarted = HostHarness::join(h.broker.clone(), &h.bridge, fleet, |host| host).await;
        for (instance, mark) in [&calm, &stuck].into_iter().zip(marks) {
            assert!(
                restarted
                    .eventually(bound(), |r| r.statuses(instance).len() > mark
                        && r.status_via(instance).is_some_and(|(_, via)| via.as_deref() == Some(r.bridge.as_str())))
                    .await,
                "the restart did not publish {instance}'s Status again: {:?}",
                restarted.statuses(instance)
            );
        }
        assert!(
            restarted.eventually(bound(), |r| r.status(&calm).as_deref() == Some("connected")).await,
            "the other device is not served again after the restart"
        );
        assert!(restarted.stop().await);
    })
    .await
}

/// A factory that does not return within `--factory-timeout` ends the process.
#[tokio::test(flavor = "multi_thread")]
async fn a_factory_that_does_not_return_ends_the_process() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let slow = instance_name("Slow");
        fleet.script(&slow, &[Build::Block(Duration::from_secs(30))]);
        let broker = Arc::new(FakeBroker::start_multi().await);
        broker.cap_connections(80);
        let mut h = HostHarness::join(broker, &instance_name("factory"), fleet, |host| host.factory_timeout(Duration::from_secs(1))).await;
        assert!(h.configure(&slow, json!({})));
        let started = Instant::now();
        let died = died_within(&mut h, Duration::from_secs(6)).await.expect("a factory that never returned did not end the process");
        assert!(died.cause.contains("factory"), "the death's cause: {}", died.cause);
        assert!(started.elapsed() < Duration::from_secs(4), "the verdict took {:?}", started.elapsed());
        assert!(h.stop_host().await);
    })
    .await
}

/// A device whose helpers' blocking budget stays exhausted ends the process.
#[tokio::test(flavor = "multi_thread")]
async fn an_exhausted_blocking_budget_ends_the_process() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let (calm, greedy) = (instance_name("Calm"), instance_name("Greedy"));
        // Each connect leaves a synchronous call stuck past its deadline: four fill the operations' cap.
        fleet.prepare(&greedy, |d| d.stick_connects());
        let mut h = HostHarness::start("budget", fleet).await;
        assert!(h.configure(&calm, json!({})) && h.configure(&greedy, json!({})));
        let died = died_within(&mut h, Duration::from_secs(15)).await.expect("an exhausted budget did not end the process");
        assert!(died.cause.contains("cap"), "the death's cause: {}", died.cause);
        assert!(h.stop_host().await);
    })
    .await
}

/// The real process: a host whose factory never returns exits with a failure status, for its
/// supervisor to restart it.
#[tokio::test(flavor = "multi_thread")]
async fn the_host_process_exits_with_a_failure_on_a_dead_device() {
    within(DEADLINE, async {
        let broker = FakeBroker::start_multi().await;
        broker.cap_connections(20);
        let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("host-process-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the test's working directory");
        std::fs::write(
            dir.join("logging.toml"),
            "[logging]\non_destination_error = \"skip\"\n\n[logging.gelf]\naddress = \"127.0.0.1:9\"\n",
        )
        .expect("the test's logging.toml");
        broker.retain(
            "Proc/Config/slow",
            json!({"bridge": "fakekind", "via": "proc-host"}).to_string(),
            &SendOptions::default().retained(),
        );
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_fake-host"))
            .args([
                "--name",
                "proc-host",
                "--broker",
                broker.address().as_str(),
                "--root",
                "Proc",
                "--factory-timeout",
                "1s",
                "--block",
                "slow",
            ])
            .arg("--log-config")
            .arg(dir.join("logging.toml"))
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("start the host");
        let status = tokio::time::timeout(Duration::from_secs(20), child.wait()).await;
        let status = status.expect("the host did not exit on a dead device").expect("its exit status");
        assert!(!status.success(), "the host exited with success on a dead device: {status:?}");
        assert_eq!(broker.retained_on("Proc/Active/proc-host").as_deref(), Some(&b"false"[..]), "Active was not false at the end");
    })
    .await
}
