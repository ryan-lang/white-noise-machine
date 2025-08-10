// main.rs
// Cargo (add these to Cargo.toml):
// [dependencies]
// anyhow = "1"
// clap = { version = "4", features = ["derive"] }
// env_logger = "0.11"
// log = "0.4"
// rumqttc = { version = "0.24", default-features = false, features = ["use-rustls"] }
// serde = { version = "1", features = ["derive"] }
// serde_json = "1"
// tokio = { version = "1", features = ["macros", "rt-multi-thread", "signal", "process"] }
// rand = "0.8"

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use log::{debug, error, info, warn};
use rand::{Rng, distributions::Alphanumeric};
use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Outgoing, QoS};
use serde::Serialize;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::signal;
use tokio::time::sleep;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "ha-white-noise",
    author,
    version,
    about = "Home Assistant MQTT white-noise switch"
)]
struct Args {
    /// Path to the MP3 to loop
    #[arg(long)]
    mp3_path: String,

    /// Friendly name for the device in Home Assistant
    #[arg(long)]
    friendly_name: String,

    /// Stable device ID (used for discovery identifiers / topics)
    #[arg(long)]
    device_id: String,

    /// MQTT server (host[:port], default port 1883 if omitted)
    #[arg(long)]
    mqtt_server: String,

    /// MQTT username (optional)
    #[arg(long)]
    mqtt_username: Option<String>,

    /// MQTT password (optional)
    #[arg(long)]
    mqtt_password: Option<String>,
}

struct Player {
    child: Option<Child>,
}

impl Player {
    fn new() -> Self {
        Self { child: None }
    }

    fn is_running(&mut self) -> bool {
        if let Some(child) = &mut self.child {
            match child.try_wait() {
                Ok(Some(_status)) => {
                    // process exited
                    self.child = None;
                    false
                }
                Ok(None) => true,
                Err(e) => {
                    warn!("try_wait on mpg123 failed: {e}");
                    // Assume still running unless proven otherwise.
                    true
                }
            }
        } else {
            false
        }
    }

    fn start(&mut self, mp3_path: &str) -> Result<()> {
        if self.is_running() {
            info!("mpg123 already running; ignoring start");
            return Ok(());
        }
        // Spawn mpg123 looping forever, quiet mode
        // --loop -1 loops indefinitely; -q suppresses stdout; -Z would shuffle (not needed)
        let child = Command::new("mpg123")
            .arg("-q")
            .arg("--loop")
            .arg("-1")
            .arg(mp3_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| "failed to spawn mpg123 (is it installed?)")?;

        info!("Started mpg123 (PID {})", child.id());
        self.child = Some(child);
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            info!("Stopping mpg123 (PID {})", child.id());
            // Try graceful kill first
            #[cfg(unix)]
            {
                use nix::sys::signal::{Signal, kill};
                use nix::unistd::Pid;
                if let Err(e) = kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM) {
                    warn!("SIGTERM to mpg123 failed: {e}");
                }
            }

            // Wait a bit, then force kill if still alive
            for _ in 0..10 {
                match child.try_wait() {
                    Ok(Some(_status)) => {
                        info!("mpg123 exited");
                        return Ok(());
                    }
                    Ok(None) => {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Err(e) => {
                        warn!("try_wait while stopping: {e}");
                        break;
                    }
                }
            }

            warn!("Forcing kill of mpg123");
            let _ = child.kill();
            let _ = child.wait();
        } else {
            debug!("stop requested, but no running mpg123 child");
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct Device {
    identifiers: Vec<String>,
    name: String,
    manufacturer: String,
    model: String,
    sw_version: String,
}

#[derive(Serialize)]
struct SwitchConfig {
    name: String,
    unique_id: String,
    command_topic: String,
    state_topic: String,
    availability_topic: String,
    payload_on: String,
    payload_off: String,
    state_on: String,
    state_off: String,
    icon: String,
    device: Device,
}

fn parse_host_port(s: &str) -> (String, u16) {
    if let Some((h, p)) = s.rsplit_once(':') {
        if let Ok(port) = p.parse::<u16>() {
            return (h.to_string(), port);
        }
    }
    (s.to_string(), 1883)
}

fn random_suffix(n: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(n)
        .map(char::from)
        .collect()
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();
    info!("Starting with args: {:?}", args);

    // Quick pre-flight: ensure mpg123 is available
    let which = Command::new("which")
        .arg("mpg123")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match which {
        Ok(st) if st.success() => debug!("mpg123 found"),
        _ => {
            return Err(anyhow!(
                "mpg123 was not found in PATH. Install it (e.g. `sudo apt install mpg123`)."
            ));
        }
    }

    // Topics
    let base = format!("homeassistant");
    let availability_topic = format!("{}/{}", base, format!("{}/availability", args.device_id));
    let switch_object_id = "white_noise";
    let command_topic = format!(
        "{}/switch/{}/{}/set",
        base, args.device_id, switch_object_id
    );
    let state_topic = format!(
        "{}/switch/{}/{}/state",
        base, args.device_id, switch_object_id
    );
    let config_topic = format!(
        "{}/switch/{}/{}/config",
        base, args.device_id, switch_object_id
    );

    // MQTT options with LWT
    let (host, port) = parse_host_port(&args.mqtt_server);
    let client_id = format!("{}-wn-{}", args.device_id, random_suffix(6));
    let mut mqttopts = MqttOptions::new(client_id, host, port);
    mqttopts.set_keep_alive(Duration::from_secs(30));
    mqttopts.set_last_will(rumqttc::LastWill {
        topic: availability_topic.clone(),
        message: "offline".into(),
        qos: QoS::AtLeastOnce,
        retain: true,
    });
    if let (Some(u), Some(p)) = (args.mqtt_username.clone(), args.mqtt_password.clone()) {
        mqttopts.set_credentials(u, p);
    }

    let (client, mut eventloop) = AsyncClient::new(mqttopts, 10);

    // Connect & wait for ack before discovery
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(90)).await;
            // keep the runtime alive; rumqttc pings internally
        }
    });

    // Small helper to publish retained messages with logging
    async fn pub_retained(client: &AsyncClient, topic: &str, payload: &str) -> Result<()> {
        info!("MQTT publish (retained) to {} -> {}", topic, payload);
        client
            .publish(topic, QoS::AtLeastOnce, true, payload.as_bytes())
            .await
            .context("publish failed")
    }

    // Build discovery config
    let cfg = SwitchConfig {
        name: args.friendly_name.clone(),
        unique_id: format!("{}_{}", args.device_id, switch_object_id),
        command_topic: command_topic.clone(),
        state_topic: state_topic.clone(),
        availability_topic: availability_topic.clone(),
        payload_on: "ON".into(),
        payload_off: "OFF".into(),
        state_on: "ON".into(),
        state_off: "OFF".into(),
        icon: "mdi:volume-high".into(),
        device: Device {
            identifiers: vec![args.device_id.clone()],
            name: args.friendly_name.clone(),
            manufacturer: "Custom".into(),
            model: "WhiteNoisePlayer".into(),
            sw_version: env!("CARGO_PKG_VERSION").into(),
        },
    };
    let cfg_json = serde_json::to_string(&cfg)?;

    // A shared player
    let player = Arc::new(Mutex::new(Player::new()));
    let player_mp3 = args.mp3_path.clone();

    // We’ll drive the event loop in a separate task and interact via channels.
    // But for simplicity, we’ll just run the loop inline here.
    // When connected, publish discovery + availability + initial state.
    let mut connected = false;

    // Subscribe once after (first) connect
    let mut subscribed = false;

    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(incoming)) => {
                // Debug trace
                debug!("Incoming: {:?}", incoming);

                use rumqttc::Packet::*;
                match incoming {
                    ConnAck(_) => {
                        info!("Connected to MQTT broker");
                        connected = true;

                        // Online
                        pub_retained(&client, &availability_topic, "online").await?;
                        // Discovery config
                        pub_retained(&client, &config_topic, &cfg_json).await?;
                        // Initial state OFF (retained)
                        pub_retained(&client, &state_topic, "OFF").await?;

                        // Subscribe to commands
                        if !subscribed {
                            info!("Subscribing to {}", command_topic);
                            client.subscribe(&command_topic, QoS::AtLeastOnce).await?;
                            subscribed = true;
                        }
                    }
                    Publish(p) => {
                        let topic = p.topic.clone();
                        let payload = String::from_utf8_lossy(&p.payload).to_string();
                        debug!("Publish on {} -> {}", topic, payload);

                        if topic == command_topic {
                            let cmd = payload.trim().to_ascii_uppercase();
                            match cmd.as_str() {
                                "ON" => {
                                    info!("Command: ON -> start white noise");
                                    let mut pl = player.lock().unwrap();
                                    if let Err(e) = pl.start(&player_mp3) {
                                        error!("Failed to start mpg123: {e:#}");
                                    } else {
                                        client
                                            .publish(&state_topic, QoS::AtLeastOnce, true, "ON")
                                            .await
                                            .ok();
                                    }
                                }
                                "OFF" => {
                                    info!("Command: OFF -> stop white noise");
                                    let mut pl = player.lock().unwrap();
                                    if let Err(e) = pl.stop() {
                                        error!("Failed to stop mpg123: {e:#}");
                                    }
                                    client
                                        .publish(&state_topic, QoS::AtLeastOnce, true, "OFF")
                                        .await
                                        .ok();
                                }
                                other => {
                                    warn!("Unknown command payload: {}", other);
                                }
                            }
                        }
                    }
                    PingReq | PingResp | SubAck(_) | PubAck(_) | PubComp(_) | PubRec(_)
                    | PubRel(_) => {
                        // fine
                    }
                    _ => {}
                }
            }
            Ok(Event::Outgoing(out)) => {
                if matches!(out, Outgoing::Disconnect) {
                    info!("Outgoing disconnect");
                } else {
                    debug!("Outgoing: {:?}", out);
                }
            }
            Err(e) => {
                error!("MQTT event loop error: {e:#}");
                connected = false;
                // Backoff to avoid hot loop
                sleep(Duration::from_secs(2)).await;
            }
        }

        // Graceful shutdown on SIGINT/SIGTERM (non-blocking check)
        if connected {
            if let Ok(Some(_)) = signal::ctrl_c().now_or_never().transpose() {
                info!("Received Ctrl-C, shutting down...");
                // Publish offline and stop player
                let _ = pub_retained(&client, &availability_topic, "offline").await;
                {
                    let mut pl = player.lock().unwrap();
                    let _ = pl.stop();
                }
                // Allow broker flush
                sleep(Duration::from_millis(200)).await;
                break;
            }
            // On Linux, also listen to SIGTERM in a background task once (best-effort)
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                static mut TERM_SET: bool = false;
                unsafe {
                    if !TERM_SET {
                        TERM_SET = true;
                        let client2 = client.clone();
                        let availability_topic2 = availability_topic.clone();
                        let player2 = player.clone();
                        tokio::spawn(async move {
                            if let Ok(mut s) = signal(SignalKind::terminate()) {
                                s.recv().await;
                                info!("Received SIGTERM, shutting down...");
                                let _ = client2
                                    .publish(
                                        &availability_topic2,
                                        QoS::AtLeastOnce,
                                        true,
                                        "offline",
                                    )
                                    .await;
                                {
                                    let mut pl = player2.lock().unwrap();
                                    let _ = pl.stop();
                                }
                                // Give a moment to flush
                                sleep(Duration::from_millis(200)).await;
                                std::process::exit(0);
                            }
                        });
                    }
                }
            }
        }
    }

    Ok(())
}
