# Aurora Playout UI Demo (Static)

This is a deploy-anywhere static demo of the unified Aurora console UI.

## What changed
- Operator + Producer use the **same interface** (role toggles permissions in the demo).
- Quick Status panel removed.
- Remote Producers moved to the right column.
- Quick status is now compact badges under the clock (ENGINE / AUDIO / STREAM / SCHED / REMOTE).

## Run locally
```bash
python3 -m http.server 8080
# open http://localhost:8080/
```

Upload the folder contents to any static web host and open `index.html`.
