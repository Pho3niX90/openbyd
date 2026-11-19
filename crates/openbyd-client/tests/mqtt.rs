//! The MQTT runtime against the test broker of `openbyd_client::testing::broker`,
//! a small MQTT v5 broker running in this process over TLS on 127.0.0.1 with
//! a certificate made for the test.

#![cfg(feature = "testing")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use openbyd_client::mqtt::{
    EventFilter, MqttBootstrap, MqttNotice, MqttRuntime, MqttRuntimeOptions, MqttStartError,
    WaitError, client_config, rustls,
};
use openbyd_client::testing::broker::{Seen, TestBroker as Broker, WAIT, runtime_options, sealed};
use openbyd_protocol::aes::aes_encrypt_hex;
use rumqttc::v5::mqttbytes::QoS;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

const KEY: &str = "00112233445566778899AABBCCDDEEFF";
const OTHER_KEY: &str = "FFEEDDCCBBAA99887766554433221100";
const VIN: &str = "TESTVIN0000000001";

/// Options for tests: the test CA, and quick reconnects.
fn options() -> MqttRuntimeOptions {
    runtime_options()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn bootstrap(broker: &Broker, user: &str) -> MqttBootstrap {
    MqttBootstrap {
        user_id: user.to_owned(),
        broker_host: "127.0.0.1".to_owned(),
        broker_port: i64::from(broker.port()),
        topic: format!("oversea/res/{user}"),
        client_id: format!("oversea_{user}"),
        username: user.to_owned(),
        password: format!("1700000000secret{user}"),
    }
}

fn vehicle_info(marker: i64) -> Value {
    json!({"event": "vehicleInfo", "vin": VIN, "data": {"respondData": {"marker": marker}}})
}

/// The next notice, failing the test after a while.
async fn next(notices: &mut broadcast::Receiver<MqttNotice>) -> MqttNotice {
    tokio::time::timeout(WAIT, notices.recv())
        .await
        .expect("a notice in time")
        .expect("an open channel")
}

/// The next notice that is not a reconnection detail (`Disconnected`).
async fn next_kept(notices: &mut broadcast::Receiver<MqttNotice>) -> MqttNotice {
    loop {
        match next(notices).await {
            MqttNotice::Disconnected { .. } => {}
            notice => return notice,
        }
    }
}

async fn expect_connected(notices: &mut broadcast::Receiver<MqttNotice>) {
    match next_kept(notices).await {
        MqttNotice::Connected => {}
        other => panic!("expected Connected, got {other:?}"),
    }
}

/// Publishes `body` sealed with `key` and returns the next notice.
async fn exchange(
    broker: &Broker,
    topic: &str,
    notices: &mut broadcast::Receiver<MqttNotice>,
    body: &Value,
    key: &str,
) -> MqttNotice {
    assert_eq!(broker.publish(topic, sealed(body, key)), 1);
    next_kept(notices).await
}

fn event_marker(notice: &MqttNotice) -> i64 {
    match notice {
        MqttNotice::Event(event) => event.payload["data"]["respondData"]["marker"]
            .as_i64()
            .expect("marker"),
        other => panic!("expected an event, got {other:?}"),
    }
}

/// A runtime connected and subscribed to `user`'s topic, with a notice receiver.
async fn connected(broker: &Broker, user: &str) -> (MqttRuntime, broadcast::Receiver<MqttNotice>) {
    let runtime = MqttRuntime::new(KEY, options());
    let mut notices = runtime.subscribe();
    runtime
        .start(&bootstrap(broker, user))
        .await
        .expect("starts");
    expect_connected(&mut notices).await;
    let topic = format!("oversea/res/{user}");
    let deadline = Instant::now() + WAIT;
    while broker.subscribers(&topic) == 0 {
        assert!(Instant::now() < deadline, "no subscription for {topic}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    (runtime, notices)
}

fn alive_tasks() -> usize {
    tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks()
}

async fn until_tasks(expected: usize) {
    let deadline = Instant::now() + WAIT;
    while alive_tasks() != expected {
        assert!(
            Instant::now() < deadline,
            "expected {expected} tasks, {} alive",
            alive_tasks()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connects_with_the_bootstrap_and_subscribes() {
    let broker = Broker::start().await;
    let (runtime, _notices) = connected(&broker, "1001").await;
    let status = runtime.status();
    assert!(status.running && status.connected);
    assert_eq!(status.consecutive_refusals, 0);
    assert!(!status.key_confirmed);
    let seen = broker.seen();
    assert_eq!(
        seen[0],
        Seen::Connect {
            conn: 0,
            client_id: "oversea_1001".into(),
            username: Some("1001".into()),
            password: Some("1700000000secret1001".into()),
            keep_alive: 120,
            clean_start: true,
            max_packet_size: Some(1024 * 1024),
        }
    );
    assert_eq!(
        seen[1],
        Seen::Subscribe {
            conn: 0,
            topic: "oversea/res/1001".into(),
            qos: QoS::AtMostOnce
        }
    );
    assert!(format!("{runtime:?}").contains("connected: true"));
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delivers_events_with_their_fields() {
    let broker = Broker::start().await;
    let (runtime, mut notices) = connected(&broker, "1002").await;
    let topic = "oversea/res/1002";
    let body = json!({"event": "vehicleInfo", "vin": VIN, "data": {"respondData": {"soc": 80}}});
    match exchange(&broker, topic, &mut notices, &body, KEY).await {
        MqttNotice::Event(event) => {
            assert_eq!(event.event, "vehicleInfo");
            assert_eq!(event.vin.as_deref(), Some(VIN));
            assert_eq!(event.topic, topic);
            assert_eq!(Value::Object(event.payload.clone()), body);
        }
        other => panic!("expected an event, got {other:?}"),
    }
    // pyBYD's `str(payload.get("event") or "")` and the VIN rule.
    let odd = json!({"event": 5, "vin": ""});
    match exchange(&broker, topic, &mut notices, &odd, KEY).await {
        MqttNotice::Event(event) => {
            assert_eq!(event.event, "5");
            assert_eq!(event.vin, None);
        }
        other => panic!("expected an event, got {other:?}"),
    }
    // Whitespace around the hex text is ignored.
    let mut spaced = b" \n".to_vec();
    spaced.extend(sealed(&vehicle_info(3), KEY));
    assert_eq!(broker.publish(topic, spaced), 1);
    assert_eq!(event_marker(&next_kept(&mut notices).await), 3);
    assert!(runtime.status().key_confirmed);
    runtime.stop().await;
}

/// pyBYD's `test_failure_before_any_decrypt_requests_reauth`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decrypt_failure_before_any_decrypt_requests_reauth() {
    let broker = Broker::start().await;
    let (runtime, mut notices) = connected(&broker, "1003").await;
    let topic = "oversea/res/1003";
    let notice = exchange(&broker, topic, &mut notices, &vehicle_info(1), OTHER_KEY).await;
    assert!(matches!(notice, MqttNotice::DecryptFailed), "{notice:?}");
    // It keeps asking while the key is unconfirmed.
    let notice = exchange(&broker, topic, &mut notices, &vehicle_info(2), OTHER_KEY).await;
    assert!(matches!(notice, MqttNotice::DecryptFailed), "{notice:?}");
    runtime.stop().await;
}

/// pyBYD's `test_failure_after_decrypt_is_ignored`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decrypt_failure_after_a_decrypt_is_ignored() {
    let broker = Broker::start().await;
    let (runtime, mut notices) = connected(&broker, "1004").await;
    let topic = "oversea/res/1004";
    let first = exchange(&broker, topic, &mut notices, &vehicle_info(1), KEY).await;
    assert_eq!(event_marker(&first), 1);
    // Another session's message: nothing, so the next notice is the next event.
    assert_eq!(
        broker.publish(topic, sealed(&vehicle_info(2), OTHER_KEY)),
        1
    );
    let next_event = exchange(&broker, topic, &mut notices, &vehicle_info(3), KEY).await;
    assert_eq!(event_marker(&next_event), 3);
    runtime.stop().await;
}

/// pyBYD's `test_new_key_must_be_confirmed_again`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_key_must_be_confirmed_again() {
    let broker = Broker::start().await;
    let (runtime, mut notices) = connected(&broker, "1005").await;
    let topic = "oversea/res/1005";
    event_marker(&exchange(&broker, topic, &mut notices, &vehicle_info(1), KEY).await);
    runtime.update_decrypt_key(OTHER_KEY);
    assert!(!runtime.status().key_confirmed);
    let notice = exchange(&broker, topic, &mut notices, &vehicle_info(2), KEY).await;
    assert!(matches!(notice, MqttNotice::DecryptFailed), "{notice:?}");
    // The new key works.
    let notice = exchange(&broker, topic, &mut notices, &vehicle_info(3), OTHER_KEY).await;
    assert_eq!(event_marker(&notice), 3);
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_payloads_are_ignored() {
    let broker = Broker::start().await;
    let (runtime, mut notices) = connected(&broker, "1006").await;
    let topic = "oversea/res/1006";
    for payload in [
        aes_encrypt_hex("[1, 2]", KEY).unwrap().into_bytes(),
        aes_encrypt_hex("not json", KEY).unwrap().into_bytes(),
        Vec::new(),
    ] {
        assert_eq!(broker.publish(topic, payload), 1);
    }
    // None of them confirmed the key, so a foreign message still asks for a login.
    let notice = exchange(&broker, topic, &mut notices, &vehicle_info(1), OTHER_KEY).await;
    assert!(matches!(notice, MqttNotice::DecryptFailed), "{notice:?}");
    assert!(!runtime.status().key_confirmed);
    assert!(runtime.status().connected);
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refused_connacks_are_counted_and_reset() {
    let broker = Broker::start().await;
    broker.refuse_next(3);
    let runtime = MqttRuntime::new(KEY, options());
    let mut notices = runtime.subscribe();
    // A refusal is not a start failure: the runtime keeps trying.
    runtime
        .start(&bootstrap(&broker, "1007"))
        .await
        .expect("starts");
    for expected in 1..=3 {
        match next_kept(&mut notices).await {
            MqttNotice::ConnackRefused {
                consecutive,
                reason,
            } => {
                assert_eq!(consecutive, expected);
                assert!(reason.contains("NotAuthorized"), "{reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
    expect_connected(&mut notices).await;
    assert_eq!(runtime.status().consecutive_refusals, 0);
    // Clean start on the first CONNECT only, as paho's MQTT_CLEAN_START_FIRST_ONLY.
    broker
        .until("a subscription", |seen| {
            seen.iter()
                .any(|seen| matches!(seen, Seen::Subscribe { .. }))
        })
        .await;
    let clean: Vec<bool> = broker
        .seen()
        .iter()
        .filter_map(|seen| match seen {
            Seen::Connect { clean_start, .. } => Some(*clean_start),
            _ => None,
        })
        .collect();
    assert_eq!(clean, [true, false, false, false]);

    // A later refusal counts from 1 again.
    broker.refuse_next(1);
    broker.drop_all();
    match next_kept(&mut notices).await {
        MqttNotice::ConnackRefused { consecutive, .. } => assert_eq!(consecutive, 1),
        other => panic!("expected a refusal, got {other:?}"),
    }
    expect_connected(&mut notices).await;
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnects_and_resubscribes_after_a_broker_restart() {
    let broker = Broker::start().await;
    let (runtime, mut notices) = connected(&broker, "1008").await;
    let topic = "oversea/res/1008";
    broker.restart().await;
    match next(&mut notices).await {
        MqttNotice::Disconnected { reason } => assert!(!reason.is_empty()),
        other => panic!("expected Disconnected, got {other:?}"),
    }
    expect_connected(&mut notices).await;
    broker
        .until("a second subscription", |seen| {
            seen.iter()
                .filter(|seen| matches!(seen, Seen::Subscribe { .. }))
                .count()
                == 2
        })
        .await;
    while broker.subscribers(topic) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let notice = exchange(&broker, topic, &mut notices, &vehicle_info(7), KEY).await;
    assert_eq!(event_marker(&notice), 7);
    // Same credentials on the reconnection.
    let connects: Vec<Seen> = broker
        .seen()
        .into_iter()
        .filter(|seen| matches!(seen, Seen::Connect { .. }))
        .collect();
    assert_eq!(connects.len(), 2);
    match (&connects[0], &connects[1]) {
        (
            Seen::Connect {
                client_id: a,
                password: pa,
                ..
            },
            Seen::Connect {
                client_id: b,
                password: pb,
                ..
            },
        ) => assert!(a == b && pa == pb),
        _ => unreachable!(),
    }
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_disconnects_and_releases_the_socket() {
    let broker = Broker::start().await;
    let baseline = alive_tasks();
    let (runtime, _notices) = connected(&broker, "1009").await;
    assert_eq!(broker.open(), 1);
    runtime.stop().await;
    assert!(!runtime.is_running());
    assert!(!runtime.status().connected);
    broker
        .until("DISCONNECT and close", |seen| {
            seen.contains(&Seen::Disconnect { conn: 0 }) && seen.contains(&Seen::Closed { conn: 0 })
        })
        .await;
    assert_eq!(broker.open(), 0);
    // Only the broker's own tasks are left.
    until_tasks(baseline).await;
    // Stopping twice is harmless.
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn starting_twice_replaces_the_connection() {
    let broker = Broker::start().await;
    let baseline = alive_tasks();
    let (runtime, mut notices) = connected(&broker, "1010").await;
    let topic = "oversea/res/1010";
    event_marker(&exchange(&broker, topic, &mut notices, &vehicle_info(1), KEY).await);
    assert!(runtime.status().key_confirmed);
    runtime
        .start(&bootstrap(&broker, "1010"))
        .await
        .expect("restarts");
    // start forgets that the key was confirmed, as pyBYD's does.
    assert!(!runtime.status().key_confirmed);
    expect_connected(&mut notices).await;
    broker
        .until("the first connection to end cleanly", |seen| {
            seen.contains(&Seen::Disconnect { conn: 0 }) && seen.contains(&Seen::Closed { conn: 0 })
        })
        .await;
    assert_eq!(broker.open(), 1);
    // One connection task (and the broker's one for it).
    until_tasks(baseline + 2).await;
    while broker.subscribers(topic) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let notice = exchange(&broker, topic, &mut notices, &vehicle_info(2), KEY).await;
    assert_eq!(event_marker(&notice), 2);
    runtime.stop().await;
    until_tasks(baseline).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waits_for_a_specific_event() {
    let broker = Broker::start().await;
    let (runtime, mut notices) = connected(&broker, "1011").await;
    let runtime = Arc::new(runtime);
    let topic = "oversea/res/1011";

    // Registered before the request, matched by VIN, event and serial.
    let waiter = runtime.expect(
        EventFilter::new(VIN)
            .event("remoteControl")
            .matching(|event| event.payload["data"]["uuid"] == "SERIAL-2"),
    );
    let reply = |serial: &str, vin: &str, event: &str| {
        sealed(
            &json!({"event": event, "vin": vin, "data": {"uuid": serial}}),
            KEY,
        )
    };
    broker.publish(
        topic,
        reply("SERIAL-2", "OTHERVIN000000001", "remoteControl"),
    );
    broker.publish(topic, reply("SERIAL-2", VIN, "vehicleInfo"));
    broker.publish(topic, reply("SERIAL-1", VIN, "remoteControl"));
    broker.publish(topic, reply("SERIAL-2", VIN, "remoteControl"));
    let event = waiter.wait(WAIT).await.expect("the matching event");
    assert_eq!(event.payload["data"]["uuid"], "SERIAL-2");
    assert_eq!(event.vin.as_deref(), Some(VIN));
    // All four still reached the notice receiver.
    for _ in 0..4 {
        assert!(matches!(
            next_kept(&mut notices).await,
            MqttNotice::Event(_)
        ));
    }

    // wait_for_event registers and waits in one call.
    let waiting = {
        let runtime = Arc::clone(&runtime);
        tokio::spawn(async move { runtime.wait_for_event(VIN, "vehicleInfo", WAIT).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    broker.publish(topic, sealed(&vehicle_info(9), KEY));
    let event = waiting.await.unwrap().expect("vehicleInfo");
    assert_eq!(event.event, "vehicleInfo");

    // Timeouts, a panicking test, and a stop while waiting.
    assert_eq!(
        runtime
            .wait_for_event(VIN, "never", Duration::from_millis(50))
            .await
            .unwrap_err(),
        WaitError::Timeout
    );
    let panicking = runtime.expect(EventFilter::new(VIN).matching(|_| panic!("filter bug")));
    broker.publish(topic, sealed(&vehicle_info(10), KEY));
    assert_eq!(panicking.wait(WAIT).await.unwrap_err(), WaitError::Closed);
    // The runtime carries on (the notices of markers 9 and 10 come first).
    broker.publish(topic, sealed(&vehicle_info(11), KEY));
    while event_marker(&next_kept(&mut notices).await) != 11 {}

    let dropped = runtime.expect(EventFilter::new(VIN));
    drop(dropped);
    let pending = runtime.expect(EventFilter::new(VIN).event("never"));
    runtime.stop().await;
    assert_eq!(pending.wait(WAIT).await.unwrap_err(), WaitError::Closed);
    assert_eq!(
        runtime
            .wait_for(EventFilter::new(VIN), WAIT)
            .await
            .unwrap_err(),
        WaitError::NotRunning
    );
    assert!(format!("{:?}", runtime.expect(EventFilter::new(VIN))).contains("EventWaiter"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_or_panicking_consumers_do_not_hold_up_the_connection() {
    let broker = Broker::start().await;
    let mut small = options();
    small.notice_capacity = 2;
    let runtime = MqttRuntime::new(KEY, small);
    let mut slow = runtime.subscribe();
    let mut panicking = runtime.subscribe();
    let crashed = tokio::spawn(async move {
        let _ = panicking.recv().await;
        panic!("consumer bug");
    });
    runtime
        .start(&bootstrap(&broker, "1012"))
        .await
        .expect("starts");
    assert!(crashed.await.is_err());
    let topic = "oversea/res/1012";
    while broker.subscribers(topic) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Nobody reads `slow` while ten events arrive; the waits still get theirs.
    for marker in 0..10 {
        let waiter = runtime.expect(EventFilter::new(VIN));
        broker.publish(topic, sealed(&vehicle_info(marker), KEY));
        let event = waiter.wait(WAIT).await.expect("event");
        assert_eq!(event.payload["data"]["respondData"]["marker"], marker);
    }
    match slow.recv().await {
        Err(broadcast::error::RecvError::Lagged(missed)) => assert!(missed >= 7, "{missed}"),
        other => panic!("expected lag, got {other:?}"),
    }
    // The newest notices are still there, in order. A wait can be answered
    // just before the matching notice is broadcast, so the last notice may
    // still be on its way when the loop above ends: read on until it arrives.
    let mut last = event_marker(&slow.recv().await.unwrap());
    assert!(last >= 7, "{last}");
    while last < 9 {
        let next = event_marker(&slow.recv().await.unwrap());
        assert_eq!(next, last + 1);
        last = next;
    }
    assert!(runtime.status().connected);
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_fails_when_the_broker_cannot_be_reached() {
    let broker = Broker::start().await;
    let runtime = MqttRuntime::new(KEY, options());

    let mut bad_port = bootstrap(&broker, "1013");
    bad_port.broker_port = 70_000;
    assert_eq!(
        runtime.start(&bad_port).await.unwrap_err(),
        MqttStartError::InvalidPort(70_000)
    );

    let mut short = options();
    short.keepalive = 4;
    assert_eq!(
        MqttRuntime::new(KEY, short)
            .start(&bootstrap(&broker, "1013"))
            .await
            .unwrap_err(),
        MqttStartError::InvalidKeepalive(4)
    );

    // Nothing listening.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut nowhere = bootstrap(&broker, "1013");
    nowhere.broker_port = i64::from(closed.local_addr().unwrap().port());
    drop(closed);
    assert!(matches!(
        runtime.start(&nowhere).await.unwrap_err(),
        MqttStartError::Connect(_)
    ));
    assert!(!runtime.is_running());

    // A broker whose certificate is not trusted.
    let mut untrusting = options();
    let other_ca = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(other_ca.cert.der().clone()).unwrap();
    untrusting.tls = Some(client_config(roots).unwrap());
    let runtime = MqttRuntime::new(KEY, untrusting);
    let error = runtime
        .start(&bootstrap(&broker, "1013"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("connect failed"), "{error}");
    assert!(!runtime.is_running());
    assert_eq!(
        runtime
            .wait_for_event(VIN, "x", Duration::from_millis(1))
            .await
            .unwrap_err(),
        WaitError::NotRunning
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_platform_roots_are_used_by_default() {
    // No broker trusts a platform root here, so this only shows that the
    // default configuration loads and reaches the TLS handshake.
    let broker = Broker::start().await;
    let mut defaults = options();
    defaults.tls = None;
    let runtime = MqttRuntime::new(KEY, defaults);
    match runtime.start(&bootstrap(&broker, "1014")).await {
        Err(MqttStartError::Connect(message)) => {
            assert!(
                message.contains("TLS") || message.contains("certificate"),
                "{message}"
            );
        }
        // A machine without any root certificates.
        Err(MqttStartError::Certificates(_)) => {}
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_a_running_runtime_closes_its_socket() {
    let broker = Broker::start().await;
    let baseline = alive_tasks();
    let (runtime, _notices) = connected(&broker, "1015").await;
    let waiter = runtime.expect(EventFilter::new(VIN));
    drop(runtime);
    assert_eq!(waiter.wait(WAIT).await.unwrap_err(), WaitError::Closed);
    broker
        .until("the socket to close", |seen| {
            seen.contains(&Seen::Closed { conn: 0 })
        })
        .await;
    assert!(!broker.seen().contains(&Seen::Disconnect { conn: 0 }));
    until_tasks(baseline).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pings_at_the_keepalive_interval() {
    let broker = Broker::start().await;
    let mut quick = options();
    quick.keepalive = 5;
    let runtime = MqttRuntime::new(KEY, quick);
    runtime
        .start(&bootstrap(&broker, "1016"))
        .await
        .expect("starts");
    broker
        .until("keep_alive 5", |seen| {
            seen.iter()
                .any(|seen| matches!(seen, Seen::Connect { keep_alive: 5, .. }))
        })
        .await;
    let started = Instant::now();
    broker
        .until("a ping", |seen| seen.contains(&Seen::PingReq { conn: 0 }))
        .await;
    assert!(started.elapsed() >= Duration::from_secs(4));
    assert!(runtime.status().connected);
    runtime.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn five_hundred_runtimes_in_one_process() {
    const COUNT: usize = 500;
    let broker = Broker::start().await;
    let baseline_tasks = alive_tasks();
    let baseline_memory = memory_stats::memory_stats().map(|stats| stats.physical_mem);

    let mut starting = Vec::new();
    for index in 0..COUNT {
        let broker = Arc::clone(&broker);
        starting.push(tokio::spawn(async move {
            let user = format!("u{index}");
            let runtime = MqttRuntime::new(KEY, options());
            let notices = runtime.subscribe();
            runtime
                .start(&bootstrap(&broker, &user))
                .await
                .expect("starts");
            (runtime, notices)
        }));
    }
    let mut runtimes = Vec::new();
    for task in starting {
        runtimes.push(task.await.unwrap());
    }
    broker
        .until("every subscription", |seen| {
            seen.iter()
                .filter(|seen| matches!(seen, Seen::Subscribe { .. }))
                .count()
                == COUNT
        })
        .await;
    while (0..COUNT).any(|index| broker.subscribers(&format!("oversea/res/u{index}")) == 0) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // One task per runtime, plus the broker's one per connection.
    until_tasks(baseline_tasks + 2 * COUNT).await;
    let memory = memory_stats::memory_stats().map(|stats| stats.physical_mem);
    if let (Some(before), Some(after)) = (baseline_memory, memory) {
        println!(
            "{COUNT} runtimes: {} tasks, {} KiB resident in total ({} KiB per runtime, broker side included)",
            alive_tasks() - baseline_tasks,
            after.saturating_sub(before) / 1024,
            after.saturating_sub(before) / 1024 / COUNT
        );
    }

    for (index, (runtime, notices)) in runtimes.iter_mut().enumerate() {
        let topic = format!("oversea/res/u{index}");
        let marker = i64::try_from(index).unwrap();
        assert_eq!(
            broker.publish(&topic, sealed(&vehicle_info(marker), KEY)),
            1
        );
        let mut notice = next_kept(notices).await;
        if matches!(notice, MqttNotice::Connected) {
            notice = next_kept(notices).await;
        }
        assert_eq!(event_marker(&notice), marker);
        match &notice {
            MqttNotice::Event(event) => assert_eq!(event.topic, topic),
            _ => unreachable!(),
        }
        assert!(runtime.is_running());
    }

    let mut stopping = Vec::new();
    for (runtime, _) in runtimes {
        stopping.push(tokio::spawn(async move { runtime.stop().await }));
    }
    for task in stopping {
        task.await.unwrap();
    }
    until_tasks(baseline_tasks).await;
    let deadline = Instant::now() + WAIT;
    while broker.open() != 0 {
        assert!(
            Instant::now() < deadline,
            "{} sockets still open",
            broker.open()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let disconnects = broker
        .seen()
        .iter()
        .filter(|seen| matches!(seen, Seen::Disconnect { .. }))
        .count();
    assert_eq!(disconnects, COUNT);
}
