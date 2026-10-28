#!/bin/sh
# Wait for the Mini 3 network ADB root after activation, then disable OTA.
# Usage: ./scripts/wait_for_root_disable_ota.sh station-ip
set -eu

if [ "$#" -lt 1 ] || [ -z "$1" ]; then
    echo "usage: $0 station-ip" >&2
    exit 2
fi
STATION_IP=$1
ADB=${ADB:-$(command -v adb || true)}
OTA_SCRIPT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)/yandex-noupdate.sh
SERIAL="$STATION_IP:5555"

if [ -z "$ADB" ]; then
    echo "adb not found in PATH" >&2
    exit 1
fi
if [ ! -r "$OTA_SCRIPT" ]; then
    echo "OTA script not found: $OTA_SCRIPT" >&2
    exit 1
fi

echo "Waiting for root ADB at $SERIAL. Press Ctrl+C to stop."
while :; do
    "$ADB" connect "$SERIAL" >/dev/null 2>&1 || true
    if [ "$("$ADB" -s "$SERIAL" get-state 2>/dev/null || true)" = device ]; then
        DEVICE_UID=$("$ADB" -s "$SERIAL" shell id -u 2>/dev/null | tr -d '\r')
        if [ "$DEVICE_UID" = 0 ]; then
            echo "Root ADB detected at $SERIAL; installing OTA protection."
            "$ADB" -s "$SERIAL" push "$OTA_SCRIPT" /data/local/tmp/yandex-noupdate.sh >/dev/null
            "$ADB" -s "$SERIAL" shell 'mkdir -p /data/opt && cp /data/local/tmp/yandex-noupdate.sh /data/opt/yandex-noupdate.sh && chmod 0700 /data/opt/yandex-noupdate.sh && sh /data/opt/yandex-noupdate.sh disable'
            echo "Verifying OTA status..."
            "$ADB" -s "$SERIAL" shell 'sh /data/opt/yandex-noupdate.sh status'
            echo "OTA protection applied and status checked. No reboot was issued."
            exit 0
        fi
    fi
    sleep 2
done
