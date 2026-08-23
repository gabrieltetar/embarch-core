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
  body { font-family: monospace; max-width: 42rem; margin: 2rem auto; }
  fieldset { margin-bottom: 1.5rem; }
  .error { color: #b00; }
  .ok { color: #070; }
  .hint { color: #b8860b; }
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
  <legend>Currently attached</legend>
  <div id="attached">(checking…)</div>
</fieldset>

<fieldset>
  <legend>Enrolled boards</legend>
  <ul id="enrolled-list"><li><i>(checking…)</i></li></ul>
</fieldset>

<fieldset>
  <legend>Enroll the currently-attached probe</legend>
  <form id="enroll-form">
    <label>Role:
      <select id="role">
        <option value="dev-bench">dev-bench</option>
        <option value="dut">dut</option>
      </select>
    </label>
    <label>Chip: <input id="chip" list="chip-suggestions" placeholder="e.g. nRF54L15" required></label>
    <datalist id="chip-suggestions">
      <option value="nRF54L15">
      <option value="esp32c5">
    </datalist>
    <button type="submit">Enroll</button>
  </form>
  <p id="enroll-result"></p>
</fieldset>

<script>
const TOKEN_KEY = "embarch_core_token";

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

async function refreshAttached() {
  const el = document.getElementById("attached");
  try {
    const resp = await authedFetch("/status");
    if (!resp.ok) { el.innerHTML = '<span class="error">' + resp.status + " " + escapeHtml(await resp.text()) + "</span>"; return; }
    const data = await resp.json();
    const probes = data.probes || [];
    if (probes.length === 0) { el.textContent = "no debug probes detected"; return; }
    el.innerHTML = probes.map(p => escapeHtml(p.identifier + " (serial " + (p.serial_number || "none") + ")")).join(", ");
    if (probes.length !== 1) {
      el.innerHTML += '<br><span class="hint">enrolling needs exactly one probe attached — unplug down to one board, then submit below</span>';
    }
  } catch (e) {
    el.innerHTML = '<span class="error">' + escapeHtml(String(e)) + "</span>";
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

document.getElementById("token-input").value = getToken();
document.getElementById("token-save").addEventListener("click", () => {
  setToken(document.getElementById("token-input").value.trim());
  document.getElementById("token-status").textContent = "saved";
  refreshAll();
});

document.getElementById("enroll-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const role = document.getElementById("role").value;
  const chip = document.getElementById("chip").value;
  const result = document.getElementById("enroll-result");
  result.textContent = "enrolling…";
  try {
    const resp = await authedFetch("/probes/enroll", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ role, chip }),
    });
    const text = await resp.text();
    if (!resp.ok) { result.innerHTML = '<span class="error">' + resp.status + " " + escapeHtml(text) + "</span>"; return; }
    const board = JSON.parse(text);
    result.innerHTML = '<span class="ok">enrolled \'' + escapeHtml(board.role) + "' — chip " + escapeHtml(board.chip) +
      ", hardware_id " + escapeHtml(board.hardware_id) + "</span>";
    refreshAll();
  } catch (e) {
    result.innerHTML = '<span class="error">' + escapeHtml(String(e)) + "</span>";
  }
});

refreshAll();
</script>
</body>
</html>
"#;
