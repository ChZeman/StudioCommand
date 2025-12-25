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
