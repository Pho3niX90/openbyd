//! Idle MQTT connections: the openbyd side of `bench/mqtt_idle.py`, and the broker both sides connect to.
//!
//! Two modes, each a process of its own so that the broker's memory is not
//! counted in the client's:
//!
//! ```text
//! mqtt_idle broker
//! mqtt_idle client --port 41234 --count 1000
//! ```
//!
//! `broker` starts the TLS test broker and prints `PORT <port>`, then
//! `READY`. It answers `stats` on standard input with one JSON line (open
//! connections and subscriptions seen) and exits on `quit`.
//!
//! `client` holds `--count` [`MqttRuntime`]s on one tokio runtime
//! (`--workers` threads, 0 for tokio's default), each connected and
//! subscribed to its own topic and then idle. It prints `READY`, waits for
//! `go`, connects them all, prints `RESULT <json>` and waits for `quit`
//! while keeping the connections open, so the driver can measure it.
//!
//! The runtimes do not verify the broker's certificate, which is made for
//! the run: the pyBYD side of the benchmark switches verification off too,
//! so both pay for the same TLS handshake and neither for a chain check.

use std::io::{BufRead, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use openbyd_client::mqtt::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use openbyd_client::mqtt::rustls::crypto::{CryptoProvider, ring};
use openbyd_client::mqtt::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use openbyd_client::mqtt::rustls::{ClientConfig, DigitallySignedStruct, Error, SignatureScheme};
use openbyd_client::mqtt::{MqttBootstrap, MqttRuntime, MqttRuntimeOptions};
use openbyd_client::testing::broker::{Seen, TestBroker};
use serde_json::json;

/// Accepts any certificate. For the benchmark only.
#[derive(Debug)]
struct AcceptAnything(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnything {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        openbyd_client::mqtt::rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        openbyd_client::mqtt::rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn insecure_tls() -> Arc<ClientConfig> {
    let provider = Arc::new(ring::default_provider());
    let config = ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnything(provider)))
        .with_no_client_auth();
    Arc::new(config)
}

fn line() -> Option<String> {
    let mut text = String::new();
    match std::io::stdin().lock().read_line(&mut text) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(text.trim().to_owned()),
    }
}

fn say(text: &str) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{text}").expect("stdout");
    out.flush().expect("stdout");
}

fn broker() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let broker = runtime.block_on(TestBroker::start());
    say(&format!("PORT {}", broker.port()));
    say("READY");
    while let Some(command) = line() {
        match command.as_str() {
            "stats" => {
                let subscribed = broker
                    .seen()
                    .iter()
                    .filter(|seen| matches!(seen, Seen::Subscribe { .. }))
                    .count();
                say(&json!({"open": broker.open(), "subscribed": subscribed}).to_string());
            }
            "quit" => break,
            _ => say("ERR unknown command"),
        }
    }
}

fn client(args: &[String]) {
    let (mut port, mut count, mut workers) = (0_u16, 200_usize, 0_usize);
    let mut iter = args.iter();
    while let Some(flag) = iter.next() {
        let value = iter
            .next()
            .unwrap_or_else(|| panic!("{flag} needs a value"));
        match flag.as_str() {
            "--port" => port = value.parse().expect("--port"),
            "--count" => count = value.parse().expect("--count"),
            "--workers" => workers = value.parse().expect("--workers"),
            other => panic!("unknown argument {other}"),
        }
    }
    assert!(port != 0, "--port is required");
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    if workers > 0 {
        builder.worker_threads(workers);
    }
    let runtime = builder.build().expect("runtime");
    let tls = insecure_tls();
    say("READY");
    if line().as_deref() != Some("go") {
        return;
    }

    let started = Instant::now();
    let runtimes: Vec<Arc<MqttRuntime>> = runtime.block_on(async {
        let mut tasks = Vec::new();
        for index in 0..count {
            let mut options = MqttRuntimeOptions::default();
            options.tls = Some(Arc::clone(&tls));
            options.connect_timeout = Duration::from_secs(60);
            // Any 32 hex digits: nothing is published, so the key is never used.
            let runtime = Arc::new(MqttRuntime::new(
                "00112233445566778899aabbccddeeff",
                options,
            ));
            let bootstrap = MqttBootstrap {
                user_id: format!("{}", 10_001 + index),
                broker_host: "127.0.0.1".to_owned(),
                broker_port: i64::from(port),
                topic: format!("oversea/res/{}", 10_001 + index),
                client_id: format!("bench-client-{index}"),
                username: format!("{}", 10_001 + index),
                password: "bench".to_owned(),
            };
            tasks.push(tokio::spawn(async move {
                runtime.start(&bootstrap).await.map(|()| runtime)
            }));
        }
        let mut runtimes = Vec::new();
        let mut failed = 0_u64;
        for task in tasks {
            match task.await {
                Ok(Ok(runtime)) => runtimes.push(runtime),
                _ => failed += 1,
            }
        }
        if failed > 0 {
            eprintln!("{failed} runtimes failed to start");
        }
        runtimes
    });
    let connected = runtimes
        .iter()
        .filter(|runtime| runtime.status().connected)
        .count();
    say(&format!(
        "RESULT {}",
        json!({
            "implementation": "openbyd",
            "count": count,
            "started": runtimes.len(),
            "connected": connected,
            "connect_s": started.elapsed().as_secs_f64(),
        })
    ));
    // Hold the connections until told to quit.
    while let Some(command) = line() {
        match command.as_str() {
            "status" => {
                let connected = runtimes
                    .iter()
                    .filter(|runtime| runtime.status().connected)
                    .count();
                say(&json!({"connected": connected}).to_string());
            }
            "quit" => break,
            _ => say("ERR unknown command"),
        }
    }
    runtime.block_on(async {
        for runtime in &runtimes {
            runtime.stop().await;
        }
    });
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("broker") => broker(),
        Some("client") => client(&args[1..]),
        _ => {
            eprintln!(
                "usage: mqtt_idle broker | mqtt_idle client --port P --count N [--workers W]"
            );
            std::process::exit(2);
        }
    }
}
