// main.rs
// Cargo (add these to Cargo.toml):
// [dependencies]
// anyhow = "1"
// clap = { version = "4" }
// log = "0.4"
// rumqttc = { version = "0.24", default-features = false, features = ["use-rustls"] }
// tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
// rand = "0.9"

use anyhow::{anyhow, Context, Result};
use clap::{Arg, Command as ClapCommand};
use log::{debug, error, info, warn};
use rand::{distr::Alphanumeric, Rng};
use rumqttc::{AsyncClient, Event, MqttOptions, Outgoing, QoS};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::sleep;

#[derive(Debug, Clone)]
struct Args {
    mp3_path: String,
    friendly_name: String,
    device_id: String,
    mqtt_server: String,
    mqtt_username: Option<String>,
    mqtt_password: Option<String>,
}

impl Args {
    fn parse() -> Self {
        let matches = ClapCommand::new("ha-white-noise")
            .arg(
                Arg::new("mp3_path")
                    .long("mp3_path")
                    .value_name("PATH")
                    .required(true),
            )
            .arg(
                Arg::new("friendly_name")
                    .long("friendly_name")
                    .value_name("NAME")
                    .required(true),
            )
            .arg(
                Arg::new("device_id")
                    .long("device_id")
                    .value_name("ID")
                    .required(true),
            )
            .arg(
                Arg::new("mqtt_server")
                    .long("mqtt_server")
                    .value_name("HOST")
                    .required(true),
            )
            .arg(
                Arg::new("mqtt_username")
                    .long("mqtt_username")
                    .value_name("USER"),
            )
            .arg(
                Arg::new("mqtt_password")
                    .long("mqtt_password")
                    .value_name("PASS"),
            )
            .get_matches();

        Args {
            mp3_path: matches.get_one::<String>("mp3_path").unwrap().to_owned(),
            friendly_name: matches
                .get_one::<String>("friendly_name")
                .unwrap()
                .to_owned(),
            device_id: matches.get_one::<String>("device_id").unwrap().to_owned(),
            mqtt_server: matches.get_one::<String>("mqtt_server").unwrap().to_owned(),
            mqtt_username: matches.get_one::<String>("mqtt_username").cloned(),
            mqtt_password: matches.get_one::<String>("mqtt_password").cloned(),
        }
    }
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
            // Attempt to terminate the process. If this fails, fall back to force kill.
            if child.kill().is_err() {
                warn!("Failed to send kill signal to mpg123");
            }
            let _ = child.wait();
        } else {
            debug!("stop requested, but no running mpg123 child");
        }
        Ok(())
    }
}

struct Device {
    identifiers: Vec<String>,
    name: String,
    manufacturer: String,
    model: String,
    sw_version: String,
}

struct SwitchConfig {
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

impl SwitchConfig {
    fn to_json(&self) -> String {
        let dev = &self.device;
        let identifier = dev.identifiers.first().cloned().unwrap_or_default();
        format!(
            "{{\"unique_id\":\"{unique_id}\",\"command_topic\":\"{command}\",\"state_topic\":\"{state}\",\"availability_topic\":\"{availability}\",\"payload_on\":\"{payload_on}\",\"payload_off\":\"{payload_off}\",\"state_on\":\"{state_on}\",\"state_off\":\"{state_off}\",\"icon\":\"{icon}\",\"device\":{{\"identifiers\":[\"{identifier}\"],\"name\":\"{dev_name}\",\"manufacturer\":\"{manufacturer}\",\"model\":\"{model}\",\"sw_version\":\"{sw_version}\"}}}}",
            unique_id = self.unique_id,
            command = self.command_topic,
            state = self.state_topic,
            availability = self.availability_topic,
            payload_on = self.payload_on,
            payload_off = self.payload_off,
            state_on = self.state_on,
            state_off = self.state_off,
            icon = self.icon,
            identifier = identifier,
            dev_name = dev.name,
            manufacturer = dev.manufacturer,
            model = dev.model,
            sw_version = dev.sw_version,
        )
    }
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
    let mut rng = rand::rng();
    (&mut rng)
        .sample_iter(Alphanumeric)
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
    let cfg_json = cfg.to_json();

    // A shared player
    let player = Arc::new(Mutex::new(Player::new()));
    let player_mp3 = args.mp3_path.clone();

    // We’ll drive the event loop in a separate task and interact via channels.
    // But for simplicity, we’ll just run the loop inline here.
    // When connected, publish discovery + availability + initial state.

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
                // Backoff to avoid hot loop
                sleep(Duration::from_secs(2)).await;
            }
        }
    }

    #[allow(unreachable_code)]
    Ok(())
}
