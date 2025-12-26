# StudioCommand Playout UI (Static)

The StudioCommand UI is intentionally shipped as **plain static files** (no bundler, no Node build step).
This keeps releases boring and reliable: update the UI by replacing files on disk.

## Demo mode vs Live mode

The UI has two modes:

- **DEMO**: uses locally generated data, so you can click around even if the engine is down.
- **LIVE**: driven by `GET /api/v1/status` and uses backend endpoints for actions.

Why keep DEMO?

- It makes it easy to iterate on layout and interactions without having to run audio/playout.
- It gives you a "safe sandbox" to evaluate operator workflows.

## Queue reordering

Queue items are reorderable in the UI via drag-and-drop (upcoming items only).

Design constraints (on purpose):

- The **currently playing** row is pinned (index `0`).
- Reordering affects only **upcoming** rows (`log[1..]`).

Reasoning:

- In real playout, changing the currently playing item mid-stream is surprising and potentially dangerous.
- Pinned-first behavior lets the backend enforce a simple, safe rule.

The UI calls:

- `POST /api/v1/queue/reorder` with `{ order: ["uuid", ...] }`.

The payload is a full, ordered list of **upcoming** item IDs (strict by design). This makes the API easy to reason about
and avoids ambiguous partial updates.

## Run locally
```bash
python3 -m http.server 8080
# open http://localhost:8080/
```

Upload the folder contents to any static web host and open `index.html`.
