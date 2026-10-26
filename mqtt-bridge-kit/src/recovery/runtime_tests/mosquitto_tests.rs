//! Explicit acceptance test, with one owned loopback broker and no persistence.
use super::*;
use rumqttc::v5::mqttbytes::v5::Packet;
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::{AsyncClient, Event, MqttOptions};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncReadExt;

struct LocalBroker {
    child: Option<tokio::process::Child>,
    directory: PathBuf,
    port: u16,
}
impl Drop for LocalBroker {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
impl LocalBroker {
    async fn start() -> Result<Self, String> {
        let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|error| error.to_string())?;
        let port = reservation.local_addr().map_err(|error| error.to_string())?.port();
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|error| error.to_string())?.as_nanos();
        let directory = std::env::temp_dir().join(format!("bridge-recovery-mosquitto-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).map_err(|error| error.to_string())?;
        let mut broker = Self { child: None, directory, port };
        let config = broker.directory.join("mosquitto.conf");
        std::fs::write(&config, format!("listener {port} 127.0.0.1\nallow_anonymous true\npersistence false\nlog_type error\n"))
            .map_err(|error| error.to_string())?;
        drop(reservation);
        broker.child = Some(
            tokio::process::Command::new("mosquitto")
                .arg("-c")
                .arg(config)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|error| format!("start isolated mosquitto: {error}"))?,
        );
        let until = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let child = broker.child.as_mut().unwrap();
            if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                let mut message = String::new();
                if let Some(stderr) = child.stderr.take() {
                    let _ = stderr.take(4096).read_to_string(&mut message).await;
                }
                return Err(format!("isolated mosquitto exited {status}: {message}"));
            }
            if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                return Ok(broker);
            }
            if std::time::Instant::now() >= until {
                return Err("isolated mosquitto did not bind its loopback listener".into());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    async fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            child.kill().await.unwrap();
        }
    }
}

async fn seed_snapshot(port: u16, document: Vec<u8>) {
    let mut options = MqttOptions::new(format!("recovery-seed-{}", std::process::id()), "127.0.0.1", port);
    options.set_keep_alive(Duration::from_secs(5));
    let (client, mut events) = AsyncClient::new(options, 16);
    client.publish("Recovery/State/i/_recovery", QoS::AtLeastOnce, true, document).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(events.poll().await.unwrap(), Event::Incoming(Packet::PubAck(_))) {
                break;
            }
        }
    })
    .await
    .expect("mosquitto did not acknowledge the seed snapshot");
    client.disconnect().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(events.poll().await.unwrap(), Event::Outgoing(rumqttc::Outgoing::Disconnect)) {
                break;
            }
        }
    })
    .await
    .expect("snapshot seed client did not disconnect");
}

async fn read_retained_snapshot(port: u16) -> Bytes {
    let mut options = MqttOptions::new(format!("recovery-reader-{}", std::process::id()), "127.0.0.1", port);
    options.set_keep_alive(Duration::from_secs(5));
    let (client, mut events) = AsyncClient::new(options, 16);
    client.subscribe("Recovery/State/i/_recovery", QoS::AtLeastOnce).await.unwrap();
    let document = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Event::Incoming(Packet::Publish(publish)) = events.poll().await.unwrap() {
                assert!(publish.retain, "snapshot observer did not receive a retained document");
                break publish.payload;
            }
        }
    })
    .await
    .expect("snapshot observer received no retained document");
    client.disconnect().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(events.poll().await.unwrap(), Event::Outgoing(rumqttc::Outgoing::Disconnect)) {
                break;
            }
        }
    })
    .await
    .expect("snapshot observer did not disconnect");
    document
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn isolated_mosquitto_orders_retained_snapshot_before_the_later_barrier_ack() {
    let mut broker = LocalBroker::start().await.expect("local Mosquitto acceptance prerequisite");
    let snapshot = crate::wire::encode_state(&serde_json::json!({"snapshot":"selected-real-broker"}).as_object().unwrap().clone(), false);
    seed_snapshot(broker.port, snapshot.clone()).await;
    let (send, receive) = oneshot::channel();
    let running = Bridge::new("Recovery")
        .instance("i")
        .broker(&format!("127.0.0.1:{}", broker.port))
        .recovery(RecoveryOptions { target: "_recovery".into(), max_bytes: 4096 })
        .buffers(BufferLimits { incoming_bytes: 1024 * 1024, reply_bytes: 1024 * 1024 })
        .start_managed(Owner { calls: Calls::default(), handle: send })
        .await
        .unwrap();
    let mut handle = receive.await.unwrap();
    let readback = tokio::time::timeout(Duration::from_secs(3), handle.readback()).await.unwrap().unwrap();
    assert_eq!(
        readback.document.unwrap(),
        Some(Bytes::from(snapshot)),
        "deployment broker's replay did not precede the barrier acknowledgment"
    );
    let selected_snapshot = super::snapshot(8);
    let mut selected = image(Some(11));
    selected.snapshot = Some(selected_snapshot.clone());
    tokio::time::timeout(Duration::from_secs(3), handle.complete(readback.connection, selected, RecoveryReady::Ready, None))
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(receipt) = handle.receipt() {
                assert_eq!(receipt.snapshot_id, selected_snapshot.id());
                assert_eq!(receipt.revision, 8);
                assert_eq!(receipt.connection, readback.connection);
                assert!(receipt.packet.is_some_and(|packet| packet != 0));
                receipt.result.unwrap();
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("real broker acceptance did not produce an exact snapshot receipt");
    assert!(running.stop().await);
    assert_eq!(
        read_retained_snapshot(broker.port).await,
        *selected_snapshot.document(),
        "new subscribers did not receive the accepted snapshot"
    );
    broker.stop().await;
}
