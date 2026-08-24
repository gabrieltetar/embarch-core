//! `GET /enroll`: a static HTML/JS page, served by Core itself, for
//! enrolling a board's identity — Core's own live hardware I/O
//! (`POST /probes/enroll`, unchanged) and `embarch-topology`'s own
//! file-backed storage underneath it, reached through a browser instead of
//! a separate process.
//!
//! **Why this lives in Core, not a standalone tool:** hardware I/O (opening
//! a debug probe, reading chip memory) and the system-file write that
//! records the result both already happen in Core's process, for the exact
//! same reason `/flash`/`/reset` do — Core is the one process with a live
//! `hw_lock`-guarded connection to the hardware. A separate binary calling
//! the same underlying `embarch_topology::hardware::enroll` function from a
//! second process doesn't share that lock, and makes a human start and stop
//! a process just to do something Core — already running, always — can
//! serve directly. `embarch-core/design.md` §3 decision 25 records this.
//!
//! **Drag-and-drop, not one-at-a-time isolation (§3 decision 15).** Every
//! currently-attached probe is shown at once as a draggable card; dropping
//! one onto a "DUT" or "dev-bench" zone enrolls *that specific probe* (by
//! serial — `POST /probes/enroll`'s `probe_serial` field), rather than
//! requiring a human to unplug down to one board per enrollment. This only
//! works because a human can tell the cards apart (probe identifier/serial)
//! without needing to isolate them — it does **not** resolve the case two
//! boards share an identical probe type (decision 10's own flagged risk);
//! that case still has no answer but physical isolation.
//!
//! **Why this one route skips `auth_middleware`:** the page itself is
//! static markup with nothing secret in it. Every actual read/write it
//! performs (`GET /status`, `GET /probes/enrolled`, `POST /probes/enroll`)
//! still goes through the same bearer-token check as any other caller —
//! the page's own JavaScript just has to attach the header itself, since a
//! plain browser navigation can't. The token is asked for once and kept in
//! this origin's own `localStorage`, never sent anywhere but back to this
//! same Core.

use axum::response::Html;

pub async fn enroll_page_handler() -> Html<&'static str> {
    Html(PAGE)
}

const PAGE: &str = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<title>embarch-core: enroll a board</title>
<style>
  body { font-family: monospace; max-width: 48rem; margin: 2rem auto; }
  fieldset { margin-bottom: 1.5rem; }
  .error { color: #b00; }
  .ok { color: #070; }
  .hint { color: #b8860b; }
  .probes-pool { display: flex; flex-wrap: wrap; gap: 0.6rem; min-height: 3.2rem; padding: 0.5rem; border: 1px dashed #999; }
  .probe-card {
    border: 1px solid #666; border-radius: 6px; padding: 0.4rem 0.6rem; background: #f7f7f7;
    cursor: grab; user-select: none;
  }
  .probe-card.selected { outline: 2px solid #06c; background: #eef6ff; }
  .drop-zones { display: flex; gap: 1rem; margin-top: 0.8rem; }
  .drop-zone {
    flex: 1; min-height: 5rem; border: 2px dashed #aaa; border-radius: 8px; padding: 0.6rem;
    text-align: center; cursor: pointer;
  }
  .drop-zone.dragover { border-color: #06c; background: #eef6ff; }
  .drop-zone.highlight { animation: highlight-pulse 1.5s ease-in-out 3; }
  @keyframes highlight-pulse {
    0%, 100% { border-color: #aaa; box-shadow: none; }
    50% { border-color: #06c; box-shadow: 0 0 0 4px #eef6ff; }
  }
  .drop-zone h3 { margin: 0 0 0.4rem 0; }
  #assign-dialog {
    display: none; position: fixed; top: 30%; left: 50%; transform: translateX(-50%);
    border: 1px solid #333; background: white; padding: 1rem; box-shadow: 0 2px 12px rgba(0,0,0,0.3);
  }
  ul { padding-left: 1.2rem; }
</style>
</head>
<body>
<h1>embarch-core: enroll a board</h1>
<p>Hardware topology only — declares which physical board (by its debug probe) plays which role.
Software topology is fully automatic and has nothing to set here.</p>

<fieldset id="token-box">
  <legend>Core token</legend>
  <label>Bearer token: <input id="token-input" type="password" size="40"></label>
  <button id="token-save">Save</button>
  <span id="token-status"></span>
</fieldset>

<fieldset>
  <legend>Attached probes — drag onto a role below, or click a card then a zone</legend>
  <div class="probes-pool" id="probes-pool">(checking…)</div>
  <p class="hint">Two boards using visibly different probes (different identifier/serial) can both stay
  plugged in — drag each onto its role. Two boards that happen to share the exact same probe type can't be
  told apart this way; unplug down to one of them and enroll it alone instead.</p>
</fieldset>

<div class="drop-zones">
  <div class="drop-zone" id="zone-dev-bench" data-role="dev-bench">
    <h3>dev-bench</h3>
    <div class="hint">drop here</div>
  </div>
  <div class="drop-zone" id="zone-dut" data-role="dut">
    <h3>dut</h3>
    <div class="hint">drop here</div>
  </div>
</div>

<div id="assign-dialog">
  <p>Enroll <b id="assign-probe-label"></b> as <b id="assign-role-label"></b>:</p>
  <label>Chip: <input id="assign-chip" list="chip-suggestions" placeholder="e.g. nRF54L15" required></label>
  <datalist id="chip-suggestions">
    <option value="nRF54L15">
    <option value="esp32c5">
  </datalist>
  <button id="assign-confirm">Enroll</button>
  <button id="assign-cancel">Cancel</button>
  <p id="assign-result"></p>
</div>

<fieldset>
  <legend>Enrolled boards</legend>
  <ul id="enrolled-list"><li><i>(checking…)</i></li></ul>
</fieldset>

<script>
const TOKEN_KEY = "embarch_core_token";
let selectedSerial = null;
let attachedProbes = [];

function getToken() { return localStorage.getItem(TOKEN_KEY) || ""; }
function setToken(t) { localStorage.setItem(TOKEN_KEY, t); }

function authedFetch(path, opts) {
  opts = opts || {};
  opts.headers = Object.assign({}, opts.headers, { "Authorization": "Bearer " + getToken() });
  return fetch(path, opts);
}

function escapeHtml(s) {
  return String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
}

// Polled, not pushed (see the polling loop near the bottom of this script):
// `GET /status` already re-enumerates USB probes fresh on every call — cheap
// descriptor enumeration, not an attach — so a human plugging or unplugging
// a board while this page is open sees it appear/disappear within one poll
// interval, with no new backend endpoint needed. `probe-rs` has no hotplug
// *event* stream to push from instead; polling a call this cheap is the
// simpler answer, not a placeholder for a "real" push mechanism later.
let lastAttachedKey = null;

async function refreshAttached() {
  const pool = document.getElementById("probes-pool");
  try {
    const resp = await authedFetch("/status");
    if (!resp.ok) { pool.innerHTML = '<span class="error">' + resp.status + " " + escapeHtml(await resp.text()) + "</span>"; lastAttachedKey = null; return; }
    const data = await resp.json();
    const probes = data.probes || [];
    // Skip re-rendering (and dropping mid-drag/selection state) when
    // nothing actually changed — every poll would otherwise rebuild the
    // DOM even while a card is mid-drag.
    const key = JSON.stringify(probes.map(p => [p.identifier, p.serial_number]));
    if (key === lastAttachedKey) { return; }
    lastAttachedKey = key;
    attachedProbes = probes;
    if (probes.length === 0) { pool.innerHTML = "<i>no debug probes detected</i>"; return; }
    pool.innerHTML = "";
    for (const p of probes) {
      const card = document.createElement("div");
      card.className = "probe-card" + (p.serial_number === selectedSerial ? " selected" : "");
      card.draggable = true;
      card.dataset.serial = p.serial_number || "";
      card.textContent = p.identifier + " (" + (p.serial_number || "no serial") + ")";
      card.addEventListener("dragstart", (ev) => {
        ev.dataTransfer.setData("text/plain", card.dataset.serial);
      });
      card.addEventListener("click", () => {
        document.querySelectorAll(".probe-card").forEach(c => c.classList.remove("selected"));
        if (selectedSerial === card.dataset.serial) { selectedSerial = null; }
        else { selectedSerial = card.dataset.serial; card.classList.add("selected"); }
      });
      pool.appendChild(card);
    }
  } catch (e) {
    pool.innerHTML = '<span class="error">' + escapeHtml(String(e)) + "</span>";
    lastAttachedKey = null;
  }
}

async function refreshEnrolled() {
  const ul = document.getElementById("enrolled-list");
  try {
    const resp = await authedFetch("/probes/enrolled");
    if (!resp.ok) {
      ul.innerHTML = '<li class="error">' + resp.status + " " + escapeHtml(await resp.text()) + "</li>";
      return;
    }
    const boards = await resp.json();
    if (boards.length === 0) { ul.innerHTML = "<li><i>none enrolled yet</i></li>"; return; }
    ul.innerHTML = boards.map(b =>
      "<li>role <b>" + escapeHtml(b.role) + "</b> — chip " + escapeHtml(b.chip) +
      " — probe " + escapeHtml(b.probe_serial) + " — hardware_id " + escapeHtml(b.hardware_id) + "</li>"
    ).join("");
  } catch (e) {
    ul.innerHTML = '<li class="error">' + escapeHtml(String(e)) + "</li>";
  }
}

function refreshAll() { refreshAttached(); refreshEnrolled(); }

function probeLabel(serial) {
  const p = attachedProbes.find(p => (p.serial_number || "") === serial);
  return p ? (p.identifier + " (" + serial + ")") : serial;
}

function openAssignDialog(serial, role) {
  document.getElementById("assign-probe-label").textContent = probeLabel(serial);
  document.getElementById("assign-role-label").textContent = role;
  document.getElementById("assign-chip").value = "";
  document.getElementById("assign-result").textContent = "";
  const dialog = document.getElementById("assign-dialog");
  dialog.dataset.serial = serial;
  dialog.dataset.role = role;
  dialog.style.display = "block";
}

document.getElementById("assign-cancel").addEventListener("click", () => {
  document.getElementById("assign-dialog").style.display = "none";
});

document.getElementById("assign-confirm").addEventListener("click", async () => {
  const dialog = document.getElementById("assign-dialog");
  const serial = dialog.dataset.serial;
  const role = dialog.dataset.role;
  const chip = document.getElementById("assign-chip").value.trim();
  const result = document.getElementById("assign-result");
  if (!chip) { result.innerHTML = '<span class="error">chip is required</span>'; return; }
  result.textContent = "enrolling…";
  try {
    const resp = await authedFetch("/probes/enroll", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ role, chip, probe_serial: serial }),
    });
    const text = await resp.text();
    if (!resp.ok) { result.innerHTML = '<span class="error">' + resp.status + " " + escapeHtml(text) + "</span>"; return; }
    const board = JSON.parse(text);
    result.innerHTML = '<span class="ok">enrolled \'' + escapeHtml(board.role) + "' — chip " + escapeHtml(board.chip) + "</span>";
    selectedSerial = null;
    setTimeout(() => { dialog.style.display = "none"; refreshAll(); }, 700);
  } catch (e) {
    result.innerHTML = '<span class="error">' + escapeHtml(String(e)) + "</span>";
  }
});

for (const zone of document.querySelectorAll(".drop-zone")) {
  zone.addEventListener("dragover", (ev) => { ev.preventDefault(); zone.classList.add("dragover"); });
  zone.addEventListener("dragleave", () => zone.classList.remove("dragover"));
  zone.addEventListener("drop", (ev) => {
    ev.preventDefault();
    zone.classList.remove("dragover");
    const serial = ev.dataTransfer.getData("text/plain");
    if (serial) openAssignDialog(serial, zone.dataset.role);
  });
  // Click-to-assign fallback for anyone who'd rather select-then-click
  // than drag — same underlying flow, just a different trigger. Useful on
  // touch devices too, where native HTML5 drag-and-drop is inconsistently
  // supported.
  zone.addEventListener("click", () => {
    if (selectedSerial) openAssignDialog(selectedSerial, zone.dataset.role);
  });
}

// `?role=<role>` pre-fill (embarch-topology's own UI links here with it
// from a per-alert "re-enroll this board" action, `milestone-1.md` item 6):
// highlight and scroll to the matching drop zone so a human landing from
// that link immediately sees which physical role needs attention. Read
// client-side, not via a server-side query extractor — this whole page is
// one static `&'static str` response, so there's no per-request Rust code
// to thread a query param through in the first place.
const highlightRole = new URLSearchParams(location.search).get("role");
if (highlightRole) {
  const zone = document.querySelector('.drop-zone[data-role="' + highlightRole.replace(/"/g, "") + '"]');
  if (zone) {
    zone.classList.add("highlight");
    zone.scrollIntoView({ behavior: "smooth", block: "center" });
  }
}

document.getElementById("token-input").value = getToken();
document.getElementById("token-save").addEventListener("click", () => {
  setToken(document.getElementById("token-input").value.trim());
  document.getElementById("token-status").textContent = "saved";
  refreshAll();
});

refreshAll();
// Live updates: see refreshAttached's own comment on why this is a poll,
// not a push. 1.5s is fast enough that plugging a board reads as "the page
// noticed," without hammering Core on a purely local, single-user page.
setInterval(refreshAll, 1500);
</script>
</body>
</html>
"#;
