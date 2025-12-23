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
