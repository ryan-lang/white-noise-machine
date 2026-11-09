# MQTT white-noise player

The player owns audio playback through `mpg123 -> sox -> aplay`; Nestling can
control it directly through MQTT while Home Assistant discovery remains available.
The first pass fades in over three seconds; subsequent passes loop through the
same `aplay` process for continuous playback. Runtime dependencies include
`mpg123`, `sox`, `aplay`, `sh`, `pkill`, and the host's `all_speakers` ALSA PCM.

`--topic_base` is required and accepts `appliances/white-noise/florencesroom`
or `appliances/white-noise/masterbedroom`.

| Purpose | Topic | Payload |
| --- | --- | --- |
| Command | `{topic_base}/set` | `ON` / `OFF` |
| State | `{topic_base}/state` | `ON` / `OFF` |
| Availability | `{topic_base}/availability` | `online` / `offline` |

Commands must be **non-retained**; retained commands are ignored. Home Assistant
discovery remains at `homeassistant/switch/{device_id}/white_noise/config`.
States and availability are retained;
availability uses an MQTT last will. Reconnection reports actual process state
and restores the command subscription without starting or stopping playback.
An unexpected process exit is detected by periodic process checks and reported
as `OFF`. State describes the process, not an acoustic measurement.

## Source synchronization

The local source incorporates Firefly's on-device commits `049b056` (ALSA
pipeline), `08cf53a` (fade-in and Makefile), and `bbe104c` (continuous playback),
plus the deployed room-topic changes. Existing local process-state and shutdown
error-handling fixes are retained. Firefly's separate graceful-OFF adapter at
`/usr/local/libexec/firefly-noise/aplay` and host ALSA/systemd configuration are
not part of this repository and must remain in place on that host.

## Temporary fade (source only; not commissioned)

With `--volume_state_file` enabled, the same non-retained volume topic accepts:

```json
{"action":"fade","volume":25,"duration_seconds":2700,"expected_version":"device-version","operation_id":"unique-id","expires_at_ms":1800000008000}
```

The player rejects a fade while playback is OFF, a target above the current
effective level, or durations outside 60–43200 seconds. It executes locally in
the existing noise-only PCM gain helper; no repeated MQTT commands, new audio
processes, hardware mixer changes, or playback restarts are needed. Normal volume
is preserved. `volume` in reports remains normal volume; `effective_volume` gives
current attenuation, `supports_fade: true` advertises support, and optional `fade`
contains `status` (fading/holding), `target_volume`, and `ends_at_ms`.

`action: "cancel"` holds current effective volume; `action: "restore"` explicitly
returns to normal. These use `volume: 0`, zero/omitted `duration_seconds`, and the
same version/operation/expiry fields. Ordinary volume commands cancel temporary
gain and save a new normal level. The next explicit OFF→ON clears temporary gain
before opening audio. Repeated ON while already playing does not reset a fade.
OFF does not raise gain while graceful-stop buffers drain. At target zero the
process remains ON and silent. Playback never starts automatically.

Temporary state is saved alongside, not in place of, normal volume; opening or
reconnecting is read-only. The helper continues an accepted fade if Nestling or
MQTT disconnects. A newer state file is not readable by older strict-schema
players: downgrades require a reviewed maintenance procedure, not replacing the
file with a stale saved normal level. Preserve backups independently of CI.
Nestling owns daily schedule triggers and once-per-date receipts; the player
accepts a generic fade action suitable for future sleep-state triggers.

**Do not deploy/restart either player as part of the disabled Nestling rollout.**
Commission in a separately approved awake maintenance window, one room at a time.

## Optional durable white-noise volume (not activated on live hosts)

`--volume_state_file /var/lib/white-noise-machine/volume.json` opts into a
noise-only PCM gain stage between the existing decoder/fade and `aplay`.
Without this option, the existing playback pipeline/topics are unchanged.
Volume changes do not reopen playback, reapply the start fade, or touch the
shared ALSA mixer, camera audio, music, or external amplifier controls.

The player owns this file; Nestling and clients never restore cached values.
The setting is atomically replaced with file and directory fsync, with private
permissions. The parent directory must already exist and be writable by the
existing service account (for example, a systemd `StateDirectory`). A missing
file initially means 100%, equal to the existing unchanged digital level;
startup itself writes nothing. Existing corrupt/unreadable state is an error,
not a reason to reset to full volume. Once set, volume survives controller/host
restarts and ON/OFF cycles. This does not add automatic playback after reboot.
Treat the state file as player-owned; do not edit it behind the controller.

The gain ranges from 0–100% of the existing baseline, with no amplification.
Changes ramp across one PCM chunk (up to 20 ms at the nominal 48 kHz rate).
The PCM helper reads the same durable state while running, without changing
the existing single-`aplay` path or Firefly's external graceful-stop adapter.
If state becomes unreadable during playback, the helper keeps its last gain.

| Purpose | Topic | Payload |
| --- | --- | --- |
| Volume command | `{topic_base}/volume/set` | JSON below, non-retained |
| Volume state | `{topic_base}/volume/state` | retained JSON, refreshed every second |

```json
{"volume":40,"expected_version":"device-issued-version","operation_id":"unique-id","expires_at_ms":1800000008000}
```

Commands expire within 30 seconds and use an absolute volume, not a relative
increment. Repeated last-command IDs are acknowledged without reapplying;
stale versions, expired/retained/invalid commands cannot adjust gain.
State contains `volume`, `max_volume` (100), `version`, `operation_id`, `status`
and `observed_at_ms`. A confirmed result means the device durably accepted its
digital gain setting, not an acoustic measurement or proof of speaker health.
The helper applies the saved setting on its next PCM chunk. No volume commands
are generated by startup/reconnect, and the heartbeat queue is nonblocking.

**Activation is a separate, approved awake/maintenance-window operation.** Do
not install this binary into either running player, change ALSA routing, or add
the new service argument during source development/testing. Preserve each
host's service identity, room topics, current routing/gains, media, and Firefly's
graceful-stop PATH adapter. Commission one room at a time, initially at unchanged
100%, then verify attenuation, restoration, OFF-time changes, continuity, and
camera/music independence with explicit approval before advancing.

### Safe validation

```sh
cargo test --locked
```

Unit tests do not run the program, connect to MQTT, or invoke `mpg123`.
**Do not start/restart/deploy this player or send ON/OFF commands while a baby is
sleeping.** Source changes require a separately approved maintenance window.
