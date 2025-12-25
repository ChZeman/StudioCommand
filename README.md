# StudioCommand Playout (v0)

Deployable stub for StudioCommand:
- Rust **Axum** engine serving a static UI
- systemd + `/opt/studiocommand` symlink-based layout

## Dev run
```bash
cd engine
cargo run
# open http://127.0.0.1:3000/
```

## Endpoints
- `GET /health` -> `OK`
- `GET /api/v1/system/info` -> version, arch, cpu, load, temp (best-effort)
- `GET /admin/api/v1/updates/status` -> stub status

## Packaging
See `packaging/` for `install.sh`, `studiocommand.service`, and an nginx template.

## One-liner installer (Node-RED style)

```bash
curl -fsSL https://raw.githubusercontent.com/ChZeman/StudioCommand/main/packaging/install-online.sh | \
  sudo bash -s -- --domain studiocommand.yourstation.org --email you@yourstation.org
```

- If `--version` is omitted, the installer will propose the **latest** GitHub Release and ask you to confirm.
- If `--domain` is omitted, the installer will prompt for it.
- If `--email` is omitted, the installer uses a **self-signed** certificate (browser warning) for quick testing.
- StudioCommand is served via **nginx HTTPS on port 8443**.
