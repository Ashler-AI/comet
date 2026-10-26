# Namespace provisioning

Use the official `devbox` CLI and interactive `devbox login`. Inspect `devbox create --help`, image/Blueprint choices and current shapes. Target 16 CPU / 32 GB: the current Linux `l` shape bursts to 16 vCPU and has 32 GB; verify rather than assuming labels are permanent. Do not shrink a reused larger machine solely to match the default.

List existing owner-matched machines first with `devbox list --output json`. On an uncertain create response, inspect the inventory rather than creating another billable machine.

```bash
devbox create --name "$DEVBOX_NAME" --purpose "$OWNER_NAME's Ashler development machine" \
  --image builtin:agents --size "$VERIFIED_SIZE" --platform linux/amd64 \
  --persistent --access_mode private --auto_stop_idle_timeout 1h \
  --checkout github.com/Ashler-AI/ashler-platform
devbox configure-ssh "$DEVBOX_NAME"
```

Use the returned immutable ID, not the display name. Keep the checkout at `/workspaces/ashler-platform`, not Namespace's nonpersistent home. Linux uses Docker/kind/Tilt, not OrbStack. Only transfer local GitHub auth through `--setup_github` after explicit authorization; otherwise log in on the host interactively. Install from a verified immutable release and fetch its workflow/scripts from that manifest's source commit; an existing production Namespace setup does not require the new staging bootstrap or a profile migration.

Transfer verified artifacts/scripts with `devbox upload`. Setup forwarding is one supervised process:

```bash
devbox port-forward "$DEVBOX_ID" \
  --ports "$CALLBACK_PORT:$CALLBACK_PORT,8085:8085,$WEB_LOCAL_PORT:3000,$TILT_LOCAL_PORT:10350"
```

Check the actual local ports immediately before starting. Stop setup forwarding before testing automatic forwards only when the selected verified release supports them. Namespace's declared interactive `dev-shell` runs the shared autostart `.bashrc` hook at boot; the helper always addresses the same dedicated tmux server despite inherited `TMUX` and changed window indices. Native Namespace support must be exercised end to end before it is described as verified; the existing released workflow remains valid independently.

The diagnostic wake equivalent is `devbox exec "$DEVBOX_ID" -- sh -c 'exec "$HOME/.local/bin/crew-devbox-autostart" "$CHANNEL"'` with the selected channel substituted on the host. Normal use is Crew Wake/pending send, not a manual CLI command.

Sources: https://namespace.so/docs/devbox/lifecycle, https://namespace.so/docs/guides/devbox/long-running-tasks, https://namespace.so/pricing.
