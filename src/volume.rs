//! Device-owned noise gain. No sound devices, MQTT, or playback commands here.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Setting {
    pub volume: u8,
    pub version: String,
    pub operation_id: String,
}

impl Setting {
    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path)?;
        let value: Self = serde_json::from_slice(&bytes)?;
        if value.volume > 100 || value.version.is_empty() || value.version.len() > 128 {
            bail!("invalid persisted noise volume");
        }
        Ok(value)
    }

    fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .context("volume state needs a parent directory")?;
        let tmp = parent.join(format!(
            ".noise-volume-{}-{}",
            std::process::id(),
            crate::random_suffix(12)
        ));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(&serde_json::to_vec(self)?)?;
            file.sync_all()?;
            fs::rename(&tmp, path)?;
            File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(tmp);
        }
        result
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub volume: u8,
    pub expected_version: String,
    pub operation_id: String,
    pub expires_at_ms: u64,
}

pub struct Controller {
    pub setting: Setting,
    pub path: std::path::PathBuf,
    durable: bool,
}

impl Controller {
    // Read-only startup. Missing state starts at the unchanged baseline; a corrupt
    // state is an error, never an excuse to reset gain to full volume.
    pub fn open(path: &Path) -> Result<Self> {
        let setting = match Setting::load(path) {
            Ok(value) => value,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Setting {
                    volume: 100,
                    version: crate::random_suffix(24),
                    operation_id: String::new(),
                }
            }
            Err(error) => return Err(error.context("cannot read persisted noise volume")),
        };
        Ok(Self {
            setting,
            path: path.into(),
            durable: true,
        })
    }

    pub fn apply(&mut self, command: &Command, now: u64) -> &'static str {
        if command.volume > 100
            || command.operation_id.is_empty()
            || command.operation_id.len() > 128
        {
            return "failed";
        }
        if self.setting.operation_id == command.operation_id {
            if !self.durable {
                return "unknown";
            }
            return if self.setting.volume == command.volume {
                "confirmed"
            } else {
                "conflict"
            };
        }
        if command.expires_at_ms <= now || command.expires_at_ms > now.saturating_add(30_000) {
            return "expired";
        }
        if command.expected_version != self.setting.version {
            return "conflict";
        }
        let next = Setting {
            volume: command.volume,
            version: crate::random_suffix(24),
            operation_id: command.operation_id.clone(),
        };
        if next.save(&self.path).is_err() {
            // Rename can succeed before directory fsync fails. Keep reporting
            // what is actually on disk rather than a stale in-memory setting.
            if let Ok(actual) = Setting::load(&self.path) {
                self.durable = actual.operation_id != command.operation_id;
                self.setting = actual;
            }
            return if self.durable { "failed" } else { "unknown" };
        }
        self.setting = next;
        self.durable = true;
        "confirmed"
    }

    pub fn report(&self, operation_id: &str, status: &str) -> String {
        let status = if status == "confirmed" && !self.durable {
            "unknown"
        } else {
            status
        };
        serde_json::json!({ "volume": self.setting.volume, "max_volume": 100,
            "version": self.setting.version, "operation_id": operation_id,
            "status": status, "observed_at_ms": now_ms() })
        .to_string()
    }

    pub fn receive(&mut self, payload: &str, retained: bool, now: u64) -> Option<String> {
        if retained {
            return None;
        }
        let command = serde_json::from_str::<Command>(payload).ok()?;
        let status = self.apply(&command, now);
        Some(self.report(&command.operation_id, status))
    }
}

/// Apply a bounded linear gain ramp to stereo signed 16-bit little-endian PCM.
/// The existing first-pass fade and downstream graceful-OFF adapter are intact.
fn scale(bytes: &mut [u8], from: f64, to: f64) {
    let frames = bytes.len() / 4;
    for (i, frame) in bytes.chunks_exact_mut(4).enumerate() {
        let gain = from + (to - from) * ((i + 1) as f64 / frames as f64);
        for sample in frame.chunks_exact_mut(2) {
            let value = i16::from_le_bytes([sample[0], sample[1]]);
            sample.copy_from_slice(&((value as f64 * gain).round() as i16).to_le_bytes());
        }
    }
}

pub fn pcm(path: &Path) -> Result<()> {
    process_pcm(path, std::io::stdin().lock(), std::io::stdout().lock())
}

fn process_pcm(path: &Path, mut input: impl Read, mut output: impl Write) -> Result<()> {
    let initial = Controller::open(path)?;
    let mut gain = initial.setting.volume as f64 / 100.0;
    let mut buffer = [0u8; 3840]; // 20 ms at 48 kHz stereo.
    let mut pending = Vec::with_capacity(buffer.len() + 3);
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        pending.extend_from_slice(&buffer[..count]);
        let aligned = pending.len() / 4 * 4;
        // On a later read error keep the last applied gain, never jump to unity.
        let next = Setting::load(path)
            .map(|s| s.volume as f64 / 100.0)
            .unwrap_or(gain);
        scale(&mut pending[..aligned], gain, next);
        output.write_all(&pending[..aligned])?;
        output.flush()?;
        pending.drain(..aligned);
        if aligned > 0 {
            gain = next;
        }
    }
    if !pending.is_empty() {
        bail!("truncated stereo PCM frame");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn directory() -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("noise-volume-test-{}", crate::random_suffix(16)));
        fs::create_dir(&path).unwrap();
        path
    }
    #[test]
    fn persistence_versions_duplicates_and_expiry() {
        let dir = directory();
        let path = dir.join("volume.json");
        let mut controller = Controller::open(&path).unwrap();
        assert!(!path.exists(), "startup wrote to disk");
        let mut command = Command {
            volume: 25,
            expected_version: controller.setting.version.clone(),
            operation_id: "one".into(),
            expires_at_ms: 2000,
        };
        assert_eq!(controller.apply(&command, 1000), "confirmed");
        let version = controller.setting.version.clone();
        assert_eq!(Controller::open(&path).unwrap().setting.volume, 25);
        assert_eq!(controller.apply(&command, 3000), "confirmed");
        assert_eq!(controller.setting.version, version);
        command.operation_id = "two".into();
        assert_eq!(controller.apply(&command, 3000), "expired");
        assert_eq!(controller.apply(&command, 1000), "conflict");
        command.expected_version = version;
        assert_eq!(controller.apply(&command, 1000), "confirmed");
        fs::remove_file(path).unwrap();
        fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn corrupt_state_and_unwritable_location_never_reset_or_confirm() {
        let dir = directory();
        let path = dir.join("volume.json");
        fs::write(&path, b"broken").unwrap();
        assert!(Controller::open(&path).is_err());
        fs::remove_file(&path).unwrap();
        let mut controller = Controller::open(&dir.join("missing/volume.json")).unwrap();
        let command = Command {
            volume: 0,
            expected_version: controller.setting.version.clone(),
            operation_id: "one".into(),
            expires_at_ms: 2000,
        };
        assert_eq!(controller.apply(&command, 1000), "failed");
        assert_eq!(controller.setting.volume, 100);
        fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn pcm_gain_is_bounded_and_ramped() {
        let mut bytes = [10000i16.to_le_bytes(), (-10000i16).to_le_bytes()].concat();
        scale(&mut bytes, 1.0, 0.5);
        assert_eq!(i16::from_le_bytes([bytes[0], bytes[1]]), 5000);
        assert_eq!(i16::from_le_bytes([bytes[2], bytes[3]]), -5000);
        scale(&mut bytes, 0.0, 0.0);
        assert_eq!(bytes, vec![0; 4]);
    }

    #[test]
    fn retained_and_malformed_commands_cannot_write_or_change_volume() {
        let dir = directory();
        let path = dir.join("volume.json");
        let mut controller = Controller::open(&path).unwrap();
        let payload=serde_json::json!({"volume":0,"expected_version":controller.setting.version,"operation_id":"one","expires_at_ms":2000}).to_string();
        assert!(controller.receive(&payload, true, 1000).is_none());
        assert!(controller.receive(r#"{"volume":0}"#, false, 1000).is_none());
        assert!(controller
            .receive(r#"{"volume":-1}"#, false, 1000)
            .is_none());
        assert!(!path.exists());
        assert_eq!(controller.setting.volume, 100);
        let report: serde_json::Value =
            serde_json::from_str(&controller.receive(&payload, false, 1000).unwrap()).unwrap();
        assert_eq!(report["status"], "confirmed");
        assert_eq!(report["volume"], 0);
        assert_eq!(Controller::open(&path).unwrap().setting.volume, 0);
        fs::remove_file(path).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn pcm_restores_saved_gain_and_follows_live_changes_without_reopening_output() {
        let dir = directory();
        let path = dir.join("volume.json");
        Setting {
            volume: 50,
            version: "v1".into(),
            operation_id: "one".into(),
        }
        .save(&path)
        .unwrap();
        struct Frames {
            path: std::path::PathBuf,
            reads: usize,
        }
        impl Read for Frames {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                if self.reads > 2 {
                    return Ok(0);
                }
                if self.reads == 2 {
                    Setting {
                        volume: 25,
                        version: "v2".into(),
                        operation_id: "two".into(),
                    }
                    .save(&self.path)
                    .unwrap();
                }
                buffer[..4]
                    .copy_from_slice(&[10000i16.to_le_bytes(), (-10000i16).to_le_bytes()].concat());
                Ok(4)
            }
        }
        let mut output = Vec::new();
        process_pcm(
            &path,
            Frames {
                path: path.clone(),
                reads: 0,
            },
            &mut output,
        )
        .unwrap();
        let samples: Vec<_> = output
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(samples, vec![5000, -5000, 2500, -2500]);
        assert_eq!(Setting::load(&path).unwrap().volume, 25);
        fs::remove_file(path).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[test]
    fn pcm_handles_partial_reads_and_rejects_truncated_frames() {
        let dir = directory();
        let path = dir.join("volume.json");
        struct Bytes {
            data: std::io::Cursor<Vec<u8>>,
        }
        impl Read for Bytes {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.data.read(&mut buffer[..1])
            }
        }
        let frame = vec![0xff, 0x7f, 0, 0x80];
        let mut output = Vec::new();
        process_pcm(
            &path,
            Bytes {
                data: std::io::Cursor::new(frame.clone()),
            },
            &mut output,
        )
        .unwrap();
        assert_eq!(output, frame);
        assert!(!path.exists());
        assert!(process_pcm(&path, &[1u8, 2, 3][..], Vec::new()).is_err());
        fs::remove_dir(dir).unwrap();
    }
}
