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
mod volume;

#[derive(Debug, Clone)]
struct Args {
    mp3_path: String,
    friendly_name: String,
    device_id: String,
    mqtt_server: String,
    topic_base: String,
    volume_state_file: Option<String>,
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
            .arg(
                Arg::new("topic_base")
                    .long("topic_base")
                    .value_name("TOPIC")
                    .required(true)
                    .value_parser([
                        "appliances/white-noise/florencesroom",
                        "appliances/white-noise/masterbedroom",
                    ]),
            )
            .arg(
                Arg::new("volume_state_file")
                    .long("volume_state_file")
                    .value_name("PATH"),
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
            topic_base: matches.get_one::<String>("topic_base").unwrap().to_owned(),
            volume_state_file: matches.get_one::<String>("volume_state_file").cloned(),
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

    fn start(&mut self, mp3_path: &str, volume_state_file: Option<&str>) -> Result<()> {
        if self.is_running() {
            info!("mpg123 already running; ignoring start");
            return Ok(());
        }
        // Two-phase audio pipeline fed into a single aplay process:
        //   Phase 1: first play fades in over 3 seconds via sox
        //   Phase 2: subsequent loops play at full volume (no sox overhead)
        // A subshell groups the decode stages so aplay sees one continuous stream.
        let mut cmd = format!(
            concat!(
                "(mpg123 -s -r 48000 --stereo '{}' 2>/dev/null",
                " | sox -t raw -r 48000 -c 2 -e signed-integer -b 16 -L -",
                " -t raw -e signed-integer -b 16 -L - fade t 3;",
                " while true; do",
                " mpg123 -s -r 48000 --stereo '{}' 2>/dev/null;",
                " done)",
                " | aplay -D all_speakers -f S16_LE -r 48000 -c 2 2>/dev/null"
            ),
            mp3_path, mp3_path
        );
        let mut process = Command::new("sh");
        if let Some(path) = volume_state_file {
            cmd = cmd.replace(" | aplay -D", " | \"$WHITE_NOISE_GAIN_PROGRAM\" --pcm_gain_state_file \"$WHITE_NOISE_GAIN_STATE\" | aplay -D");
            process
                .env("WHITE_NOISE_GAIN_PROGRAM", std::env::current_exe()?)
                .env("WHITE_NOISE_GAIN_STATE", path);
        }
        let child = process
            .arg("-c")
            .arg(&cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| "failed to spawn mpg123|aplay pipeline")?;

        info!("Started audio pipeline with fade-in (PID {})", child.id());
        self.child = Some(child);
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            let pid = child.id();
            info!("Stopping audio pipeline (PID {})", pid);
            // Kill all child processes of the shell first
            let _ = Command::new("pkill")
                .arg("-P")
                .arg(pid.to_string())
                .status();
            // Then kill the shell itself
            if let Err(error) = child.kill() {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return Ok(());
                }
                // Preserve the handle rather than claiming OFF or blocking on
                // wait() for a process that could still be playing.
                self.child = Some(child);
                return Err(error).context("failed to terminate audio pipeline shell");
            }
            let _ = child.wait();
        } else {
            debug!("stop requested, but no running audio pipeline");
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

fn player_topics(base: &str) -> Result<(String, String, String)> {
    if !matches!(
        base,
        "appliances/white-noise/florencesroom" | "appliances/white-noise/masterbedroom"
    ) {
        return Err(anyhow!("unsupported appliance topic base"));
    }
    Ok((
        format!("{base}/set"),
        format!("{base}/state"),
        format!("{base}/availability"),
    ))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli: Vec<_> = std::env::args_os().collect();
    if cli.len() == 3 && cli[1] == "--pcm_gain_state_file" {
        return volume::pcm(std::path::Path::new(&cli[2]));
    }
    env_logger::init();
    let args = Args::parse();
    let mut noise_volume = args
        .volume_state_file
        .as_ref()
        .map(|path| volume::Controller::open(std::path::Path::new(path)))
        .transpose()?;
    info!("Starting white-noise player {}", args.device_id);

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

    // Quick pre-flight: ensure sox is available (used for fade-in)
    let which_sox = Command::new("which")
        .arg("sox")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match which_sox {
        Ok(st) if st.success() => debug!("sox found"),
        _ => {
            return Err(anyhow!(
                "sox was not found in PATH. Install it (e.g. `sudo apt install sox`)."
            ));
        }
    }

    // Home Assistant discovery is metadata only; all appliance traffic is separate.
    let (command_topic, state_topic, availability_topic) = player_topics(&args.topic_base)?;
    let volume_command_topic = format!("{}/volume/set", args.topic_base);
    let volume_state_topic = format!("{}/volume/state", args.topic_base);
    let switch_object_id = "white_noise";
    let config_topic = format!(
        "homeassistant/switch/{}/{}/config",
        args.device_id, switch_object_id
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

    // MQTT reconnection must not change playback. Observe the process independently
    // so an unexpected exit cannot leave a retained ON state indefinitely.
    let mut last_running: Option<bool> = None;
    let mut broker_connected = false;
    let mut process_check = tokio::time::interval(Duration::from_secs(1));

    loop {
        let event = tokio::select! {
            event = eventloop.poll() => Some(event),
            _ = process_check.tick() => None,
        };
        let Some(event) = event else {
            if let Some(controller) = noise_volume.as_ref().filter(|_| broker_connected) {
                // Never block the event loop on a full queue while disconnected.
                // The next tick/reconnect reports the same device-owned setting.
                let _ = client.try_publish(
                    &volume_state_topic,
                    QoS::AtLeastOnce,
                    true,
                    controller.report(&controller.setting.operation_id, "confirmed"),
                );
            }
            let running = player.lock().unwrap().is_running();
            if last_running != Some(running) {
                pub_retained(&client, &state_topic, if running { "ON" } else { "OFF" }).await?;
                last_running = Some(running);
            }
            continue;
        };
        match event {
            Ok(Event::Incoming(incoming)) => {
                // Debug trace
                debug!("Incoming: {:?}", incoming);

                use rumqttc::Packet::*;
                match incoming {
                    ConnAck(_) => {
                        broker_connected = true;
                        info!("Connected to MQTT broker");

                        // Online
                        pub_retained(&client, &availability_topic, "online").await?;
                        // Discovery config
                        pub_retained(&client, &config_topic, &cfg_json).await?;
                        // Reconnection reports actual playback; it never starts/stops it.
                        let running = player.lock().unwrap().is_running();
                        pub_retained(&client, &state_topic, if running { "ON" } else { "OFF" })
                            .await?;
                        last_running = Some(running);

                        // Subscribe to commands
                        info!("Subscribing to {}", command_topic);
                        client.subscribe(&command_topic, QoS::AtLeastOnce).await?;
                        if let Some(controller) = &noise_volume {
                            pub_retained(
                                &client,
                                &volume_state_topic,
                                &controller.report(&controller.setting.operation_id, "confirmed"),
                            )
                            .await?;
                            client
                                .subscribe(&volume_command_topic, QoS::AtLeastOnce)
                                .await?;
                        }
                    }
                    Publish(p) => {
                        let topic = p.topic.clone();
                        let payload = String::from_utf8_lossy(&p.payload).to_string();
                        debug!("Publish on {} -> {}", topic, payload);

                        if topic == volume_command_topic && !p.retain {
                            if let Some(controller) = &mut noise_volume {
                                if let Some(report) =
                                    controller.receive(&payload, p.retain, volume::now_ms())
                                {
                                    pub_retained(&client, &volume_state_topic, &report).await?;
                                } else {
                                    warn!("Invalid noise volume command");
                                }
                            }
                        }

                        if topic == command_topic && !p.retain {
                            let cmd = payload.trim().to_ascii_uppercase();
                            match cmd.as_str() {
                                "ON" => {
                                    info!("Command: ON -> start white noise");
                                    let mut pl = player.lock().unwrap();
                                    if let Err(e) =
                                        pl.start(&player_mp3, args.volume_state_file.as_deref())
                                    {
                                        error!("Failed to start mpg123: {e:#}");
                                    } else {
                                        client
                                            .publish(&state_topic, QoS::AtLeastOnce, true, "ON")
                                            .await
                                            .ok();
                                        last_running = Some(true);
                                    }
                                }
                                "OFF" => {
                                    info!("Command: OFF -> stop white noise");
                                    let mut pl = player.lock().unwrap();
                                    if let Err(e) = pl.stop() {
                                        error!("Failed to stop mpg123: {e:#}");
                                    } else {
                                        client
                                            .publish(&state_topic, QoS::AtLeastOnce, true, "OFF")
                                            .await
                                            .ok();
                                        last_running = Some(false);
                                    }
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
                broker_connected = false;
                error!("MQTT event loop error: {e:#}");
                // Backoff to avoid hot loop
                sleep(Duration::from_secs(2)).await;
            }
        }
    }

    #[allow(unreachable_code)]
    Ok(())
}

#[cfg(test)]
mod topic_tests {
    use super::*;

    #[test]
    fn separate_room_topics() {
        for room in ["florencesroom", "masterbedroom"] {
            let base = format!("appliances/white-noise/{room}");
            let (command, state, availability) = player_topics(&base).unwrap();
            assert_eq!(command, format!("{base}/set"));
            assert_eq!(state, format!("{base}/state"));
            assert_eq!(availability, format!("{base}/availability"));
            assert!(!command.starts_with("homeassistant/"));
        }
    }

    #[test]
    fn reject_legacy_unknown_or_wildcard_topics() {
        for base in [
            "homeassistant",
            "appliances/white-noise/+",
            "appliances/white-noise/#",
            "appliances/white-noise/babyboy",
            "appliances/white-noise/florencesroom/",
        ] {
            assert!(player_topics(base).is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests never connect to MQTT or invoke mpg123.
    #[test]
    fn an_empty_player_reports_not_running() {
        assert!(!Player::new().is_running());
    }

    #[test]
    fn an_exited_inert_process_is_detected_without_playback() {
        let mut child = Command::new("/bin/true").spawn().unwrap();
        child.wait().unwrap();
        let mut player = Player { child: Some(child) };
        assert!(!player.is_running());
        assert!(player.child.is_none());
    }

    #[test]
    fn broker_addresses_preserve_the_explicit_port() {
        assert_eq!(
            parse_host_port("broker.invalid:1884"),
            ("broker.invalid".to_string(), 1884)
        );
        assert_eq!(
            parse_host_port("broker.invalid"),
            ("broker.invalid".to_string(), 1883)
        );
    }
}
