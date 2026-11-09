#!/usr/bin/env python3
"""Explicitly authorized idle-player activation. Never sends playback commands."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import pwd
import shutil
import socket
import subprocess
import time

HOSTS = {
    "firefly": ("florencesroom", Path("/home/firefly/white-noise-machine"), "firefly"),
    "nightstand-masterbedroom": ("masterbedroom", Path("/usr/local/share/white-noise-source"), "white-noise"),
}


def run(*args, **kwargs):
    return subprocess.run(args, check=True, timeout=30, **kwargs)


def retained(topic):
    return subprocess.check_output(
        ["mosquitto_sub", "-h", "mqtt-ha.lan", "-t", topic, "-C", "1", "-W", "3"],
        text=True, timeout=5).strip()


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def idle():
    result = subprocess.run(["pgrep", "-x", "mpg123"], capture_output=True, timeout=5)
    if result.returncode != 1:
        raise RuntimeError("Playback must already be OFF; no implicit stop is allowed")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--authorize-maintenance", action="store_true", required=True)
    parser.add_argument("--artifacts", type=Path, required=True)
    parser.add_argument("--sha256", required=True)
    args = parser.parse_args()
    host = socket.gethostname().split(".")[0]
    if os.geteuid() != 0 or host not in HOSTS:
        raise RuntimeError("Wrong host/account")
    room, source_root, user = HOSTS[host]
    base = "appliances/white-noise/" + room
    candidate = args.artifacts / "white-noise-machine"
    if sha(candidate) != args.sha256:
        raise RuntimeError("Candidate checksum mismatch")
    help_text = subprocess.check_output([str(candidate), "--help"], text=True, timeout=5)
    if "--volume_state_file" not in help_text or "--topic_base" not in help_text:
        raise RuntimeError("Candidate is missing required capabilities")
    account = pwd.getpwnam(user)
    binary = Path("/usr/local/bin/white-noise-machine")
    unit = Path("/etc/systemd/system/white-noise.service")
    dropin = Path("/etc/systemd/system/white-noise.service.d/40-volume.conf")
    topics = unit.parent / "white-noise.service.d/30-room-topics.conf"
    starts = [line.removeprefix("ExecStart=") for line in topics.read_text().splitlines()
              if line.startswith("ExecStart=") and line != "ExecStart="]
    if len(starts) != 1 or not starts[0].startswith(str(binary) + " ") or "--topic_base " + base not in starts[0]:
        raise RuntimeError("Unexpected effective player command")
    state = Path("/var/lib/white-noise-machine/volume.json")
    desired = ("[Service]\nStateDirectory=white-noise-machine\nStateDirectoryMode=0700\n"
               "ExecStart=\nExecStart=" + starts[0] + " --volume_state_file " + str(state) + "\n")
    if sha(binary) == args.sha256 and dropin.exists() and dropin.read_text() == desired:
        run("systemctl", "is-active", "--quiet", "white-noise.service")
        print("Already activated; no restart")
        return
    if dropin.exists():
        raise RuntimeError("An unknown volume override already exists; investigate first")
    if sha(binary) != "98a4e9705517cbe952578932db96f30c0513f66697c19744b849535949b1df7d":
        raise RuntimeError("Unexpected deployed baseline; refusing replacement")
    run("systemctl", "is-active", "--quiet", "white-noise.service")
    idle()
    if retained(base + "/state") != "OFF":
        raise RuntimeError("Player does not report OFF")
    if state.exists():
        raise RuntimeError("Existing canonical volume needs explicit review before first activation")
    # Stage a complete effective unit and verify before stopping any service.
    validation = args.artifacts / "white-noise-candidate.service"
    validation.write_text(unit.read_text() + "\n" + desired)
    run("systemd-analyze", "verify", str(validation))
    source_dir = source_root / "src" if (source_root / "src").is_dir() else source_root
    files = {binary: candidate, source_dir / "main.rs": args.artifacts / "src/main.rs",
             source_dir / "volume.rs": args.artifacts / "src/volume.rs",
             source_root / "Cargo.toml": args.artifacts / "Cargo.toml",
             source_root / "Cargo.lock": args.artifacts / "Cargo.lock"}
    for source in files.values():
        if not source.is_file():
            raise RuntimeError("Incomplete candidate")
    backup = Path("/var/backups/white-noise-volume") / str(time.time_ns())
    backup.mkdir(parents=True, mode=0o700)
    shutil.copy2(unit, backup / "white-noise.service")
    shutil.copytree(topics.parent, backup / "service.d")
    original = {}
    for index, destination in enumerate(files):
        original[destination] = destination.exists()
        if destination.exists():
            shutil.copy2(destination, backup / str(index))
    (backup / "manifest.json").write_text(json.dumps({str(k): v for k, v in original.items()}))
    routing_hash = sha(Path("/etc/asound.conf"))
    startup_ms = int(time.time() * 1000)
    try:
        idle()  # Recheck immediately before stopping the idle controller.
        run("systemctl", "stop", "white-noise.service")
        for destination, source in files.items():
            temporary = destination.with_name(destination.name + ".volume-candidate")
            shutil.copyfile(source, temporary)
            temporary.chmod(0o755 if destination == binary else 0o644)
            if destination != binary:
                os.chown(temporary, account.pw_uid, account.pw_gid)
            os.replace(temporary, destination)
        dropin.write_text(desired)
        dropin.chmod(0o644)
        run("systemctl", "daemon-reload")
        run("systemctl", "start", "white-noise.service")
        deadline = time.monotonic() + 30
        while True:
            try:
                run("systemctl", "is-active", "--quiet", "white-noise.service")
                value = json.loads(retained(base + "/volume/state"))
                if (value["observed_at_ms"] < startup_ms or value["volume"] != 100
                        or value["max_volume"] != 100 or not value["version"]):
                    raise RuntimeError("Fresh unchanged-baseline volume not reported")
                if retained(base + "/availability") != "online" or retained(base + "/state") != "OFF":
                    raise RuntimeError("Expected online/OFF telemetry")
                idle()
                break
            except Exception:
                if time.monotonic() >= deadline:
                    raise
                time.sleep(1)
        if sha(Path("/etc/asound.conf")) != routing_hash or state.exists():
            raise RuntimeError("Routing changed or read-only startup wrote canonical state")
    except Exception:
        subprocess.run(["systemctl", "stop", "white-noise.service"], timeout=30)
        for index, destination in enumerate(files):
            if original[destination]:
                shutil.copy2(backup / str(index), destination)
                if destination != binary:
                    os.chown(destination, account.pw_uid, account.pw_gid)
            else:
                destination.unlink(missing_ok=True)
        dropin.unlink(missing_ok=True)
        subprocess.run(["systemctl", "daemon-reload"], timeout=30)
        subprocess.run(["systemctl", "start", "white-noise.service"], timeout=30)
        print(f"Activation failed; previous files restored from {backup}")
        raise
    print(f"{host}: volume ready at unchanged 100%, playback OFF; backup={backup}")


if __name__ == "__main__":
    main()
