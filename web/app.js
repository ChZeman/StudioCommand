// StudioCommand UI Demo — Unified console, static, no build tools required.
// - Operator + Producer share the same layout (role toggles permissions behavior in real product).
// - Queue-style log: finished items disappear; queue refills to keep the column full.
// Keyboard shortcuts: L library, M monitors, S skip, D dump, R reload, Esc close drawers.

const qs = (s) => document.querySelector(s);
const qsa = (s) => Array.from(document.querySelectorAll(s));

const TARGET_LOG_LEN = 12;

// NOTE: UI_VERSION is purely informational (tooltip on the header).
// The authoritative running version is exposed by the backend at /api/v1/status.
const UI_VERSION = "0.1.40";

const state = {
  role: "operator",
  log: [],
  history: [], // not displayed here; would be in reports/admin
  selectedLogIndex: 0,
  selectedLogId: null,
  library: [],
  selectedLibraryIndex: 0,
  carts: {},
  cartTab: "jingles",
  producers: [
    { name: "Sarah", role: "Producer", onAir: true,  conn: "OK",   jitter: "18ms", loss: "0.7%", camera: false },
    { name: "Emily", role: "Producer", onAir: false, conn: "OK",   jitter: "9ms",  loss: "0.2%", camera: false },
    { name: "Michael", role: "Producer", onAir: false, conn: "WARN", jitter: "45ms", loss: "3.8%", camera: false },
  ],
  now: { title: "", artist: "", dur: 180, pos: 0, ends: "" },

  // The UI can run in 2 modes:
  // - DEMO: uses locally generated data and local-only queue editing.
  // - LIVE: reflects /api/v1/status and uses backend endpoints.
  //
  // We keep this as an explicit string instead of a boolean so we can add
  // additional modes later (e.g. "STALE" when the API is reachable but
  // updates are delayed).
  apiMode: "DEMO",
  lastStatusError: null,
  lastStatusAt: 0,

  // Drag-and-drop interaction state.
  // Rationale: we poll /api/v1/status every second in LIVE mode. Re-rendering the
  // queue while a drag gesture is in progress can cancel the drag in some
  // browsers. We therefore defer re-rendering while dragging.
  isDraggingLog: false,
  pendingRenderAfterDrag: false,

  // Flash-highlight support: after a successful reorder action we capture the
  // previous log order and, on the next /api/v1/status refresh, compute which
  // items changed index. This yields a deterministic "what moved" highlight even
  // when many titles are identical.
  flashArmed: false,
  flashPrevOrder: [], // array of UUID strings (full log order before reorder)
  flashIds: new Set(), // UUID strings to flash on next render

  // Undo support (v0.1.31)
  // We keep a single-level undo for reorder operations. Rationale:
  // - Simpler mental model for operators: "Undo" is always "the last reorder".
  // - Avoids building a full history stack before we have real playout + logs.
  // - Works equally well for drag/drop and ▲/▼ button moves.
  undoPendingUpcoming: null, // array of UUID strings captured before a reorder request
  undoUpcoming: null,        // array of UUID strings available for Ctrl/Cmd+Z
  undoAvailable: false,

  // Streaming output (Icecast) config + status (LIVE mode)
  output: {
    config: null,
    status: null,
    lastAt: 0,
    lastError: null,
    formDirty: false,
  },
};

function pad(n){ return String(n).padStart(2,'0');}
function fmtTime(sec){
  sec = Math.max(0, Math.floor(sec));
  const m = Math.floor(sec/60), s = sec%60;
  return `${pad(m)}:${pad(s)}`;
}
function fmtPosDur(pos, dur){
  const m1 = Math.floor(pos/60), s1 = pos%60;
  const m2 = Math.floor(dur/60), s2 = dur%60;
  return [`${m1}:${pad(s1)}`, `${m2}:${pad(s2)}`];
}
function parseDurToSec(d){
  const parts = String(d).split(":");
  if(parts.length !== 2) return 180;
  const m = parseInt(parts[0], 10) || 0;
  const s = parseInt(parts[1], 10) || 0;
  return m*60 + s;
}
function randFrom(arr){ return arr[Math.floor(Math.random()*arr.length)]; }

function setBadge(sel, level, text){
  const el = qs(sel);
  if(!el) return;
  el.classList.remove("badge-ok","badge-warn","badge-bad");
  el.classList.add(level);
  el.textContent = text;
}

function toast(msg){
  const el = document.createElement("div");
  el.style.position = "fixed";
  el.style.left = "50%";
  el.style.top = "18px";
  el.style.transform = "translateX(-50%)";
  el.style.padding = "10px 12px";
  el.style.borderRadius = "12px";
  el.style.border = "1px solid rgba(36,48,64,.95)";
  el.style.background = "rgba(12,16,20,.92)";
  el.style.boxShadow = "0 18px 40px rgba(0,0,0,.45)";
  el.style.zIndex = 80;
  el.style.fontWeight = 800;
  el.textContent = msg;
  document.body.appendChild(el);
  setTimeout(()=>{ el.style.opacity = "0"; el.style.transition="opacity .2s ease"; }, 1200);
  setTimeout(()=>{ el.remove(); }, 1500);
}

function initData(){
  state.library = [
    { title:"99 Luftballons", artist:"Nena", dur:"3:46", cat:"Gold", code:"MUS-00991" },
    { title:"Amanda", artist:"Boston", dur:"4:06", cat:"A", code:"MUS-01352" },
    { title:"Billie Jean", artist:"Michael Jackson", dur:"4:40", cat:"Gold", code:"MUS-00012" },
    { title:"Don't Stop Believin'", artist:"Journey", dur:"4:11", cat:"A", code:"MUS-01001" },
    { title:"Take On Me", artist:"a-ha", dur:"3:49", cat:"Recurrent", code:"MUS-00444" },
    { title:"Sweet Child O' Mine", artist:"Guns N' Roses", dur:"5:55", cat:"B", code:"MUS-00777" },
  ];

  // Seed queue
  state.log = [
    { time:"15:33", tag:"MUS", title:"Lean On Me", artist:"Club Nouveau", dur:"3:48", state:"playing" },
    { time:"15:37", tag:"MUS", title:"Bette Davis Eyes", artist:"Kim Carnes", dur:"3:30", state:"queued" },
    { time:"15:41", tag:"MUS", title:"Talk Dirty To Me", artist:"Poison", dur:"3:42", state:"queued" },
    { time:"15:45", tag:"EVT", title:"TOH Legal ID", artist:"", dur:"0:10", state:"locked" },
    { time:"15:46", tag:"MUS", title:"Jessie's Girl", artist:"Rick Springfield", dur:"3:07", state:"queued" },
    { time:"15:49", tag:"AD",  title:"Sponsor Break", artist:"2 spots", dur:"1:00", state:"queued" },
  ];

  state.carts = {
    jingles: [
      { title:"Station Sweep", sub:"Dry", key:"F1", len:"0:08" },
      { title:"Top of Hour", sub:"ID", key:"F2", len:"0:10" },
      { title:"Weather Bed", sub:"Bed", key:"F3", len:"1:00" },
      { title:"News Stinger", sub:"SFX", key:"F4", len:"0:03" },
      { title:"Promo In", sub:"Dry", key:"F5", len:"0:05" },
      { title:"Promo Out", sub:"Dry", key:"F6", len:"0:05" },
    ],
    beds: [
      { title:"Soft Bed 1", sub:"Music", key:"F1", len:"2:00" },
      { title:"Soft Bed 2", sub:"Music", key:"F2", len:"2:00" },
      { title:"Upbeat Bed", sub:"Music", key:"F3", len:"2:00" },
      { title:"Sports Bed", sub:"Music", key:"F4", len:"2:00" },
    ],
    sfx: [
      { title:"Whoosh", sub:"SFX", key:"F1", len:"0:01" },
      { title:"Chime", sub:"SFX", key:"F2", len:"0:02" },
      { title:"Applause", sub:"SFX", key:"F3", len:"0:04" },
      { title:"Record Scratch", sub:"SFX", key:"F4", len:"0:02" },
    ],
    ads: [
      { title:"Sponsor A", sub:"30s", key:"F1", len:"0:30" },
      { title:"Sponsor B", sub:"15s", key:"F2", len:"0:15" },
      { title:"PSA", sub:"30s", key:"F3", len:"0:30" },
      { title:"Station Promo", sub:"20s", key:"F4", len:"0:20" },
    ]
  };

  syncNowPlayingFromQueue(true);
  refillLog();
}



function renderApiBadge(){
  const el = qs("#apiBadge");
  if(!el) return;

  const lastOk = state.lastStatusAt || 0;
  const ageMs = lastOk ? (Date.now() - lastOk) : Infinity;

  if(lastOk > 0){
    if(ageMs > 5000){
      el.textContent = "LIVE (STALE)";
      el.classList.remove("badge-live","badge-demo");
      el.classList.add("badge-stale");
      el.title = `LIVE (last update ${Math.round(ageMs/1000)}s ago)`;
    }else{
      el.textContent = "LIVE";
      el.classList.remove("badge-demo","badge-stale");
      el.classList.add("badge-live");
      el.title = "LIVE (driven by /api/v1/status)";
    }
    return;
  }

  el.textContent = "DEMO";
  el.classList.remove("badge-live","badge-stale");
  el.classList.add("badge-demo");
  el.title = state.lastStatusError ? `DEMO (API error: ${state.lastStatusError})` : "DEMO (using local UI data)";
}






async function postAction(path, body){
  // Small helper for operator controls. We keep it simple for now (no auth yet).
  const opts = { method: "POST", headers: { "content-type": "application/json" } };
  if(body !== undefined) opts.body = JSON.stringify(body);
  const r = await fetch(path, opts);
if(!r.ok){
    const t = await r.text().catch(()=> "");
    throw new Error(`HTTP ${r.status} ${t}`);
  }
  return r.json().catch(()=> ({}));
}

async function fetchStatus(){
  try{
    const r = await fetch("/api/v1/status", { cache: "no-store" });
    const ct = (r.headers.get("content-type") || "").toLowerCase();

    if(!r.ok) throw new Error(`HTTP ${r.status}`);

    // Treat only JSON as LIVE.
    if(!ct.includes("application/json")){
      const t = await r.text();
      const preview = t.slice(0,80).replace(/\s+/g," ");
      throw new Error(`Non-JSON response (${ct || "unknown"}): ${preview}...`);
    }

    const data = await r.json();

    // === LIVE MODE DATA FLOW ===
    // We keep the UI intentionally "dumb": /api/v1/status is the single source of
    // truth for queue + producer tiles + now-playing, and the UI simply renders it.
    //
    // This makes later features (drag/drop, remote producers, etc.) easier:
    // - the UI never has to guess the canonical queue order
    // - after any action we can just refetch /api/v1/status
    // - the UI remains usable in DEMO mode when the engine is not running
    state.apiMode = "LIVE";

    // Playout queue (log)
    state.log = Array.isArray(data.log) ? data.log : [];

// If a reorder action just completed, compute which items *actually* moved.
// We do this here (after we ingest the fresh log) so the highlight reflects
// the authoritative backend order.
if(state.flashArmed && Array.isArray(state.flashPrevOrder) && state.flashPrevOrder.length){
  const prev = state.flashPrevOrder;
  const next = state.log.map(it => it.id);
  const moved = [];
  for(let i = 1; i < next.length; i++){
    const id = next[i];
    const pi = prev.indexOf(id);
    if(pi !== -1 && pi !== i){
      moved.push(id);
    }
  }
  state.flashIds = new Set(moved);
  state.flashArmed = false;
  state.flashPrevOrder = [];
}

    // Now-playing (pos/dur are seconds in the API)
    if(data.now && typeof data.now === "object"){
      state.now = {
        title: data.now.title || "",
        artist: data.now.artist || "",
        dur: Number(data.now.dur || 0) || 0,
        pos: Number(data.now.pos || 0) || 0,
        ends: "",
      };
    }

    // Producers: the API uses a slightly different field naming than the DEMO tiles.
    // We normalize here so renderProducers() stays unchanged.
    if(Array.isArray(data.producers)){
      state.producers = data.producers.map(p => ({
        name: p.name || "(unknown)",
        role: p.role || "Producer",
        onAir: !!p.onAir,
        conn: p.connected ? "OK" : "WARN",
        jitter: p.jitter || "—",
        loss: p.loss || "—",
        camera: !!p.camOn,
      }));
    }

    state.status = data; // keep raw around for debugging/inspection
    state.lastStatusAt = Date.now();
    state.lastStatusError = null;

    setApiBadge("LIVE");

    // Fetch streaming output status in LIVE mode.
    // We keep it separate from /api/v1/status so output can evolve independently.
    fetchOutput().catch(()=>{});

    // Re-render immediately unless the operator is currently dragging a log row.
    // (See state.isDraggingLog for rationale.)
    if(state.isDraggingLog){
      state.pendingRenderAfterDrag = true;
    }else{
      renderAll();
    }
}catch(e){
    state.lastStatusError = (e && e.message) ? e.message : String(e);

    const lastOk = state.lastStatusAt || 0;
    const ageMs = lastOk ? (Date.now() - lastOk) : Infinity;

    // If we never had a successful fetch, we fall back to DEMO mode.
    if(lastOk === 0) state.apiMode = "DEMO";

    if(lastOk > 0 && ageMs > 5000){
      setApiBadge("STALE", `LIVE (last update ${Math.round(ageMs/1000)}s ago). Error: ${state.lastStatusError}`);
    }else if(lastOk > 0){
      setApiBadge("LIVE", `LIVE (temporary error: ${state.lastStatusError})`);
    }else{
      setApiBadge("DEMO", `DEMO (API error: ${state.lastStatusError})`);
    }

    // In DEMO mode (or transient error), we still re-render so badges and tiles update.
    if(state.isDraggingLog){
      state.pendingRenderAfterDrag = true;
    }else{
      renderAll();
    }
}
	// end fetchStatus
}

async function fetchOutput(){
  try{
    const r = await fetch("/api/v1/output", { cache: "no-store" });
    if(!r.ok) throw new Error(`HTTP ${r.status}`);
    const data = await r.json();
    state.output.config = data.config || null;
    state.output.status = data.status || null;
    state.output.lastAt = Date.now();
    state.output.lastError = null;
  }catch(e){
    state.output.lastError = (e && e.message) ? e.message : String(e);
  }
  renderStreaming();
}





function stripeFor(st){
  if(st==="playing") return "linear-gradient(180deg, rgba(79,156,255,.95), rgba(143,188,255,.85))";
  if(st==="queued") return "linear-gradient(180deg, rgba(124,255,178,.9), rgba(79,156,255,.55))";
  if(st==="locked") return "linear-gradient(180deg, rgba(255,209,102,.9), rgba(255,209,102,.55))";
  if(st==="skipped") return "linear-gradient(180deg, rgba(255,92,92,.8), rgba(255,92,92,.45))";
  return "linear-gradient(180deg, rgba(127,138,160,.7), rgba(127,138,160,.45))";
}

function makeNextQueueItem(){
  const roll = Math.random();
  if(roll < 0.10) return { time:"--:--", tag:"ID", title:"Station ID", artist:"", dur:"0:10", state:"queued" };
  if(roll < 0.20) return { time:"--:--", tag:"AD", title:"Sponsor Spot", artist:"15s", dur:"0:15", state:"queued" };
  const t = randFrom(state.library);
  return { time:"--:--", tag:"MUS", title:t.title, artist:t.artist, dur:t.dur, state:"queued" };
}

function refillLog(){
  while(state.log.length < TARGET_LOG_LEN) state.log.push(makeNextQueueItem());
}

// --- Queue reordering helpers -------------------------------------------------
// Why IDs?
// Drag-and-drop reordering must be stable across refreshes and multi-user views.
// Indices are not stable (items can be inserted/removed at any time). The backend
// therefore exposes a UUID per item, and the reorder endpoint accepts an ordered
// list of those UUIDs.

function upcomingIdsFromState(){
  // Backend guardrail: the currently playing item is pinned at index 0.
  // Reordering applies only to the *upcoming* items (log[1..]).
  return state.log.slice(1).map(it => it.id);
}

async function postUpcomingReorder(upcomingIds){
  // The backend expects the full upcoming list, in the desired order.
  // (Strictness keeps the API simple and prevents accidental partial moves.)
  return await postAction("/api/v1/queue/reorder", { order: upcomingIds });
}


function armFlashForReorder(){
  // Called immediately before we request a reorder.
  // We snapshot the current order as the "before" picture. After the reorder
  // completes, we refetch /api/v1/status and compare the "after" order to this
  // snapshot to determine which items actually moved.
  state.flashPrevOrder = state.log.map(it => it.id);
  state.flashArmed = true;
}

function renderUndoButton(){
  const b = qs("#btnUndoReorder");
  if(!b) return;
  b.disabled = !state.undoAvailable;
  b.style.opacity = state.undoAvailable ? "1" : ".55";
}

function armUndoForReorder(){
  // Called immediately before we request a reorder. We snapshot the current
  // upcoming order so a later Undo can restore it.
  state.undoPendingUpcoming = upcomingIdsFromState();
}

function commitUndoForReorder(){
  // Called after a reorder request successfully reaches the backend.
  if(!state.undoPendingUpcoming) return;
  state.undoUpcoming = state.undoPendingUpcoming;
  state.undoPendingUpcoming = null;
  state.undoAvailable = true;
  renderUndoButton();
}

function clearUndo(){
  state.undoPendingUpcoming = null;
  state.undoUpcoming = null;
  state.undoAvailable = false;
  renderUndoButton();
}

async function undoLastReorder(){
  if(!state.undoAvailable || !state.undoUpcoming) return;
  try{
    if(state.apiMode === "LIVE"){
      armFlashForReorder();
      await postUpcomingReorder(state.undoUpcoming);
      clearUndo();
      await fetchStatus();
    }else{
      const playing = state.log[0];
      const byId = new Map(state.log.slice(1).map(it => [it.id, it]));
      const newUpcoming = [];
      for(const id of state.undoUpcoming){
        const it = byId.get(id);
        if(it){ newUpcoming.push(it); byId.delete(id); }
      }
      for(const it of byId.values()) newUpcoming.push(it);
      state.log = [playing, ...newUpcoming];
      clearUndo();
      renderLog();
    }
    toast("Undo reorder");
  }catch(err){
    alert(err.message || String(err));
  }
}


function moveWithinUpcoming(upcoming, fromUpcomingIdx, toUpcomingIdx){
  const arr = upcoming.slice();
  const it = arr.splice(fromUpcomingIdx, 1)[0];
  arr.splice(toUpcomingIdx, 0, it);
  return arr;
}

// Like moveWithinUpcoming(), but supports inserting *after* the target row.
// This is required for a good drag-and-drop UX: users expect the drop location
// to reflect whether they released above or below the target item.
function moveWithinUpcomingRelative(upcoming, fromIdx, toIdx, insertAfter){
  let target = toIdx + (insertAfter ? 1 : 0);

  // When moving an item downwards and inserting after, removing the source first
  // shifts the target index left by 1.
  if(fromIdx < target) target -= 1;

  // Allow appending at the end.
  if(target < 0) target = 0;
  if(target > upcoming.length) target = upcoming.length;

  return moveWithinUpcoming(upcoming, fromIdx, target);
}

// Re-render helpers ----------------------------------------------------------
// We keep rendering centralized so LIVE polling can safely trigger updates.
// This also makes it easier to pause queue re-renders during drag gestures.
function renderAll(){
  renderApiBadge();
  renderUndoButton();
  renderLog();
  renderProducers();
  renderLibrary();
  renderCarts();
  // Now-playing/VU are already driven by tickNowPlaying/tickVu, but the initial
  // paint still benefits from re-rendering derived fields.
  setVuUI();
}

function renderLog(){
  const el = qs("#logList");
  el.innerHTML = "";

  // Render rows. We keep this function pure (no network calls, no event wiring).
  // All queue interaction handlers are installed once via event delegation in
  // wireQueueInteractionHandlers(). This avoids brittle per-row listeners which
  // can be lost during frequent LIVE polling re-renders.
  state.log.forEach((it, idx) => {
    const row = document.createElement("div");
    row.className = "log-item";
    row.tabIndex = 0;

    // Stable identifiers used by delegated handlers.
    row.dataset.idx = String(idx);
    row.dataset.id = it.id || "";

    // Drag-and-drop is only allowed for upcoming items (idx > 0). Pinning the
    // playing row avoids surprising behavior and matches backend guardrails.
    row.draggable = (idx > 0);

    // Flash-highlight (one-shot) for items that moved during the last reorder.
    if(idx > 0 && state.flashIds && it.id && state.flashIds.has(it.id)){
      row.classList.add("flash");
    }

    // Selection (for keyboard reorder). IMPORTANT: do not re-render synchronously
    // on click/focus; doing so can swallow the click that also targets ▲/▼ buttons.
    const isSelected = (state.selectedLogId && it.id === state.selectedLogId)
      || (!state.selectedLogId && idx === state.selectedLogIndex);
    if(isSelected){
      row.classList.add("selected");
    }

    const stripe = document.createElement("div");
    stripe.className = "log-stripe";
    stripe.style.background = stripeFor(it.state);

    const main = document.createElement("div");
    main.className = "log-main";

    const top = document.createElement("div");
    top.className = "log-top";

    const tag = document.createElement("span");
    tag.className = "tag";
    tag.textContent = it.tag;

    const time = document.createElement("span");
    time.className = "time";
    time.textContent = it.time;

    const title = document.createElement("span");
    title.className = "title";
    title.textContent = it.title;

    const stateEl = document.createElement("span");
    stateEl.className = "state";
    stateEl.textContent = it.state.toUpperCase();

    top.appendChild(tag); top.appendChild(time); top.appendChild(title); top.appendChild(stateEl);

    const artist = document.createElement("div");
    artist.className = "artist";
    artist.textContent = it.artist || "";

    const meta = document.createElement("div");
    meta.className = "meta";

    // Meta line includes a short ID prefix + index so identical titles remain testable.
    const idShort = (it.id || "").slice(0,8);
    meta.innerHTML = `<span>Dur: ${it.dur}</span><span>Cart: ${it.cart}</span><span>ID: ${idShort}</span><span>#${idx}</span>`;

    // Action buttons (delegated click handling).
    const actions = document.createElement("span");
    actions.className = "log-actions";

    const mkBtn = (label, title, action) => {
      const b = document.createElement("button");
      b.className = "mini";
      b.type = "button";
      b.textContent = label;
      b.title = title;
      b.setAttribute("aria-label", title);
      b.dataset.action = action;
      return b;
    };

    const canEdit = idx > 0;
    const canUp = canEdit && idx > 1;                 // don't move above "playing"
    const canDown = canEdit && idx < state.log.length - 1;

    const up = mkBtn("▲", "Move up", "up");
    const down = mkBtn("▼", "Move down", "down");
    const del = mkBtn("✕", "Remove from queue", "remove");

    if(!canUp) up.disabled = true;
    if(!canDown) down.disabled = true;
    if(!canEdit) del.disabled = true;

    actions.appendChild(up);
    actions.appendChild(down);
    actions.appendChild(del);
    meta.appendChild(actions);

    main.appendChild(top);
    if(it.artist) main.appendChild(artist);
    main.appendChild(meta);

    row.appendChild(stripe);
    row.appendChild(main);

    el.appendChild(row);
  });

  // Clear one-shot flash markers so normal polling does not replay the animation.
  if(state.flashIds && state.flashIds.size){
    state.flashIds = new Set();
  }
}

// Queue interaction handlers --------------------------------------------------
// We install queue interaction once using event delegation. This makes behavior
// robust even under frequent re-rendering from LIVE polling.
function wireQueueInteractionHandlers(){
  if(state._queueHandlersWired) return;
  state._queueHandlersWired = true;

  const logEl = qs("#logList");
  let dragId = null;
  let dropIndicatorRow = null;
  let dropIndicatorAfter = false;

  const clearDropIndicator = () => {
    if(dropIndicatorRow){
      dropIndicatorRow.classList.remove("drop-before");
      dropIndicatorRow.classList.remove("drop-after");
    }
    dropIndicatorRow = null;
    dropIndicatorAfter = false;
  };

  const updateSelectionFromRow = (row) => {
    if(!row) return;
    const idx = parseInt(row.dataset.idx || "-1", 10);
    const id = row.dataset.id || null;
    if(!Number.isFinite(idx) || idx < 0) return;

    state.selectedLogIndex = idx;
    state.selectedLogId = id;

    // Update DOM selection without re-rendering.
    document.querySelectorAll("#logList .log-item.selected").forEach(x => x.classList.remove("selected"));
    row.classList.add("selected");
  };

  // Row selection for keyboard reorder.
  logEl.addEventListener("mousedown", (e) => {
    const row = e.target.closest(".log-item");
    if(row) updateSelectionFromRow(row);
  }, true);
  logEl.addEventListener("focusin", (e) => {
    const row = e.target.closest(".log-item");
    if(row) updateSelectionFromRow(row);
  }, true);

  // Delegated ▲/▼ click handling.
  logEl.addEventListener("click", async (e) => {
    const btn = e.target.closest("button[data-action]");
    if(!btn) return;
    if(btn.disabled) return;

    const row = btn.closest(".log-item");
    if(!row) return;

    updateSelectionFromRow(row);

    const action = btn.dataset.action;
    const id = row.dataset.id || null;
    const absIdx = parseInt(row.dataset.idx || "-1", 10);

    // Never mutate playing row.
    if(!id || !Number.isFinite(absIdx) || absIdx <= 0) return;

    const upcoming = upcomingIdsFromState();
    const upIdx = upcoming.indexOf(id);
    if(upIdx === -1) return;

    try{
      if(action === "up"){
        if(upIdx <= 0) return;
        const newUpcoming = moveWithinUpcoming(upcoming, upIdx, upIdx - 1);
        armUndoForReorder(); armFlashForReorder();
        await postUpcomingReorder(newUpcoming);
        commitUndoForReorder();
        await fetchStatus();
        toast("Moved up");
        return;
      }
      if(action === "down"){
        if(upIdx >= upcoming.length - 1) return;
        const newUpcoming = moveWithinUpcoming(upcoming, upIdx, upIdx + 1);
        armUndoForReorder(); armFlashForReorder();
        await postUpcomingReorder(newUpcoming);
        commitUndoForReorder();
        await fetchStatus();
        toast("Moved down");
        return;
      }
      if(action === "remove"){
        throw new Error("Remove not implemented on backend yet (queue/reorder only)");
      }
    }catch(err){
      alert(err.message || String(err));
    }
  }, true);

  // Delegated drag/drop handling (by id, not by index).
  logEl.addEventListener("dragstart", (e) => {
    const row = e.target.closest(".log-item");
    if(!row) return;

    const absIdx = parseInt(row.dataset.idx || "-1", 10);
    const id = row.dataset.id || null;
    if(!id || !Number.isFinite(absIdx) || absIdx <= 0) return;

    dragId = id;
    state.isDraggingLog = true;
    row.classList.add("dragging");

    e.dataTransfer.effectAllowed = "move";
    e.dataTransfer.setData("text/plain", id);
  }, true);

  logEl.addEventListener("dragend", (e) => {
    const row = e.target.closest(".log-item");
    if(row) row.classList.remove("dragging");
    state.isDraggingLog = false;
    dragId = null;

    clearDropIndicator();

    if(state.pendingRenderAfterDrag){
      state.pendingRenderAfterDrag = false;
      renderAll();
    }
  }, true);

  logEl.addEventListener("dragover", (e) => {
    const row = e.target.closest(".log-item");
    if(!row) return;
    const absIdx = parseInt(row.dataset.idx || "-1", 10);
    if(!Number.isFinite(absIdx) || absIdx <= 0) return;

    e.preventDefault();
    e.dataTransfer.dropEffect = "move";

    // Drop indicator: show whether the item would land before or after the
    // hovered row. This matches common "playlist" UIs and reduces ambiguity.
    const rect = row.getBoundingClientRect();
    const after = e.clientY > (rect.top + rect.height / 2);

    if(dropIndicatorRow !== row){
      clearDropIndicator();
      dropIndicatorRow = row;
    }
    dropIndicatorAfter = after;
    row.classList.toggle("drop-after", after);
    row.classList.toggle("drop-before", !after);
  }, true);

  logEl.addEventListener("drop", async (e) => {
    const row = e.target.closest(".log-item");
    if(!row) return;

    e.preventDefault();

    // Snapshot the indicator before we clear it.
    const insertAfter = (dropIndicatorRow === row) ? dropIndicatorAfter : (() => {
      // Fallback if the indicator wasn't set (e.g. fast drop).
      const rect = row.getBoundingClientRect();
      return e.clientY > (rect.top + rect.height / 2);
    })();

    clearDropIndicator();

    const toId = row.dataset.id || null;
    const toAbsIdx = parseInt(row.dataset.idx || "-1", 10);
    const fromId = e.dataTransfer.getData("text/plain") || dragId;

    if(!fromId || !toId) return;
    if(!Number.isFinite(toAbsIdx) || toAbsIdx <= 0) return;
    if(fromId === toId) return;

    const upcoming = upcomingIdsFromState();
    const fromUpcoming = upcoming.indexOf(fromId);
    const toUpcoming = upcoming.indexOf(toId);
    if(fromUpcoming === -1 || toUpcoming === -1) return;
    if(fromUpcoming === toUpcoming) return;

    try{
      if(state.apiMode === "LIVE"){
        const newUpcoming = moveWithinUpcomingRelative(upcoming, fromUpcoming, toUpcoming, insertAfter);
        armUndoForReorder(); armFlashForReorder();
        await postUpcomingReorder(newUpcoming);
        commitUndoForReorder();
        await fetchStatus();
      }else{
        const fromAbs = fromUpcoming + 1;
        // In DEMO mode we still respect before/after to keep behavior consistent
        // with LIVE mode (the operator should not have to think about modes).
        const toAbs = toUpcoming + 1 + (insertAfter ? 1 : 0);
        const it2 = state.log.splice(fromAbs, 1)[0];
        state.log.splice(toAbs, 0, it2);
        renderLog();
      }
      toast("Reordered");
    }catch(err){
      alert(err.message || String(err));
    }
  }, true);
}

function renderProducers(){
  const el = qs("#producerTiles");
  el.innerHTML = "";
  state.producers.forEach(p => {
    const row = document.createElement("div");
    row.className = "producer";

    const av = document.createElement("div");
    av.className = "avatar";
    av.textContent = p.name.split(" ").map(x=>x[0]).slice(0,2).join("").toUpperCase();

    const main = document.createElement("div");
    main.className = "p-main";
    const name = document.createElement("div");
    name.className = "p-name";
    name.textContent = p.name;
    const sub = document.createElement("div");
    sub.className = "p-sub";
    sub.textContent = `${p.role}`;
    const meter = document.createElement("div");
    meter.className = "p-meter";
    const fill = document.createElement("div");
    fill.style.width = (p.conn === "WARN" ? "38%" : "62%");
    meter.appendChild(fill);

    main.appendChild(name); main.appendChild(sub); main.appendChild(meter);

    const pills = document.createElement("div");
    pills.className = "p-badges";
    const c = document.createElement("span");
    c.className = "pill " + (p.conn === "WARN" ? "warn" : "ok");
    c.textContent = p.conn === "WARN" ? "DEGRADED" : "CONNECTED";
    const a = document.createElement("span");
    a.className = "pill " + (p.onAir ? "onair" : "");
    a.textContent = p.onAir ? "ON AIR" : "OFF AIR";
    const cam = document.createElement("span");
    cam.className = "pill";
    cam.textContent = p.camera ? "CAM ON" : "CAM OFF";
    pills.appendChild(c); pills.appendChild(a); pills.appendChild(cam);

    const stats = document.createElement("div");
    stats.className = "p-stats";
    stats.innerHTML = `<span>Jitter <b>${p.jitter}</b></span><span>Loss <b>${p.loss}</b></span>`;

    // Footer row: status pills + jitter/loss live together beneath the meter.
    // This avoids the "sometimes shifts" behavior that can happen when right-side
    // elements are vertically centered in the main flex row at wider breakpoints.
    const footer = document.createElement("div");
    footer.className = "p-footer";
    footer.appendChild(pills);
    footer.appendChild(stats);

    main.appendChild(footer);

    row.appendChild(av);
    row.appendChild(main);
    el.appendChild(row);
  });
}

function renderCarts(){
  const el = qs("#cartGrid");
  el.innerHTML = "";
  const items = state.carts[state.cartTab] || [];
  items.forEach(it => {
    const c = document.createElement("div");
    c.className = "cart";
    c.innerHTML = `
      <div class="c-title">${it.title}</div>
      <div class="c-sub">${it.sub}</div>
      <div class="c-meta"><span>${it.key}</span><span>${it.len}</span></div>
    `;
    c.addEventListener("click", () => toast(`Cart: ${it.title}`));
    el.appendChild(c);
  });
}

function renderLibrary(){
  const el = qs("#libResults");
  el.innerHTML = "";
  state.library.forEach((it, idx) => {
    const r = document.createElement("div");
    r.className = "row" + (idx === state.selectedLibraryIndex ? " selected" : "");
    r.innerHTML = `
      <div class="r-title">${it.title}</div>
      <div class="r-sub">${it.artist} • ${it.cat}</div>
      <div class="r-meta"><span>${it.code}</span><span>${it.dur}</span></div>
    `;
    r.addEventListener("click", () => { state.selectedLibraryIndex = idx; renderLibrary(); });
    el.appendChild(r);
  });
}

function setClock(){
  const d = new Date();
  qs("#clock").textContent = `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
  const ends = new Date(d.getTime() + (state.now.dur - state.now.pos)*1000);
  qs("#npEnds").textContent = `Ends ${pad(ends.getHours())}:${pad(ends.getMinutes())}:${pad(ends.getSeconds())}`;
}

function openDrawer(which){
  const scrim = qs("#scrim");
  scrim.hidden = false;
  if(which === "library"){
    qs("#libraryDrawer").classList.add("open");
    qs("#libraryDrawer").setAttribute("aria-hidden", "false");
    qs("#libSearch").focus();
  } else {
    qs("#monitorsDrawer").classList.add("open");
    qs("#monitorsDrawer").setAttribute("aria-hidden", "false");
  }
  scrim.onclick = closeDrawers;
}
function closeDrawers(){
  qs("#libraryDrawer").classList.remove("open");
  qs("#monitorsDrawer").classList.remove("open");
  qs("#libraryDrawer").setAttribute("aria-hidden", "true");
  qs("#monitorsDrawer").setAttribute("aria-hidden", "true");
  qs("#scrim").hidden = true;
  qs("#scrim").onclick = null;
}

function syncNowPlayingFromQueue(initial=false){
  const playing = state.log[0];
  if(!playing) return;
  state.now.title = playing.title;
  state.now.artist = playing.artist || "";
  state.now.dur = parseDurToSec(playing.dur);
  if(initial){
    state.now.pos = Math.min(95, Math.max(0, state.now.dur-1));
  } else {
    state.now.pos = 0;
  }
}

function advanceQueue(reason){
  const finished = state.log.shift(); // remove from visible list
  if(finished) state.history.push({ ...finished, finishedAt: new Date().toISOString(), reason: reason || "finished" });

  if(state.log.length > 0) state.log[0].state = "playing";
  for(let i=1;i<state.log.length;i++){
    if(state.log[i].state !== "locked") state.log[i].state = "queued";
  }

  refillLog();
  state.selectedLogIndex = Math.max(0, state.selectedLogIndex - 1);
  syncNowPlayingFromQueue(false);
  renderLog();
}


// VU meter simulation (demo only)
let vu = { l: 0.15, r: 0.18, lpk: 0.25, rpk: 0.28 };
function clamp01(x){ return Math.max(0, Math.min(1, x)); }
function vuToDb(x){
  // x in [0,1] -> roughly -60..0 dB
  const db = -60 + (x*x) * 60;
  return db;
}
function setVuUI(){
  const elL = qs("#vuL"), elR = qs("#vuR");
  const elLpk = qs("#vuLpk"), elRpk = qs("#vuRpk");
  const elDb = qs("#vuDb");
  if(elL) elL.style.width = (vu.l*100).toFixed(1) + "%";
  if(elR) elR.style.width = (vu.r*100).toFixed(1) + "%";
  const ldb = vuToDb(vu.l), rdb = vuToDb(vu.r);
  const db = Math.max(ldb, rdb);
  if(elDb) elDb.textContent = `${db.toFixed(0)} dB`;
  if(elLpk) elLpk.textContent = `${vuToDb(vu.lpk).toFixed(0)}`;
  if(elRpk) elRpk.textContent = `${vuToDb(vu.rpk).toFixed(0)}`;
}
function tickVu(){
  // Random-ish program audio with smoothing and occasional peaks.
  const base = 0.18 + Math.random()*0.28;  // average program level
  const bump = (Math.random() < 0.08) ? (0.25 + Math.random()*0.25) : 0;
  const targetL = clamp01(base + bump + (Math.random()-0.5)*0.08);
  const targetR = clamp01(base + bump + (Math.random()-0.5)*0.08);

  // Smooth
  vu.l = vu.l*0.78 + targetL*0.22;
  vu.r = vu.r*0.78 + targetR*0.22;

  // Peak hold with decay
  vu.lpk = Math.max(vu.lpk*0.98, vu.l);
  vu.rpk = Math.max(vu.rpk*0.98, vu.r);

  setVuUI();
}

function tickNowPlaying(){
  // When connected to the engine API, we do not advance the clock locally.
  // The engine is the source of truth for pos/dur.
  if(state.apiMode !== "LIVE"){
  if(state.log.length === 0){
    state.log.push(makeNextQueueItem());
    state.log[0].state = "playing";
    refillLog();
    syncNowPlayingFromQueue(false);
  }
    state.now.pos += 1;
  if(state.now.pos >= state.now.dur) advanceQueue("finished");
  }

  const rem = state.now.dur - state.now.pos;
  qs("#npRemaining").textContent = fmtTime(rem);
  const [posStr, durStr] = fmtPosDur(state.now.pos, state.now.dur);
  qs("#npPos").textContent = posStr;
  qs("#npDur").textContent = durStr;
  qs("#npTitle").textContent = state.now.title;
  qs("#npArtist").textContent = state.now.artist;
  qs("#npBar").style.width = Math.min(100, Math.max(0, (state.now.pos/state.now.dur)*100)).toFixed(1) + "%";
}

function skipNext(){
  if(state.log.length <= 1) return toast("No next item to skip");
  const skipped = state.log.splice(1, 1)[0];
  if(skipped) state.history.push({ ...skipped, finishedAt: new Date().toISOString(), reason: "skipped" });
  toast(`Skipped: ${skipped?.title || "item"}`);
  refillLog();
  renderLog();
}
function dumpNow(){
  toast("DUMP executed (demo)");
  advanceQueue("dumped");
}
function reloadLog(){
  toast("Reloaded queue (demo)");
  refillLog();
  renderLog();
}

function applyRole(){
  const isProducer = state.role === "producer";
  // In unified UI, producer role hides dangerous playout controls (demo behavior).
  qs("#btnSkip").style.display = isProducer ? "none" : "";
  qs("#btnDump").style.display = isProducer ? "none" : "";
  qs("#btnReload").style.display = isProducer ? "none" : "";
  qs("#npSub").textContent = isProducer ? "Remote Studio • Talkback enabled" : "Automation • Segue: AutoFade 2.0s";
  qs("#modeBadge").textContent = isProducer ? "PRODUCER" : "OPERATOR";
}

function wireUI(){
  qs("#btnLibrary").onclick = () => openDrawer("library");
  qs("#closeLibrary").onclick = closeDrawers;
  qs("#libClear").onclick = () => { qs("#libSearch").value=""; qs("#libSearch").focus(); };

  qs("#libAdd").onclick = async () => {
    const it = state.library[state.selectedLibraryIndex];
    if(!it) return;
    const after = Math.min(state.log.length-1, Math.max(0, state.selectedLogIndex));
    try{
      if(state.apiMode === "LIVE"){
        await postAction("/api/v1/queue/insert", { after, item: { tag:"MUS", title:it.title, artist:it.artist, dur:it.dur, cart: it.code || "" } });
      }else{
        const insertAt = Math.min(state.log.length, Math.max(1, after+1));
        state.log.splice(insertAt, 0, { time:"--:--", tag:"MUS", title:it.title, artist:it.artist, dur:it.dur, state:"queued" });
        refillLog();
        renderLog();
      }
      toast(`Queued: ${it.title}`);
    }catch(err){ alert(err.message || String(err)); }
  };
  qs("#libPreview").onclick = () => toast("Preview (demo)");

  qs("#btnMonitors").onclick = () => openDrawer("monitors");
  qs("#closeMonitors").onclick = closeDrawers;

  const undoBtn = qs("#btnUndoReorder");
  if(undoBtn) undoBtn.onclick = () => undoLastReorder();

        qs("#btnTalk").onclick = () => toast("Talk (push-to-talk in real app)");

  const tba = qs("#btnTalkbackAll");
  if(tba) tba.onclick = () => toast("Talkback All (demo)");
  const inv = qs("#btnInvite");
  if(inv) inv.onclick = () => toast("Invite link copied (demo)");

  qsa(".tab").forEach(b => b.onclick = () => {
    qsa(".tab").forEach(x => x.classList.remove("active"));
    b.classList.add("active");
    state.cartTab = b.dataset.tab;
    renderCarts();
  });

  qsa(".segmented .seg").forEach(b => b.onclick = () => {
    qsa(".segmented .seg").forEach(x=>x.classList.remove("active"));
    b.classList.add("active");
    state.role = b.dataset.role;
    applyRole();
    toast(`Role: ${state.role}`);
  });

  // Clicking badges opens monitors (future: filtered view)
  ["#engineBadge","#audioBadge","#streamBadge","#schedBadge","#remoteBadge"].forEach(sel => {
    const el = qs(sel);
    if(el) el.onclick = () => openDrawer("monitors");
    if(el) el.style.cursor = "pointer";
  });

  document.addEventListener("keydown", (e) => {
    const key = e.key.toLowerCase();

    // Undo last reorder (v0.1.31)
    // We bind Ctrl+Z / Cmd+Z for fast operator recovery after a drag mis-drop.
    // We only consume the shortcut when undo is available.
    if((e.ctrlKey || e.metaKey) && key === "z"){
      if(state.undoAvailable){
        e.preventDefault();
        undoLastReorder();
      }
      return;
    }

    // Keyboard queue reordering (v0.1.33)
    // Alt+Up / Alt+Down moves the selected queue row.
    // We intentionally do NOT rely on DOM focus being perfect across browsers and
    // frequent re-renders. Instead we track selection by stable UUID.
    //
    // Shortcut scheme:
    //  - Alt+Up / Alt+Down (primary)
    //  - Ctrl+Shift+Up / Ctrl+Shift+Down (fallback for environments that intercept Alt+Arrows)
    const isArrow = (key === "arrowup" || key === "arrowdown");
    const wantsMove = isArrow && ((e.altKey) || (e.ctrlKey && e.shiftKey));

    if(wantsMove){
      const dir = (key === "arrowup") ? -1 : 1;

      // Prefer the currently focused row, else fall back to the last selected id.
      const active = document.activeElement;
      const activeId = (active && active.classList && active.classList.contains("log-item"))
        ? (active.dataset.id || null)
        : null;

      const selId = activeId || state.selectedLogId;
      if(!selId) return;

      const idx = state.log.findIndex(it => it.id === selId);
      if(idx <= 0) return; // guard: never move the playing row

      const newIdx = idx + dir;
      if(newIdx <= 0 || newIdx >= state.log.length) return;

      e.preventDefault();

      (async () => {
        try{
          if(state.apiMode === "LIVE"){
            const upcoming = upcomingIdsFromState();
            const fromUpcoming = idx - 1;
            const toUpcoming = newIdx - 1;
            const newUpcoming = moveWithinUpcoming(upcoming, fromUpcoming, toUpcoming);
            armUndoForReorder();
            armFlashForReorder();
            await postUpcomingReorder(newUpcoming);
            commitUndoForReorder();
            // Keep selection stable across refresh.
            state.selectedLogId = selId;
            await fetchStatus();
          }else{
            const it2 = state.log.splice(idx, 1)[0];
            state.log.splice(newIdx, 0, it2);
            renderLog();
          }

          // After the DOM re-renders, focus the moved row so repeated key presses
          // continue to act on the same item.
          requestAnimationFrame(() => {
            const rows = qsa("#logList .log-item");
            const target = rows.find(r => (r.dataset && r.dataset.id === selId));
            if(target) target.focus();
          });

          toast(dir < 0 ? "Moved up" : "Moved down");
        }catch(err){
          alert(err.message || String(err));
        }
      })();
      return;
    }
    if(key === "escape") closeDrawers();
    if(key === "l" && !e.ctrlKey) openDrawer("library");
    if(key === "m") openDrawer("monitors");
    if(key === "s" && state.role==="operator") skipNext();
    if(key === "d" && state.role==="operator") dumpNow();
    if(key === "r" && state.role==="operator") reloadLog();
    if(e.ctrlKey && key === "f"){ e.preventDefault(); openDrawer("library"); }

  });
}


function wireLogDelegatedHandlers(){
  // Back-compat shim: older releases called wireLogDelegatedHandlers().
  // v0.1.38 installs all queue interaction (click + drag/drop + selection)
  // via wireQueueInteractionHandlers().
  wireQueueInteractionHandlers();
}

function simulateStatus(){
  // DEMO-only signal generator.
  // In LIVE mode, producers + queue are authoritative from /api/v1/status.
  if(state.apiMode === "LIVE") return;

  // Occasionally degrade remote (and reflect it in REMOTE badge)
  const p = state.producers[Math.floor(Math.random()*state.producers.length)];
  const degrade = Math.random() < 0.22;

  if(degrade){
    p.conn = "WARN";
    p.jitter = `${30 + Math.floor(Math.random()*80)}ms`;
    p.loss = `${(2 + Math.random()*7).toFixed(1)}%`;

    if(p.camera){
      p.camera = false;
      toast(`${p.name}: Camera disabled to protect audio`);
      qs("#mPriority").textContent = "Audio priority (video off)";
    }

    setBadge("#remoteBadge", "badge-warn", "REMOTE WARN");
    setBadge("#audioBadge", "badge-ok", "AUDIO OK");
    setBadge("#engineBadge", "badge-ok", "ENGINE OK");
    setBadge("#streamBadge", "badge-ok", "STREAM OK");
    setBadge("#schedBadge", "badge-ok", "SCHED OK");

    qs("#mJitter").textContent = p.jitter;
    qs("#mLoss").textContent = p.loss;
  } else {
    state.producers.forEach(x => { x.conn = "OK"; x.jitter = "8–20ms"; x.loss = "0.1–0.9%"; });

    setBadge("#remoteBadge", "badge-ok", "REMOTE OK");
    setBadge("#audioBadge", "badge-ok", "AUDIO OK");
    setBadge("#engineBadge", "badge-ok", "ENGINE OK");
    setBadge("#streamBadge", "badge-ok", "STREAM OK");
    setBadge("#schedBadge", "badge-ok", "SCHED OK");

    qs("#mPriority").textContent = "Normal (A+V)";
    qs("#mJitter").textContent = "18ms";
    qs("#mLoss").textContent = "0.7%";
  }

  renderProducers();
}

function renderStreaming(){
  const st = state.output.status;
  const cfg = state.output.config;

  // DEMO fallback
  if(state.apiMode !== "LIVE" || !st){
    setBadge("#streamBadge", "badge-ok", "STREAM OK");
    const ms = qs("#mStream"); if(ms) ms.textContent = "Connected";
    const mc = qs("#mCodec"); if(mc) mc.textContent = "—";
    const mt = qs("#mMeta"); if(mt) mt.textContent = "OK";
    return;
  }

  // Badge + monitors card
  const stateTxt = (st.state || "stopped").toLowerCase();
  if(stateTxt === "connected"){
    setBadge("#streamBadge", "badge-ok", "STREAM OK");
  }else if(stateTxt === "starting"){
    setBadge("#streamBadge", "badge-warn", "STREAM START");
  }else if(stateTxt === "error"){
    setBadge("#streamBadge", "badge-bad", "STREAM ERR");
  }else{
    setBadge("#streamBadge", "badge-warn", "STREAM OFF");
  }

  const ms = qs("#mStream");
  if(ms){
    ms.textContent = stateTxt === "connected" ? "Connected" : stateTxt;
    ms.className = (stateTxt === "connected") ? "ok" : (stateTxt === "error" ? "bad" : "warn");
  }

  const mc = qs("#mCodec");
  if(mc){
    const c = (st.codec || cfg?.codec || "—").toUpperCase();
    const br = st.bitrate_kbps || cfg?.bitrate_kbps;
    mc.textContent = br ? `${c} ${br}k` : c;
  }

  const mt = qs("#mMeta");
  if(mt){
    mt.textContent = st.last_error ? "ERR" : "OK";
  }

  // Config form (only sync into fields when not dirty)
  if(cfg && !state.output.formDirty){
    const setVal = (id, v) => { const el = qs(id); if(el && document.activeElement !== el) el.value = (v ?? ""); };
    setVal("#outHost", cfg.host);
    setVal("#outPort", String(cfg.port));
    setVal("#outMount", cfg.mount);
    setVal("#outUser", cfg.username);
    const codecEl = qs("#outCodec"); if(codecEl) codecEl.value = cfg.codec || "mp3";
    setVal("#outBitrate", String(cfg.bitrate_kbps || 128));
    const en = qs("#outEnabled"); if(en) en.checked = !!cfg.enabled;
    // Never auto-fill password.
  }

  const stEl = qs("#outStatusText");
  if(stEl){
    const up = typeof st.uptime_sec === "number" ? `${st.uptime_sec}s` : "—";
    const extra = st.last_error ? ` • ${st.last_error}` : "";
    stEl.textContent = `Status: ${stateTxt} • uptime ${up}${extra}`;
  }

  const urlEl = qs("#outListenerUrl");
  if(urlEl && cfg){
    urlEl.textContent = `http://${cfg.host}:${cfg.port}${cfg.mount}`;
  }
}

function wireStreamingControls(){
  // Mark form dirty on edit so we don't overwrite while typing.
  ["#outHost","#outPort","#outMount","#outUser","#outPass","#outCodec","#outBitrate","#outEnabled"].forEach(id => {
    const el = qs(id);
    if(!el) return;
    el.addEventListener("input", ()=>{ state.output.formDirty = true; });
    el.addEventListener("change", ()=>{ state.output.formDirty = true; });
  });

  const btnSave = qs("#btnOutSave");
  const btnStart = qs("#btnOutStart");
  const btnStop = qs("#btnOutStop");

  async function saveConfig(){
    const cfg0 = state.output.config || {};
    const host = (qs("#outHost")?.value || "").trim();
    const port = parseInt((qs("#outPort")?.value || "").trim(), 10) || 0;
    const mount = (qs("#outMount")?.value || "").trim();
    const username = (qs("#outUser")?.value || "").trim();
    const passIn = (qs("#outPass")?.value || "");
    const codec = qs("#outCodec")?.value || "mp3";
    const bitrate_kbps = parseInt((qs("#outBitrate")?.value || "").trim(), 10) || 128;
    const enabled = !!qs("#outEnabled")?.checked;

    const cfg = {
      type: cfg0.type || "icecast",
      host: host || cfg0.host || "seahorse.juststreamwith.us",
      port: port || cfg0.port || 8006,
      mount: mount || cfg0.mount || "/studiocommand",
      username: username || cfg0.username || "source",
      password: passIn.length ? passIn : (cfg0.password || ""),
      codec,
      bitrate_kbps,
      enabled,
      name: cfg0.name || "StudioCommand",
      genre: cfg0.genre || null,
      description: cfg0.description || null,
      public: (cfg0.public === undefined) ? false : cfg0.public,
    };

    await postAction("/api/v1/output/config", cfg);
    state.output.formDirty = false;
    // Clear password field after save for safety.
    const passEl = qs("#outPass"); if(passEl) passEl.value = "";
    await fetchOutput();
    toast("Streaming config saved");
  }

  async function run(btn, fn){
    if(!btn) return;
    const prev = btn.disabled;
    btn.disabled = true;
    try{
      await fn();
    }catch(e){
      console.error(e);
      alert(`Streaming action failed: ${e && e.message ? e.message : e}`);
    }finally{
      btn.disabled = prev;
    }
  }

  if(btnSave) btnSave.addEventListener("click", ()=> run(btnSave, saveConfig));
  if(btnStart) btnStart.addEventListener("click", ()=> run(btnStart, async()=>{ await saveConfig(); await postAction("/api/v1/output/start"); await fetchOutput(); }));
  if(btnStop) btnStop.addEventListener("click", ()=> run(btnStop, async()=>{ await postAction("/api/v1/output/stop"); await fetchOutput(); }));
}


function wireTransportControls(){
  const btnSkip = qs("#btnSkip");
  const btnDump = qs("#btnDump");
  const btnReload = qs("#btnReload");

  async function run(btn, path){
    if(!btn) return;
    const prev = btn.disabled;
    btn.disabled = true;
    try{
      await postAction(path);
      // Pull fresh status immediately so the UI reflects the action without waiting.
      await fetchStatus();
    }catch(e){
      console.error(e);
      alert(`Action failed: ${e && e.message ? e.message : e}`);
    }finally{
      btn.disabled = prev;
    }
  }

  if(btnSkip) btnSkip.addEventListener("click", ()=> run(btnSkip, "/api/v1/transport/skip"));
  if(btnDump) btnDump.addEventListener("click", ()=> run(btnDump, "/api/v1/transport/dump"));
  if(btnReload) btnReload.addEventListener("click", ()=> run(btnReload, "/api/v1/transport/reload"));
}


initData();
setHeaderVersion();
setApiBadge("DEMO");
wireTransportControls();
wireStreamingControls();
fetchStatus();
setInterval(fetchStatus, 1000);
renderApiBadge();
wireUI();
wireLogDelegatedHandlers();
applyRole();
renderLog();
renderLibrary();
renderCarts();
renderProducers();
setClock();
setVuUI();

setInterval(setClock, 1000);
setInterval(tickNowPlaying, 1000);
setInterval(tickVu, 120);
setInterval(simulateStatus, 5000);

function setApiBadge(mode, detail){
  const el = qs("#apiBadge");
  if(!el) return;
  el.classList.remove("badge-live","badge-demo","badge-stale");
  if(mode === "LIVE"){
    el.textContent = "LIVE";
    el.classList.add("badge-live");
    el.title = detail || "LIVE (driven by /api/v1/status)";
  }else if(mode === "STALE"){
    el.textContent = "LIVE (STALE)";
    el.classList.add("badge-stale");
    el.title = detail || "LIVE but updates are stale";
  }else{
    el.textContent = "DEMO";
    el.classList.add("badge-demo");
    el.title = detail || "DEMO (using local UI data)";
  }
}

function setHeaderVersion(){
  const h = qs("#hdrTitle");
  if(h) h.title = `StudioCommand UI v${UI_VERSION}`;
}
