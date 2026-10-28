#!/usr/bin/env python3
"""Safely remove Yandex cloud/telemetry tasks from Mini 3 startup.

This is deliberately not a filesystem wipe. The Mini 3 uses vendor Quasar
components for hardware access; platformd and mediad are protected because the
front display/controls and audio path must remain available. Wi-Fi and Bluetooth
services/configuration are outside this script and are never changed.

Default mode is a read-only preview. Use --apply to back up and update the
launcher configuration, or --restore BACKUP_ID to restore an exact backup.
Neither operation reboots the Station.
"""

from __future__ import annotations

import argparse
import json
import re
import shlex
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path


LAUNCH_FILE = "/system/vendor/quasar/launch.json"
BACKUP_ROOT = "/data/opt/yandex-removal"
REMOVE_TASKS = {
    "aliced",
    "external_commandsd",
    "fluent-bitd",
    "glagold",
    "iohub_clients",
    "telemetry",
    "updatesd",
}
PRESERVE_TASKS = {"mediad", "platformd"}


def ssh_base(args: argparse.Namespace) -> list[str]:
    command = [
        "ssh",
        "-p",
        str(args.port),
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=5",
        "-o",
        "StrictHostKeyChecking=accept-new",
    ]
    if args.identity:
        command += ["-i", str(args.identity)]
    return command + [f"root@{args.host}"]


def run(command: list[str], *, input_text: str | None = None) -> str:
    result = subprocess.run(
        command,
        input=input_text,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode:
        detail = result.stderr.strip() or result.stdout.strip()
        raise RuntimeError(f"command failed ({result.returncode}): {detail}")
    return result.stdout


def write_remote(args: argparse.Namespace, path: str, payload: bytes) -> None:
    command = ssh_base(args) + [f"umask 077 && cat > {shlex.quote(path)}"]
    result = subprocess.run(command, input=payload, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
    if result.returncode:
        detail = result.stderr.decode(errors="replace").strip()
        raise RuntimeError(detail or "SSH write failed")


def read_remote(args: argparse.Namespace, path: str = LAUNCH_FILE) -> bytes:
    result = subprocess.run(
        ssh_base(args) + [f"cat {shlex.quote(path)}"],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode:
        raise RuntimeError(result.stderr.decode(errors="replace").strip() or "SSH read failed")
    return result.stdout


def task_names(document: dict) -> list[str]:
    names: list[str] = []
    for node in document.get("nodes", []):
        for task in node.get("tasks", []):
            name = task.get("name")
            if isinstance(name, str):
                names.append(name)
    return names


def task_roles(document: dict) -> set[str]:
    roles: set[str] = set()
    for node in document.get("nodes", []):
        for task in node.get("tasks", []):
            name = task.get("name")
            if isinstance(name, str):
                roles.add(name)
            for argument in task.get("arg", []):
                if isinstance(argument, str) and argument.startswith("--"):
                    roles.add(argument[2:])
    return roles


def remove_tasks(document: dict, *, disable_platformd: bool = False) -> tuple[dict, list[str]]:
    removed: list[str] = []
    selected_tasks = REMOVE_TASKS | ({"platformd"} if disable_platformd else set())
    original_roles = task_roles(document)
    for node in document.get("nodes", []):
        tasks = node.get("tasks", [])
        kept = []
        for task in tasks:
            name = task.get("name")
            role_names = {name} if isinstance(name, str) else set()
            role_names.update(
                argument[2:]
                for argument in task.get("arg", [])
                if isinstance(argument, str) and argument.startswith("--")
            )
            if role_names & selected_tasks:
                removed.append(name)
            else:
                kept.append(task)
        node["tasks"] = kept

    remaining = task_roles(document)
    required_tasks = {"mediad"}
    if not disable_platformd and "platformd" in original_roles:
        required_tasks.add("platformd")
    missing = required_tasks - remaining
    if missing:
        raise RuntimeError(
            "refusing to modify unknown launcher state; required hardware tasks missing: "
            + ", ".join(sorted(missing))
        )
    return document, removed


def replace_remote(args: argparse.Namespace, staged_path: str) -> None:
    script = "\n".join(
        [
            "set -e",
            "root_mount_opts=$(awk '$2 == \"/\" { print $4; exit }' /proc/mounts)",
            "was_rw=0",
            "case \",$root_mount_opts,\" in *,rw,*) was_rw=1 ;; *) mount -o remount,rw / 2>/dev/null || :; root_mount_opts=$(awk '$2 == \"/\" { print $4; exit }' /proc/mounts); case \",$root_mount_opts,\" in *,rw,*) ;; *) echo 'rootfs did not become writable' >&2; exit 1 ;; esac ;; esac",
            "restore_mount_mode() { sync; if [ \"$was_rw\" -eq 0 ]; then mount -o remount,ro / 2>/dev/null || :; root_mount_opts=$(awk '$2 == \"/\" { print $4; exit }' /proc/mounts); case \",$root_mount_opts,\" in *,ro,*) ;; *) echo 'rootfs did not return to read-only mode' >&2; return 1 ;; esac; fi; }",
            "trap restore_mount_mode EXIT",
            f"cp {shlex.quote(staged_path)} {shlex.quote(LAUNCH_FILE + '.new')}",
            f"chown 0:0 {shlex.quote(LAUNCH_FILE + '.new')}",
            f"chmod 0644 {shlex.quote(LAUNCH_FILE + '.new')}",
            f"mv {shlex.quote(LAUNCH_FILE + '.new')} {shlex.quote(LAUNCH_FILE)}",
            "sync",
            "restore_mount_mode",
            "trap - EXIT",
        ]
    )
    run(ssh_base(args) + ["sh -c " + shlex.quote(script)])


def apply(
    args: argparse.Namespace,
    original: bytes,
    updated: bytes,
    backup_id: str,
    *,
    disable_platformd: bool = False,
) -> None:
    backup_dir = f"{BACKUP_ROOT}/{backup_id}"
    run(ssh_base(args) + [f"mkdir -p {shlex.quote(backup_dir)} && chmod 0700 {shlex.quote(BACKUP_ROOT)} {shlex.quote(backup_dir)}"])
    write_remote(args, f"{backup_dir}/launch.json.original", original)
    write_remote(args, f"{backup_dir}/launch.json.new", updated)
    replace_remote(args, f"{backup_dir}/launch.json.new")

    try:
        verified = json.loads(read_remote(args))
        selected_tasks = REMOVE_TASKS | ({"platformd"} if disable_platformd else set())
        if task_roles(verified) & selected_tasks:
            raise RuntimeError("post-write verification failed: removal tasks are still present")
        original_roles = task_roles(json.loads(original))
        required_tasks = {"mediad"}
        if not disable_platformd and "platformd" in original_roles:
            required_tasks.add("platformd")
        if not required_tasks <= task_roles(verified):
            raise RuntimeError("post-write verification failed: protected hardware tasks are missing")
    except Exception:
        restore_stage = f"{backup_dir}/restore.json"
        write_remote(args, restore_stage, original)
        replace_remote(args, restore_stage)
        raise


def restore(args: argparse.Namespace, backup_id: str) -> None:
    if not re.fullmatch(r"[A-Za-z0-9_-]+", backup_id):
        raise RuntimeError("invalid backup id")
    remote_backup = f"{BACKUP_ROOT}/{backup_id}/launch.json.original"
    original_bytes = read_remote(args, remote_backup)
    original = json.loads(original_bytes)
    if not PRESERVE_TASKS <= task_roles(original):
        raise RuntimeError("backup does not contain the protected hardware tasks; refusing restore")
    staging = f"{BACKUP_ROOT}/{backup_id}/restore.json"
    write_remote(args, staging, original_bytes)
    replace_remote(args, staging)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", required=True, help="Mini 3 address or hostname")
    parser.add_argument("--port", type=int, default=2222, help="SSH port")
    parser.add_argument("--identity", type=Path, help="SSH private key; otherwise use ssh-agent/default keys")
    parser.add_argument(
        "--disable-platformd",
        action="store_true",
        help="also remove maind --platformd from startup; microphone DSP and button handling will stop",
    )
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--apply", action="store_true", help="back up and remove known Yandex service tasks")
    mode.add_argument("--restore", metavar="BACKUP_ID", help="restore a saved launch.json")
    args = parser.parse_args()

    try:
        if args.restore:
            restore(args, args.restore)
            print(f"Restored launcher backup {args.restore}. Reboot the Station to apply it.")
            return 0

        if args.disable_platformd and not args.apply:
            raise RuntimeError("--disable-platformd requires --apply")

        original = read_remote(args)
        document = json.loads(original)
        names = task_names(document)
        print("Current startup tasks:")
        for name in names:
            print(f"  {name}")

        updated_document, removed = remove_tasks(document, disable_platformd=args.disable_platformd)
        protected = {"mediad"} if args.disable_platformd else PRESERVE_TASKS & task_roles(document)
        print("\nProtected hardware-related tasks:")
        for name in sorted(protected):
            print(f"  {name}")
        if args.disable_platformd:
            print("platformd will stop: Yandex mic DSP and physical-button handling will be unavailable.")
        print("\nWi-Fi config, Bluetooth services, kernel/drivers, /dev/fb0 and /dev/snd are not modified.")

        if not removed:
            print("No known Yandex cloud/telemetry startup tasks are present; no changes made.")
            return 0

        print("\nTasks selected for removal from startup: " + ", ".join(sorted(set(removed))))
        if not args.apply:
            print("Preview only. Pass --apply to save a device backup and update launch.json.")
            return 0

        backup_id = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        updated = (json.dumps(updated_document, indent=4, ensure_ascii=False) + "\n").encode()
        apply(args, original, updated, backup_id, disable_platformd=args.disable_platformd)
        print(f"Updated startup config. Backup ID: {backup_id}")
        print("No reboot was issued. Reboot manually after reviewing the change.")
        return 0
    except (OSError, RuntimeError, json.JSONDecodeError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
