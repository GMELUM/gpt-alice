#!/usr/bin/env python3
"""Back up and quarantine selected Yandex account and telemetry state on Mini 3.

Default mode is a read-only preview. --apply moves selected paths into a
private, timestamped backup on the same data filesystem. --restore moves them
back. Wi-Fi, Bluetooth, audio, microphone, display, LED, local sound/model,
ADB, and /data/opt files are deliberately excluded.
"""

from __future__ import annotations

import argparse
import re
import shlex
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import PurePosixPath


QUASAR = PurePosixPath("/bkupgrade/data/quasar")
BACKUP_ROOT = PurePosixPath("/bkupgrade/data/opt/yandex-removal")
RELATIVE_TARGETS = (
    "account_storage.dat",
    "appmetrica",
    "audioStorageHistory.json",
    "daemons_logs",
    "devices.json",
    "first_greeting_done",
    "minidump",
    "quasar_metricad.json",
    "registered",
)


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


def run(args: argparse.Namespace, script: str) -> str:
    result = subprocess.run(
        ssh_base(args) + ["sh -c " + shlex.quote(script)],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or result.stdout.strip() or "SSH command failed")
    return result.stdout


def safe_backup_id(value: str) -> str:
    if not re.fullmatch(r"[A-Za-z0-9_-]+", value):
        raise RuntimeError("invalid backup id")
    return value


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", required=True, help="Mini 3 address or hostname")
    parser.add_argument("--port", type=int, default=2222, help="SSH port")
    parser.add_argument("--identity", help="SSH private key")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--apply", action="store_true", help="move selected state into a backup")
    mode.add_argument("--restore", metavar="BACKUP_ID", help="restore quarantined state")
    args = parser.parse_args()

    try:
        if args.restore:
            backup_id = safe_backup_id(args.restore)
            backup_dir = BACKUP_ROOT / backup_id / "quasar-state"
            moves = []
            for relative in RELATIVE_TARGETS:
                source = QUASAR / relative
                saved = backup_dir / relative
                moves.append(
                    f"if [ -e {shlex.quote(str(saved))} ] || [ -L {shlex.quote(str(saved))} ]; then "
                    f"if [ -e {shlex.quote(str(source))} ] || [ -L {shlex.quote(str(source))} ]; then "
                    f"echo CONFLICT {shlex.quote(relative)}; exit 23; fi; "
                    f"mkdir -p {shlex.quote(str(source.parent))}; "
                    f"mv {shlex.quote(str(saved))} {shlex.quote(str(source))}; "
                    f"echo RESTORED {shlex.quote(relative)}; fi"
                )
            output = run(args, "set -e\n" + "\n".join(moves))
            print(output, end="")
            print(f"Restored state from backup {backup_id}.")
            return 0

        listing = run(
            args,
            "set -e\n" + "\n".join(
                f"if [ -e {shlex.quote(str(QUASAR / relative))} ] || [ -L {shlex.quote(str(QUASAR / relative))} ]; then "
                f"echo PRESENT {shlex.quote(relative)}; else echo ABSENT {shlex.quote(relative)}; fi"
                for relative in RELATIVE_TARGETS
            ),
        )
        present = [line.split(" ", 1)[1] for line in listing.splitlines() if line.startswith("PRESENT ")]
        print("Selected account, analytics, log, registration and personal state:")
        if present:
            for relative in present:
                print(f"  {relative}")
        else:
            print("  (none present)")
        print("Protected: Wi-Fi, Bluetooth, audio, mic, display, LEDs, local sounds/models, ADB and /data/opt.")
        if not args.apply:
            print("Preview only. Pass --apply to move these paths into a private on-device backup.")
            return 0
        if not present:
            print("Nothing to quarantine.")
            return 0

        backup_id = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        backup_dir = BACKUP_ROOT / backup_id / "quasar-state"
        commands = [
            "set -e",
            f"umask 077; mkdir -p {shlex.quote(str(backup_dir))}; chmod 0700 {shlex.quote(str(BACKUP_ROOT))} {shlex.quote(str(BACKUP_ROOT / backup_id))} {shlex.quote(str(backup_dir))}",
        ]
        for relative in present:
            source = QUASAR / relative
            saved = backup_dir / relative
            commands += [
                f"mkdir -p {shlex.quote(str(saved.parent))}",
                f"mv {shlex.quote(str(source))} {shlex.quote(str(saved))}",
                f"echo MOVED {shlex.quote(relative)}",
            ]
        output = run(args, "\n".join(commands))
        print(output, end="")
        remaining = run(
            args,
            "set -e\n" + "\n".join(
                f"if [ -e {shlex.quote(str(QUASAR / relative))} ] || [ -L {shlex.quote(str(QUASAR / relative))} ]; then "
                f"echo STILL_PRESENT {shlex.quote(relative)}; else echo ABSENT {shlex.quote(relative)}; fi"
                for relative in present
            ),
        )
        if "STILL_PRESENT " in remaining:
            raise RuntimeError("post-move verification found quarantined paths still active")
        print(f"Backup ID: {backup_id}")
        print(f"Restore with --restore {backup_id}. No system binaries or startup configuration were changed.")
        return 0
    except (OSError, RuntimeError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
