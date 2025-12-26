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
- `GET /api/v1/status` -> consolidated UI state (queue/log + now-playing + producers + system)
- `POST /api/v1/queue/reorder` -> reorder upcoming queue items by UUID (playing item is pinned)
- `GET /admin/api/v1/updates/status` -> stub status

### Why `POST /api/v1/queue/reorder` is ID-based (not index-based)

Drag-and-drop must be stable across refreshes and multi-client views. Indices are not stable because
items can be inserted/removed at any time. The engine therefore assigns each queue item a UUID and the
API accepts an ordered list of those UUIDs.

Safety rule: the currently playing item is pinned at index `0` and cannot be reordered.

## Packaging
See `packaging/` for `install.sh`, `studiocommand.service`, and an nginx template.

## One-liner installer

```bash
curl -fsSL https://raw.githubusercontent.com/ChZeman/StudioCommand/main/packaging/install-online.sh | \
  sudo bash -s -- --domain studiocommand.yourstation.org --email you@yourstation.org
```

- If `--version` is omitted, the installer will propose the **latest** GitHub Release and ask you to confirm.
- If `--domain` is omitted, the installer will prompt for it.
- If `--email` is omitted, the installer uses a **self-signed** certificate (browser warning) for quick testing.
- StudioCommand is served via **nginx HTTPS on port 9443**.


## Releases + checksums

- Pushing a tag like `v0.1.0` triggers GitHub Actions to **build x86_64 + aarch64**, generate a merged `sha256sums.txt`,
  and **publish a GitHub Release automatically** with all three files attached.
- `packaging/install-online.sh` will verify `sha256sums.txt` when it is present in the Release.



Releases include the browser UI (`index.html` + assets). GitHub Actions builds the web UI during CI and packages the build output
(`web/dist` for Vite or `web/build` for CRA) into the release tarballs.


## Packaging note (web UI)

Releases include the browser UI (`index.html` + assets) from the `web/` directory. At the moment the UI is packaged as static files
(no Node build step required yet).


## Serving model

StudioCommand uses a split model:

- **Nginx** serves the browser UI directly from disk (`/opt/studiocommand/current/web`).
- The **engine** listens privately on `127.0.0.1:3000` and serves the API (`/api/*`) and WebSockets (`/ws/*`).

This keeps UI deployment simple and avoids coupling the Rust binary to frontend assets.


### v0.1.26 UI note

The queue UI shows Cart + a short ID suffix in the metadata row. This is intentionally verbose so you can validate reorder behavior even when track titles repeat (common in demo data).
