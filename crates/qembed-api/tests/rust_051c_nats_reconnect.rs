use std::{
    net::{SocketAddr, TcpStream},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use async_nats::jetstream::{self, stream::Config as StreamConfig};
use qembed_api::adapters::nats_jetstream_publisher::NatsJetStreamPublisher;
use qembed_domain::ports::{MessagePublisher, OutboundMessage};
use uuid::Uuid;

fn compose(args: &[&str]) {
    let status = Command::new("docker")
        .args([
            "compose",
            "--project-name",
            "qembed-rust-test",
            "--file",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../../compose.test.yaml"),
        ])
        .args(args)
        .status()
        .expect("Docker Compose is available");
    assert!(status.success(), "docker compose {args:?} should succeed");
}

fn wait_for_nats(address: SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&address, Duration::from_millis(250)).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("NATS did not become available at {address}");
}

#[tokio::test]
#[ignore = "restarts the shared NATS container; invoked separately by scripts/test-postgres.sh"]
async fn existing_async_nats_client_reconnects_after_server_restart() {
    let url = std::env::var("NATS_URL").expect("NATS_URL is set by test-postgres.sh");
    let publisher = NatsJetStreamPublisher::connect(&url)
        .await
        .expect("publisher connects before outage");

    let admin = async_nats::connect(&url)
        .await
        .expect("admin client connects before outage");
    let context = jetstream::new(admin);
    let suffix = Uuid::new_v4().simple().to_string();
    let subject = format!("qembed.rust051c.reconnect.{suffix}");
    let stream = context
        .get_or_create_stream(StreamConfig {
            name: format!("QEMBED_RUST051C_RECONNECT_{}", suffix.to_uppercase()),
            subjects: vec![subject.clone()],
            duplicate_window: Duration::from_secs(120),
            max_messages: 100,
            ..Default::default()
        })
        .await
        .expect("reconnect test stream is provisioned");

    let address: SocketAddr = "127.0.0.1:4222".parse().expect("valid NATS address");
    compose(&["stop", "nats"]);
    compose(&["start", "nats"]);
    wait_for_nats(address);

    let message = OutboundMessage::new(
        Uuid::new_v4().to_string(),
        subject,
        serde_json::json!({"kind":"reconnect-probe"}),
    );
    let publish = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match publisher.publish(message.clone()).await {
                Ok(receipt) => break receipt,
                Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
    })
    .await
    .expect("existing client reconnects and receives PubAck");

    assert_eq!(publish.stream(), stream.cached_info().config.name);
    let info = stream
        .get_info()
        .await
        .expect("stream remains available after restart");
    assert_eq!(info.state.messages, 1);
}
