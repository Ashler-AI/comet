#!/usr/bin/env bash
# Register a devbox checkout as a Crew project on this host's own engine, so the
# device appears in the composer's device picker for that project. Runs on the
# devbox after sign-in, with the channel's engine already started. Idempotent:
# Crew ignores a second project for the same device and path.
# Usage: register-project.sh CHANNEL CHECKOUT_PATH
set -euo pipefail
[[ $# == 2 ]] || { echo 'Usage: register-project.sh CHANNEL CHECKOUT_PATH' >&2; exit 1; }
channel="$1"
config="$HOME/.config/crew-devbox/$channel.env"
[[ -f "$config" ]] || { echo "No configured $channel devbox; run configure-host.sh first." >&2; exit 1; }
[[ -d "$2/.git" ]] || { echo "No git checkout at $2." >&2; exit 1; }
set -a; source "$config"; set +a
CHECKOUT="$(realpath "$2")" node --input-type=module -e '
import { readFileSync } from "node:fs";
import { randomUUID } from "node:crypto";
import { basename } from "node:path";
const dir = process.env.COMET_DATA_DIR, path = process.env.CHECKOUT;
const deviceId = readFileSync(`${dir}/device-id`, "utf8").trim();
const token = readFileSync(`${dir}/ipc-token`, "utf8").trim();
const fail = (message) => { console.error(message); process.exit(1); };
setTimeout(() => fail("The Crew engine did not answer within 30 seconds."), 30000).unref();
const socket = new WebSocket(`ws://127.0.0.1:${process.env.COMET_IPC_PORT}`,
  { headers: { Authorization: `Bearer ${token}` } });
socket.addEventListener("error", () => fail("Crew engine IPC is unavailable; start the devbox engine first."));
socket.addEventListener("open", () => socket.send(JSON.stringify({ id: 1, method: "Mutate", params: {
  op: "createSpace", spaceId: randomUUID(), deviceId, path, name: basename(path), gitDetected: true } })));
socket.addEventListener("message", (event) => {
  for (const line of String(event.data).split("\n").filter(Boolean)) {
    const frame = JSON.parse(line);
    if (frame.id !== 1) continue;
    if (frame.err !== undefined) fail(`Crew refused the project: ${JSON.stringify(frame.err)}`);
    console.log(`Registered ${path} as a Crew project on device ${deviceId}.`);
    process.exit(0);
  }
});'
