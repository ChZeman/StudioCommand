// StudioCommand UI Demo — Unified console, static, no build tools required.
// - Operator + Producer share the same layout (role toggles permissions behavior in real product).
// - Queue-style log: finished items disappear; queue refills to keep the column full.
// Keyboard shortcuts: L library, M monitors, S skip, D dump, R reload, Esc close drawers.

const qs = (s) => document.querySelector(s);
const qsa = (s) => Array.from(document.querySelectorAll(s));

const TARGET_LOG_LEN = 12;

const state = {
  role: "operator",
  log: [],
  history: [], // not displayed here; would be in reports/admin
  selectedLogIndex: 0,
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
  apiLive: false,
  lastStatusError: null,
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

async function fetchStatus(){
  try{
    const r = await fetch("/api/v1/status", { cache: "no-store" });
    if(!r.ok) throw new Error(`HTTP ${r.status}`);
    const data = await r.json();

    // Mark API live: stop local simulation from diverging.
    state.apiLive = true;
    state.lastStatusError = null;

    // Map API payload into the UI state shape.
    if(data.now){
      state.now.title = data.now.title || "";
      state.now.artist = data.now.artist || "";
      state.now.dur = data.now.dur || 0;
      state.now.pos = data.now.pos || 0;
    }
    if(Array.isArray(data.log)){
      state.log = data.log.map(it => ({
        ...it,
        // Ensure required fields exist for renderer:
        tag: it.tag ?? "MUS",
        time: it.time ?? "",
        title: it.title ?? "",
        artist: it.artist ?? "",
        state: it.state ?? "queued",
        dur: it.dur ?? "0:00",
        cart: it.cart ?? ""
      }));
    }
    if(Array.isArray(data.producers)){
      state.producers = data.producers;
    }

    // Re-render sections that depend on the payload.
    renderLog();
    renderProducers();

  }catch(err){
    // If the API is temporarily unavailable, keep the demo UI alive.
    state.lastStatusError = String(err);
    // Do not flip apiLive back to false; this prevents jitter if the API blips.
  }
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

function renderLog(){
  const el = qs("#logList");
  el.innerHTML = "";
  state.log.forEach((it, idx) => {
    const row = document.createElement("div");
    row.className = "log-item";
    row.tabIndex = 0;

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
    meta.innerHTML = `<span>Dur: ${it.dur}</span><span>Action: ⋯</span>`;

    main.appendChild(top);
    if(it.artist) main.appendChild(artist);
    main.appendChild(meta);

    row.appendChild(stripe);
    row.appendChild(main);

    if(idx === state.selectedLogIndex) row.style.outline = "2px solid rgba(79,156,255,.55)";
    row.addEventListener("click", () => { state.selectedLogIndex = idx; renderLog(); });

    el.appendChild(row);
  });
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
  if(!state.apiLive){
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

  qs("#libAdd").onclick = () => {
    const it = state.library[state.selectedLibraryIndex];
    if(!it) return;
    const insertAt = Math.min(state.log.length, Math.max(1, state.selectedLogIndex + 1));
    state.log.splice(insertAt, 0, { time:"--:--", tag:"MUS", title:it.title, artist:it.artist, dur:it.dur, state:"queued" });
    toast(`Queued: ${it.title}`);
    refillLog();
    renderLog();
  };
  qs("#libPreview").onclick = () => toast("Preview (demo)");

  qs("#btnMonitors").onclick = () => openDrawer("monitors");
  qs("#closeMonitors").onclick = closeDrawers;

  qs("#btnSkip").onclick = skipNext;
  qs("#btnDump").onclick = dumpNow;
  qs("#btnReload").onclick = reloadLog;
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
    if(key === "escape") closeDrawers();
    if(key === "l" && !e.ctrlKey) openDrawer("library");
    if(key === "m") openDrawer("monitors");
    if(key === "s" && state.role==="operator") skipNext();
    if(key === "d" && state.role==="operator") dumpNow();
    if(key === "r" && state.role==="operator") reloadLog();
    if(e.ctrlKey && key === "f"){ e.preventDefault(); openDrawer("library"); }
  });
}

function simulateStatus(){
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

initData();
wireUI();
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
