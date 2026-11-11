import { invoke, isTauri } from "@tauri-apps/api/core";
import github from "../crates/foqus-app/assets/github.svg?url";
import "./style.css";

type Page = "overview" | "processes" | "profiles";
interface Process {
  identity: { pid: number; creation_time_100ns: string };
  name: string;
  executable_path: string | null;
  cpu_percent: number | null;
  working_set_bytes: number | null;
  private_bytes?: number | null;
  capability: string;
  state?: string;
}
interface FocusState {
  kind: "idle" | "starting" | "active" | "stopping" | "recovery" | "error";
  started_at_unix?: number;
  target_exited?: boolean;
  message?: string;
}
interface ProcessOutcome {
  identity: { pid: number; creation_time_100ns: string };
  name: string;
  outcome: string;
  message: string | null;
  partial: boolean;
}
interface FocusStatus {
  state: FocusState;
  target: { pid: number; creation_time_100ns: string } | null;
  profile_id?: string | null;
  profile_name: string | null;
  outcomes: ProcessOutcome[];
  errors: string[];
  last_action_errors?: string[];
  applied_changes: number;
}
interface Snapshot {
  processes: Process[];
  memory: [number, number] | null;
  cpu: number | null;
  demo: boolean;
  monitoring_error?: string | null;
}
interface Profile {
  id: string;
  name: string;
  executable_path: string;
  game_priority: "AboveNormal";
  reduce_background_memory: boolean;
  managed_executables: string[];
  excluded_executables: string[];
  restore_when_game_exits: boolean;
}
interface Store {
  schema_version: number;
  selected_profile: string | null;
  profiles: Profile[];
}
const native = isTauri();
let page: Page = "overview";
let snapshot: Snapshot = {
  processes: [],
  memory: null,
  cpu: null,
  demo: !native,
  monitoring_error: null,
};
let store: Store = { schema_version: 1, selected_profile: null, profiles: [] };
let canSave = false,
  saveStatus = "Saved",
  query = "",
  filter = "all",
  sort = "cpu",
  descending = true;
let target = "",
  refreshing = false,
  controllerErrorShown = false,
  controllerError = "",
  errorTimer: ReturnType<typeof setTimeout>;
let focus: FocusStatus = {
  state: { kind: "idle" },
  target: null,
  profile_id: null,
  profile_name: null,
  outcomes: [],
  errors: [],
  applied_changes: 0,
};
let saveQueue = Promise.resolve();
const icons = new Map<string, string | null>();
const cpuHistory: (number | null)[] = [],
  memoryHistory: (number | null)[] = [];
const esc = (text: unknown) =>
  String(text ?? "").replace(
    /[&<>"']/g,
    (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
        c
      ]!,
  );
const $ = <T extends HTMLElement>(selector: string) =>
  document.querySelector<T>(selector);
const fmt = (bytes: number | null) =>
  bytes === null
    ? "—"
    : bytes >= 1073741824
      ? (bytes / 1073741824).toFixed(1) + " GB"
      : Math.round(bytes / 1048576) + " MB";
const pct = (v: number | null, digits = 0) =>
  v === null ? "—" : v.toFixed(digits) + "%";
const selected = () =>
  store.profiles.find((p) => p.id === store.selected_profile) ??
  store.profiles[0];
const key = (p: Process) =>
  p.identity.pid + ":" + p.identity.creation_time_100ns;
const title = (name: string) => name.replace(/\.exe$/i, "");
const protectedProcess = (p: Process) =>
  p.capability === "SystemProtected" || !p.executable_path;
const svg = (body: string) =>
  '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">' +
  body +
  "</svg>";
const navIcons = {
  overview: svg('<path d="M2 12h5l3-9 4 18 3-9h5"/>'),
  processes: svg('<path d="M4 5h16M4 12h16M4 19h16"/>'),
  profiles: svg(
    '<rect x="3" y="3" width="18" height="18" rx="2"/><path d="M8 8h8M8 13h5"/>',
  ),
};
const header = (name: string, subtitle: string) =>
  '<header class="page-heading"><div><h1>' +
  name +
  "</h1><p>" +
  subtitle +
  "</p></div></header>";
function notify(message: unknown) {
  const toast = $("#toast")!;
  toast.textContent = String(message);
  toast.hidden = false;
  clearTimeout(errorTimer);
  errorTimer = setTimeout(() => {
    toast.hidden = true;
  }, 8000);
}

$("#app")!.innerHTML =
  '<div class="shell"><aside class="sidebar"><nav class="nav" aria-label="Main navigation">' +
  (["overview", "processes", "profiles"] as Page[])
    .map(
      (p) =>
        '<button data-page="' +
        p +
        '">' +
        navIcons[p] +
        "<span>" +
        p[0].toUpperCase() +
        p.slice(1) +
        "</span></button>",
    )
    .join("") +
  '</nav><div class="sidebar-bottom"><small>Foqus Manager · v1.0.1</small><button class="github" disabled title="Repository link will be available soon" aria-label="GitHub repository link unavailable"><img src="' +
  github +
  '" alt=""></button></div></aside><div class="workspace"><main class="page" id="page"></main><footer class="footer"><span><i class="dot"></i><span id="focus-footer">Focus Mode idle</span></span><span id="sample-state">Connecting to monitoring…</span></footer></div></div><div id="toast" class="toast" role="alert" hidden></div><dialog id="dialog" class="modal"></dialog>';

function chart(values: (number | null)[], color: string) {
  const points = values.map((v, i) =>
    v === null
      ? null
      : ([((59 - values.length + 1 + i) * 600) / 59, 90 - v * 0.8] as const),
  );
  let path = "",
    pen = false;
  for (const point of points) {
    if (!point) {
      pen = false;
      continue;
    }
    path += (pen ? "L" : "M") + point[0].toFixed(1) + "," + point[1].toFixed(1);
    pen = true;
  }
  return (
    '<svg viewBox="0 0 600 100" preserveAspectRatio="none" class="chart" aria-label="60 second resource history"><path class="chart-grid" d="M0 10H600M0 35H600M0 60H600M0 85H600"/><path class="chart-line" stroke="' +
    color +
    '" d="' +
    path +
    '"/></svg><div class="chart-legend"><span>60 seconds</span><span>Now</span></div>'
  );
}
function render() {
  document.querySelectorAll<HTMLButtonElement>("[data-page]").forEach((b) => {
    b.classList.toggle("active", b.dataset.page === page);
    b.setAttribute("aria-current", b.dataset.page === page ? "page" : "false");
  });
  $("#page")!.innerHTML =
    page === "overview"
      ? overview()
      : page === "processes"
        ? processes()
        : profiles();
  if (page === "overview") updateOverview();
  if (page === "processes") updateProcesses();
}
function overview() {
  return (
    header(
      "Overview",
      "Keep your game in focus. Keep background activity under control.",
    ) +
    '<div id="monitoring-error" class="notice" hidden></div>' +
    '<section class="card focus-card"><div class="focus-main"><div><span class="eyebrow">FOCUSED APPLICATION</span><h2 id="target-name">Choose a running game</h2><small id="target-meta">Select one game instance to prepare its profile.</small><div><select id="target-picker" aria-label="Choose running game"></select></div></div><div class="focus-actions"><button class="primary" id="focus-toggle" data-action="focus-toggle" disabled>Start Focus Mode</button><small id="focus-toggle-note">Process control unavailable</small></div></div><div class="focus-bottom"><span id="focus-state">Focus Mode idle</span><span id="focus-managed">0 apps managed</span><span id="target-policy">No active policy</span></div><div id="focus-error" class="notice" hidden></div></section>' +
    '<div class="overview-grid"><section class="card"><h2>CPU</h2><div class="metric" id="cpu-value">—</div><small>Total system utilization</small><div id="cpu-chart"></div></section><section class="card"><h2>Memory</h2><div class="metric" id="memory-value">—</div><small id="memory-total">System memory in use</small><div id="memory-chart"></div></section><section class="card"><div class="card-title"><h2>Resource breakdown</h2><small>Top CPU consumers</small></div><div id="breakdown"></div><small>Memory values are per-process working sets.</small></section><section class="card"><h2>Your focus policy</h2><div class="policy-row"><span class="muted">Background CPU cap</span><span>Unavailable</span></div><div class="policy-row"><span class="muted">Game priority</span><span>Above normal</span></div><div class="policy-row"><span class="muted">Memory priority</span><span id="policy-memory">No profile selected</span></div><div class="policy-row"><span class="muted">Restore when game exits</span><span id="policy-restore">On</span></div><button class="wide" data-action="edit-target">Edit game profile</button></section></div>'
  );
}
function focusLabel() {
  switch (focus.state.kind) {
    case "starting":
      return "Starting Focus Mode…";
    case "active":
      return focus.state.target_exited
        ? "Game exited · priorities held"
        : "Focus Mode active";
    case "stopping":
      return "Stopping and restoring…";
    case "recovery":
      return "Recovery required";
    case "error":
      return "Focus Mode error";
    default:
      return "Focus Mode idle";
  }
}
function focusActive() {
  return ["starting", "active", "stopping", "recovery"].includes(focus.state.kind);
}
function focusNeedsRecovery() {
  return focus.state.kind === "error" || focus.state.kind === "recovery";
}
function recoveryBlocked() {
  return (
    focus.state.kind === "error" &&
    (focus.state.message ?? "").toLowerCase().includes("recover")
  );
}
function outcomeFor(process: Process) {
  if (focus.state.kind === "idle") return undefined;
  const identity = process.identity;
  return focus.outcomes
    .filter(
      (outcome) =>
        outcome.identity.pid === identity.pid &&
        String(outcome.identity.creation_time_100ns) ===
          identity.creation_time_100ns,
    )
    .at(-1);
}
function policyFor(process: Process) {
  const profile = selected();
  const path = process.executable_path?.toLowerCase();
  if (!profile || !path) return "none";
  if (profile.excluded_executables.some((p) => p.toLowerCase() === path))
    return "excluded";
  if (profile.managed_executables.some((p) => p.toLowerCase() === path))
    return "managed";
  return "none";
}
function syncFocusControls() {
  const toggle = $<HTMLButtonElement>("#focus-toggle");
  if (!toggle) return;
  const active = focusActive();
  const error = focusNeedsRecovery();
  const game = snapshot.processes.find((p) => key(p) === target);
  const profile = game
    ? store.profiles.find(
        (p) =>
          p.executable_path.toLowerCase() ===
          game.executable_path?.toLowerCase(),
      )
    : undefined;
  toggle.disabled = !native || snapshot.demo;
  toggle.textContent = error
    ? "Retry restore"
    : active
      ? "Stop Focus Mode"
      : "Start Focus Mode";
  toggle.title = snapshot.demo
    ? "Process control requires the Windows desktop app"
    : !game && !active
      ? "Choose a running game with a saved profile first"
      : !profile && !active
        ? "Create a profile for this game first"
        : "";
  toggle.disabled = toggle.disabled || (!game && !active) || (!profile && !active);
  const note = $("#focus-toggle-note")!;
  note.textContent = snapshot.demo
    ? "Process control requires Windows"
    : error
      ? recoveryBlocked()
        ? `${focus.state.message} The existing journal was left untouched so original priorities are not lost. Retry restore or Exit Foqus Manager.`
        : focus.state.kind === "error" && focus.state.message
          ? focus.state.message
          : "A previous session still needs recovery"
      : focus.state.kind === "active" && focus.state.target_exited
        ? `${focus.applied_changes} change${focus.applied_changes === 1 ? "" : "s"} stay applied until you stop Focus Mode`
        : active
          ? `${focus.applied_changes} process${focus.applied_changes === 1 ? "" : "es"} changed`
          : "Saved settings apply when Focus Mode starts";
  $("#focus-state")!.textContent = focusLabel();
  $("#focus-managed")!.textContent = `${focus.applied_changes} apps managed`;
  const messages = controllerError ? [controllerError, ...focus.errors] : [...focus.errors];
  const box = $("#focus-error")!;
  box.hidden = !messages.length;
  box.innerHTML = messages.length ? esc(messages.join(" · ")) : "";
  if (messages.length && recoveryBlocked()) {
    const hint = document.createElement("small");
    hint.textContent =
      "The journal was preserved; Foqus will not start a new session and overwrite it.";
    box.appendChild(hint);
  }
  if (messages.length && error) {
    const exit = document.createElement("button");
    exit.textContent = "Exit Foqus Manager";
    exit.className = "quiet danger";
    exit.dataset.action = "exit-app";
    box.appendChild(exit);
  }
  $("#focus-footer")!.textContent = focusLabel();
}

function updateOverview() {
  const picker = $<HTMLSelectElement>("#target-picker")!;
  if (document.activeElement !== picker) {
    const candidates = snapshot.processes
      .filter((p) => !protectedProcess(p))
      .sort((a, b) => a.name.localeCompare(b.name));
    picker.innerHTML =
      '<option value="">Select a running game…</option>' +
      candidates
        .map(
          (p) =>
            '<option value="' +
            key(p) +
            '">' +
            esc(title(p.name)) +
            " · " +
            p.identity.pid +
            "</option>",
        )
        .join("");
    picker.value = target;
  }
  const game = snapshot.processes.find((p) => key(p) === target);
  if (target && !game) target = "";
  const monitor = $("#monitoring-error")!;
  monitor.hidden = !snapshot.monitoring_error;
  monitor.textContent = snapshot.monitoring_error
    ? "Process monitoring unavailable: " + snapshot.monitoring_error
    : "";
  $("#target-name")!.textContent = game
    ? title(game.name)
    : focusActive()
      ? (focus.profile_name ?? "Focus Mode active")
      : "Choose a running game";
  $("#target-meta")!.textContent = game
    ? game.name + " · PID " + game.identity.pid
    : focusActive()
      ? `Managed by Focus Mode · ${focus.applied_changes} process change${focus.applied_changes === 1 ? "" : "s"}`
      : "Select one game instance to prepare its profile.";
  const gameProfile = store.profiles.find(
    (p) =>
      p.executable_path.toLowerCase() === game?.executable_path?.toLowerCase(),
  );
  const profile =
    gameProfile ??
    (focusActive()
      ? store.profiles.find((p) => p.id === focus.profile_id)
      : undefined);
  const applied =
    focusActive() &&
    focus.state.kind !== "recovery" &&
    !!profile &&
    focus.profile_id === profile.id;
  $("#target-policy")!.textContent = applied
    ? "Applied: " + profile.name
    : profile
      ? "Saved profile: " + profile.name
      : "No active policy";
  $("#policy-memory")!.textContent = profile
    ? profile.reduce_background_memory
      ? "Reduced (saved)"
      : "Default (saved)"
    : "No profile selected";
  $("#policy-restore")!.textContent = profile
    ? profile.restore_when_game_exits
      ? "On (saved)"
      : "Off (saved)"
    : "On";
  $("#cpu-value")!.textContent = pct(snapshot.cpu);
  $("#memory-value")!.textContent = fmt(snapshot.memory?.[0] ?? null);
  $("#memory-total")!.textContent = snapshot.memory
    ? "of " + fmt(snapshot.memory[1]) + " installed RAM"
    : "System memory in use";
  $("#cpu-chart")!.innerHTML = chart(cpuHistory, "#06b6d4");
  $("#memory-chart")!.innerHTML = chart(memoryHistory, "#a855f7");
  const top = [...snapshot.processes]
    .sort((a, b) => (b.cpu_percent ?? -1) - (a.cpu_percent ?? -1))
    .slice(0, 3);
  $("#breakdown")!.innerHTML = top.length
    ? top
        .map(
          (p) =>
            '<div class="breakdown-row"><span class="name" title="' +
            esc(p.name) +
            '">' +
            esc(p.name) +
            "</span><span>" +
            pct(p.cpu_percent, 1) +
            "</span><span>" +
            fmt(p.working_set_bytes) +
            "</span></div>",
        )
        .join("")
    : '<div class="empty">Waiting for process snapshot…</div>';
  syncFocusControls();
}
function processes() {
  return (
    header(
      "Processes",
      "See what is using resources and how each application is managed.",
    ) +
    '<div class="toolbar"><input type="search" id="search" placeholder="Search by name or process ID" aria-label="Search processes" value="' +
    esc(query) +
    '"><select id="process-filter" aria-label="Process status filter"><option value="all">All processes</option><option value="candidate">Candidates</option><option value="protected">System protected</option></select><small id="process-count"></small><button class="right" data-action="exceptions">Manage exclusions</button></div><div class="summary"><span class="chip" id="chip-focused">0 focused</span><span class="chip cyan" id="chip-managed">0 managed</span><span class="chip gray" id="chip-excluded">0 excluded</span><small id="resource-summary"></small></div><section class="card table-card"><div class="table-scroll"><table><thead><tr><th>Name</th><th><button class="sort-button" data-sort="cpu">CPU ↓</button></th><th><button class="sort-button" data-sort="memory">Working set</button></th><th>Focus policy</th><th>Status</th></tr></thead><tbody id="process-body"></tbody></table></div></section><aside class="card callout"><p>Saved policy and applied state are shown separately.</p><small>Priority changes are applied only while Focus Mode runs. Managed apps get below-normal CPU priority and optional low memory priority; excluded apps are never changed. Removing a managed app or excluding it restores Foqus changes immediately.</small></aside>'
  );
}
function processRows() {
  const term = query.trim().toLowerCase();
  return snapshot.processes
    .filter(
      (p) =>
        (!term ||
          p.name.toLowerCase().includes(term) ||
          String(p.identity.pid).includes(term)) &&
        (filter === "all" ||
          (filter === "protected"
            ? protectedProcess(p)
            : !protectedProcess(p))),
    )
    .sort((a, b) => {
      const delta =
        sort === "cpu"
          ? (b.cpu_percent ?? -1) - (a.cpu_percent ?? -1)
          : (b.working_set_bytes ?? -1) - (a.working_set_bytes ?? -1);
      return (
        (descending ? delta : -delta) ||
        a.name.localeCompare(b.name) ||
        a.identity.pid - b.identity.pid
      );
    });
}
function updateProcesses() {
  const rows = processRows();
  $<HTMLSelectElement>("#process-filter")!.value = filter;
  $("#process-count")!.textContent =
    rows.length + " / " + snapshot.processes.length + " processes";
  $("#resource-summary")!.textContent =
    "CPU " +
    pct(snapshot.cpu) +
    " · Memory " +
    pct(
      snapshot.memory ? (snapshot.memory[0] / snapshot.memory[1]) * 100 : null,
    );
  $("#process-body")!.innerHTML = rows.length
    ? rows
        .map((p) => {
          const path = p.executable_path ?? "",
            image = icons.get(path),
            policy = policyFor(p),
            outcome = outcomeFor(p),
            partial = outcome?.partial === true,
            focusTarget = focus.target,
            isTarget =
              focusTarget?.pid === p.identity.pid &&
              String(focusTarget.creation_time_100ns) ===
                p.identity.creation_time_100ns;
          const status =
            p.state === "AccessDenied"
              ? "Access denied"
              : protectedProcess(p)
                ? "System protected"
                : outcome
                  ? partial
                    ? "Partial"
                    : outcome.outcome === "Applied"
                      ? isTarget
                        ? "Prioritized"
                        : "Managed"
                      : outcome.outcome === "Excluded"
                        ? "Excluded"
                        : outcome.outcome === "AccessDenied"
                          ? "Access denied"
                          : outcome.outcome === "Unsupported"
                            ? "Unsupported"
                            : outcome.outcome === "Conflict"
                              ? "Restore conflict"
                              : outcome.outcome === "RestoreFailed"
                                ? "Restore failed"
                                : outcome.outcome === "Exited"
                                  ? "Exited"
                                  : outcome.outcome === "Skipped"
                                    ? isTarget
                                      ? outcome.message?.includes("already above normal")
                                        ? "Already compliant"
                                        : "Left unchanged"
                                      : "Skipped"
                                    : outcome.outcome
                  : policy === "managed"
                    ? "Saved · Manage"
                    : policy === "excluded"
                      ? "Saved · Exclude"
                      : "Not applied";
          const badge =
            p.state === "AccessDenied"
              ? "protected"
              : protectedProcess(p)
                ? "protected"
                : partial
                  ? "gray"
                  : outcome?.outcome === "Applied"
                    ? "saved"
                    : policy === "excluded"
                      ? "gray"
                      : outcome?.outcome === "Conflict" ||
                          outcome?.outcome === "RestoreFailed"
                        ? "protected"
                        : "";
          const actions =
            selected() && !protectedProcess(p)
              ? '<div class="policy-actions"><button class="mini" data-policy="manage" data-process="' +
                esc(key(p)) +
                '">Manage</button><button class="mini" data-policy="exclude" data-process="' +
                esc(key(p)) +
                '">Exclude</button><button class="mini" data-policy="auto" data-process="' +
                esc(key(p)) +
                '">Auto</button></div>'
              : "";
          return (
            '<tr><td><div class="process-name"><span class="app-icon" data-icon-path="' +
            esc(path) +
            '">' +
            (image
              ? '<img alt="" src="' + image + '">'
              : esc(p.name[0]?.toUpperCase() ?? "?")) +
            '</span><span class="process-detail"><strong title="' +
            esc(path) +
            '">' +
            esc(title(p.name)) +
            '</strong><small title="' +
            esc(path) +
            '">' +
            (protectedProcess(p) ? "System component" : "Application") +
            " · " +
            p.identity.pid +
            "</small></span></div></td><td>" +
            pct(p.cpu_percent, 1) +
            "</td><td>" +
            fmt(p.working_set_bytes) +
            '</td><td class="muted">' +
            (policy === "managed"
              ? "Managed (saved)"
              : policy === "excluded"
                ? "Excluded (saved)"
                : "Not set") +
            actions +
            '</td><td><span class="badge ' +
            badge +
            '">' +
            esc(status) +
            "</span></td></tr>"
          );
        })
        .join("")
    : '<tr><td colspan="5"><div class="empty">' +
      (snapshot.processes.length
        ? "No matching processes"
        : "Waiting for process snapshot…") +
      "</div></td></tr>";
  const profile = selected();
  $("#chip-focused")!.textContent = focusActive() ? "1 focused" : "0 focused";
  $("#chip-managed")!.textContent =
    (profile?.managed_executables.length ?? 0) + " saved managed";
  $("#chip-excluded")!.textContent =
    (profile?.excluded_executables.length ?? 0) + " saved excluded";
  document.querySelectorAll<HTMLElement>("[data-sort]").forEach((b) => {
    b.textContent =
      (b.dataset.sort === "cpu" ? "CPU" : "Working set") +
      (sort === b.dataset.sort ? (descending ? " ↓" : " ↑") : "");
  });
}
function profiles() {
  const p = selected();
  return (
    header("Profiles", "Choose how Focus Mode behaves for each game.") +
    '<div class="profiles-grid"><aside class="card profile-list"><h2>Game profiles</h2><button class="wide" data-action="add-profile">+ Add game</button><button class="wide quiet" data-action="from-process">From running process</button><div class="profile-items">' +
    store.profiles
      .map(
        (profile) =>
          '<button class="profile-choice ' +
          (profile.id === p?.id ? "active" : "") +
          '" data-profile="' +
          esc(profile.id) +
          '"><strong>' +
          esc(profile.name) +
          "</strong><small>" +
          (profile.id === p?.id ? "Selected profile" : "Saved settings") +
          "</small></button>",
      )
      .join("") +
    '</div><div class="profile-note"><small>Settings apply when you activate Focus Mode.</small></div></aside>' +
    (p
      ? '<section class="card profile-editor"><div class="editor-heading"><div style="min-width:0"><input class="editor-name" id="profile-name" aria-label="Game profile name" value="' +
        esc(p.name) +
        '"><small class="path">' +
        esc(p.executable_path) +
        '</small><small class="path" id="apply-state">' +
        applyStateText(p) +
        '</small></div><span id="save-status" class="badge ' +
        (saveStatus === "Saved" || saveStatus === "Saved · applied"
          ? "saved"
          : "protected") +
        '">' +
        esc(saveStatus) +
        '</span></div><div class="settings-section"><h2>CPU allocation</h2><div class="setting-row"><div class="description">Game priority<small>Give the game preference when CPU is busy. A game already at a higher priority is never lowered.</small></div><select aria-label="Game priority"><option>Above normal</option></select></div><div class="setting-row"><div class="description">Background CPU limit<small>Combined limit for managed background apps.</small></div><button disabled title="CPU limiting is not available in this version">Unavailable</button></div><input class="range" type="range" disabled value="0" aria-label="CPU limiting unavailable"><small>CPU limiting is not available in this version.</small></div><div class="settings-section"><h2>Memory</h2><label class="setting-row"><div class="description">Reduce background memory priority<small>Helps Windows reclaim memory when needed; does not reserve RAM.</small></div><input type="checkbox" class="toggle" id="memory-setting" ' +
        (p.reduce_background_memory ? "checked" : "") +
        '></label></div><div class="settings-section"><div class="card-title"><h2>Exclusions</h2><button data-action="exceptions">Edit exclusions</button></div><div class="exceptions"><span class="exception-tag">Audio services</span><span class="exception-tag">Anti-cheat</span><span class="exception-tag">System</span>' +
        p.excluded_executables
          .map(
            (path) =>
              '<span class="exception-tag" title="' +
              esc(path) +
              '">' +
              esc(path.split(/[\\/]/).pop()) +
              "</span>",
          )
          .join("") +
        p.managed_executables
          .map(
            (path) =>
              '<span class="exception-tag managed" title="Managed: ' +
              esc(path) +
              '">' +
              esc(path.split(/[\\/]/).pop()) +
              "</span>",
          )
          .join("") +
        '</div><small>Managed apps use below-normal CPU priority and optional low memory priority. Excluded apps are never changed and are restored immediately.</small></div><div class="settings-section"><label class="setting-row"><div class="description">Restore settings when the game exits<small>When on, priorities return to their previous values. When off, Focus Mode stays active with the changes applied until you stop it.</small></div><input class="toggle" type="checkbox" id="restore-setting" ' +
        (p.restore_when_game_exits ? "checked" : "") +
        '></label></div><div class="settings-section"><button class="quiet danger" data-action="remove-profile">Remove profile</button></div></section>'
      : '<section class="card empty"><h2>Choose a game profile</h2><p>Add a game executable or select a running process to configure its settings.</p><button class="primary" data-action="add-profile">+ Add game</button></section>') +
    "</div>"
  );
}

function applyStateText(profile: Profile) {
  const applied =
    focusActive() &&
    focus.state.kind !== "recovery" &&
    focus.profile_id === profile.id;
  return applied
    ? "Saved settings are applied to the active Focus Mode session."
    : "Saved settings apply the next time Focus Mode starts.";
}

function save(): Promise<void> {
  if (!canSave) {
    notify(
      "Profiles could not be loaded. Saving is disabled to protect the existing file.",
    );
    return Promise.resolve();
  }
  saveStatus = "Saving…";
  const payload = structuredClone(store);
  const status = $("#save-status");
  if (status) {
    status.textContent = saveStatus;
    status.className = "badge protected";
  }
  saveQueue = saveQueue
    .then(async () => {
      if (native) await invoke("save_profiles", { store: payload });
      saveStatus = native ? "Saved" : "Demo only";
      const current = $("#save-status");
      if (current) {
        current.textContent = saveStatus;
        current.className = "badge saved";
      }
    })
    .catch((e) => {
      saveStatus = "Save error";
      const current = $("#save-status");
      if (current) {
        current.textContent = saveStatus;
        current.className = "badge protected";
      }
      notify("Could not save profile: " + e);
    });
  return saveQueue;
}
async function applyProfileToController(profile: Profile) {
  if (!native) return;
  if (focus.state.kind !== "active" && focus.state.kind !== "starting") return;
  if (focus.profile_id && focus.profile_id !== profile.id) {
    notify(
      "Saved. Focus Mode is running a different profile, so the active session was not changed.",
    );
    return;
  }
  try {
    focus = await invoke<FocusStatus>("apply_profile", { profile });
    const actionErrors = focus.last_action_errors ?? [];
    saveStatus = actionErrors.length
      ? "Saved · needs attention"
      : "Saved · applied";
    if (actionErrors.length) notify(actionErrors.join(" · "));
    render();
  } catch (e) {
    notify("Profile saved, but Focus Mode did not apply it: " + e);
  }
}
async function setProcessPolicy(process: Process, policy: string) {
  const profile = selected();
  if (!profile || !process.executable_path) {
    notify("Select a game profile first.");
    return;
  }
  const path = process.executable_path;
  const matches = (value: string) => value.toLowerCase() === path.toLowerCase();
  profile.managed_executables = profile.managed_executables.filter(
    (value) => !matches(value),
  );
  profile.excluded_executables = profile.excluded_executables.filter(
    (value) => !matches(value),
  );
  if (policy === "manage") profile.managed_executables.push(path);
  if (policy === "exclude") profile.excluded_executables.push(path);
  await save();
  await applyProfileToController(profile);
  render();
}
async function toggleFocus() {
  if (!native) {
    notify("Focus control is available in the Windows desktop app.");
    return;
  }
  if (snapshot.demo) {
    notify("Process control requires Windows. This build is showing demo data.");
    return;
  }
  try {
    if (focus.state.kind === "error" || focus.state.kind === "recovery") {
      focus = await invoke<FocusStatus>("retry_restore");
    } else if (focusActive()) {
      focus = await invoke<FocusStatus>("stop_focus");
    } else {
      const game = snapshot.processes.find((p) => key(p) === target);
      if (!game?.executable_path) {
        notify("Choose a running game first.");
        return;
      }
      const profile = store.profiles.find(
        (p) =>
          p.executable_path.toLowerCase() ===
          game.executable_path?.toLowerCase(),
      );
      if (!profile) {
        notify("Create a profile for this game first.");
        page = "profiles";
        render();
        return;
      }
      focus = await invoke<FocusStatus>("start_focus", {
        pid: game.identity.pid,
        creationTime100ns: game.identity.creation_time_100ns,
        profile,
      });
    }
    if (!focusActive() && saveStatus.startsWith("Saved ·")) saveStatus = "Saved";
    if (focus.errors.length) notify(focus.errors.join(" · "));
    render();
  } catch (e) {
    notify("Focus command failed: " + e);
  }
}
function addProfile(p: Profile) {
  const existing = store.profiles.find(
    (v) => v.executable_path.toLowerCase() === p.executable_path.toLowerCase(),
  );
  if (!existing) store.profiles.push(p);
  store.selected_profile = existing?.id ?? p.id;
  save();
  page = "profiles";
  render();
}
function fromProcess(p: Process) {
  if (!p.executable_path || protectedProcess(p)) return;
  addProfile({
    id: crypto.randomUUID(),
    name: title(p.name),
    executable_path: p.executable_path,
    game_priority: "AboveNormal",
    reduce_background_memory: false,
    managed_executables: [],
    excluded_executables: [],
    restore_when_game_exits: true,
  });
}
function showDialog(mode: "exceptions" | "process" | "remove") {
  const dialog = $<HTMLDialogElement>("#dialog")!;
  const profile = selected();
  if (mode === "exceptions" && !profile) {
    page = "profiles";
    render();
    notify("Add or select a game profile first.");
    return;
  }
  const unique = [
    ...new Map(
      snapshot.processes
        .filter((p) => !protectedProcess(p))
        .map((p) => [p.executable_path, p]),
    ).values(),
  ].sort((a, b) => a.name.localeCompare(b.name));
  dialog.innerHTML =
    '<div class="modal-header"><h2>' +
    (mode === "exceptions"
      ? "Application preferences"
      : mode === "remove"
        ? "Remove profile?"
        : "Add a running application") +
    '</h2><button data-action="close-dialog" aria-label="Close dialog">×</button></div>' +
    (mode === "remove"
      ? "<p>Remove " + esc(profile?.name) + " and its saved settings?</p>"
      : mode === "process"
        ? '<div class="modal-list">' +
          (unique.length
            ? unique
                .map(
                  (p) =>
                    '<button class="exception-row" data-process="' +
                    key(p) +
                    '"><span>' +
                    esc(p.name) +
                    " · " +
                    p.identity.pid +
                    "</span><small>Add profile</small></button>",
                )
                .join("")
            : '<p class="muted">No eligible running applications available.</p>') +
          "</div>"
        : '<small>Preferences are saved for the next Focus Mode session. Current processes are unchanged.</small><div class="modal-list" style="margin-top:12px">' +
          (unique.length
            ? unique
                .map(
                  (p) =>
                    '<label class="exception-row"><span>' +
                    esc(p.name) +
                    '</span><select data-exception="' +
                    esc(p.executable_path) +
                    '" aria-label="Policy for ' +
                    esc(p.name) +
                    '"><option value="candidate">Candidate</option><option value="manage" ' +
                    (profile?.managed_executables.includes(p.executable_path!)
                      ? "selected"
                      : "") +
                    '>Manage</option><option value="exclude" ' +
                    (profile?.excluded_executables.includes(p.executable_path!)
                      ? "selected"
                      : "") +
                    ">Exclude</option></select></label>",
                )
                .join("")
            : '<p class="muted">No eligible running applications available.</p>') +
          "</div>") +
    '<div class="modal-footer"><button data-action="close-dialog">Cancel</button>' +
    (mode === "exceptions"
      ? '<button class="primary" data-action="save-exceptions">Save preferences</button>'
      : mode === "remove"
        ? '<button class="danger" data-action="confirm-remove">Remove profile</button>'
        : "") +
    "</div>";
  dialog.showModal();
}

document.addEventListener("click", async (event) => {
  const el = (event.target as Element).closest<HTMLElement>("button");
  if (!el) return;
  if (el.dataset.policy) {
    const process = snapshot.processes.find(
      (p) => key(p) === el.dataset.process,
    );
    if (process) await setProcessPolicy(process, el.dataset.policy);
    return;
  }
  if (el.dataset.page) {
    page = el.dataset.page as Page;
    render();
    $("#page")!.scrollTop = 0;
    return;
  }
  if (el.dataset.profile) {
    store.selected_profile = el.dataset.profile;
    save();
    render();
    return;
  }
  if (el.dataset.sort) {
    descending = sort === el.dataset.sort ? !descending : true;
    sort = el.dataset.sort;
    updateProcesses();
    return;
  }
  if (el.dataset.process) {
    const p = snapshot.processes.find((p) => key(p) === el.dataset.process);
    if (p) fromProcess(p);
    $<HTMLDialogElement>("#dialog")!.close();
    return;
  }
  try {
    switch (el.dataset.action) {
      case "focus-toggle":
        await toggleFocus();
        break;
      case "add-profile":
        if (!native) {
          notify("Native executable picker is available in the desktop app.");
          return;
        }
        {
          const p = await invoke<Profile | null>("choose_executable");
          if (p) addProfile(p);
        }
        break;
      case "from-process":
        showDialog("process");
        break;
      case "exceptions":
        showDialog("exceptions");
        break;
      case "remove-profile":
        showDialog("remove");
        break;
      case "close-dialog":
        $<HTMLDialogElement>("#dialog")!.close();
        break;
      case "confirm-remove":
        {
          const p = selected();
          if (
            p &&
            focus.state.kind !== "idle" &&
            focus.profile_id === p.id
          ) {
            notify("Stop Focus Mode before removing the profile it is using.");
            return;
          }
          store.profiles = store.profiles.filter((v) => v.id !== p?.id);
          store.selected_profile = store.profiles[0]?.id ?? null;
          save();
          $<HTMLDialogElement>("#dialog")!.close();
          render();
        }
        break;
      case "exit-app":
        if (!native) {
          notify("Exiting the application is available in the desktop app.");
          return;
        }
        await invoke("exit_app");
        break;
      case "edit-target":
        {
          const p = snapshot.processes.find((p) => key(p) === target);
          if (p) fromProcess(p);
          else {
            page = "profiles";
            render();
          }
        }
        break;
      case "save-exceptions":
        {
          const p = selected();
          if (!p) break;
          document
            .querySelectorAll<HTMLSelectElement>("[data-exception]")
            .forEach((select) => {
              const path = select.dataset.exception!;
              p.managed_executables = p.managed_executables.filter(
                (v) => v !== path,
              );
              p.excluded_executables = p.excluded_executables.filter(
                (v) => v !== path,
              );
              if (select.value === "manage") p.managed_executables.push(path);
              if (select.value === "exclude") p.excluded_executables.push(path);
            });
          await save();
          $<HTMLDialogElement>("#dialog")!.close();
          if (page === "profiles") render();
          await applyProfileToController(p);
        }
        break;
    }
  } catch (e) {
    notify(e);
  }
});
document.addEventListener("input", (event) => {
  const el = event.target as HTMLInputElement;
  if (el.id === "search") {
    query = el.value;
    updateProcesses();
  }
});
document.addEventListener("change", (event) => {
  const el = event.target as HTMLInputElement | HTMLSelectElement;
  if (el.id === "target-picker") {
    target = el.value;
    updateOverview();
  }
  if (el.id === "process-filter") {
    filter = el.value;
    updateProcesses();
  }
  const p = selected();
  if (!p) return;
  if (el.id === "profile-name") {
    if (!el.value.trim()) {
      el.value = p.name;
      return;
    }
    p.name = el.value.trim();
    save();
    render();
  }
  if (el.id === "memory-setting") {
    p.reduce_background_memory = (el as HTMLInputElement).checked;
    void save().then(() => applyProfileToController(p));
  }
  if (el.id === "restore-setting") {
    p.restore_when_game_exits = (el as HTMLInputElement).checked;
    void save().then(() => applyProfileToController(p));
  }
});

async function refresh() {
  if (refreshing || document.hidden) return;
  refreshing = true;
  try {
    if (native) snapshot = await invoke<Snapshot>("get_snapshot");
    if (native) {
      try {
        focus = await invoke<FocusStatus>("get_focus_status");
        controllerError = "";
        controllerErrorShown = false;
      } catch (e) {
        controllerError = "Focus controller unavailable: " + e;
        if (!controllerErrorShown) {
          notify("Could not read focus status: " + e);
          controllerErrorShown = true;
        }
      }
    }
    cpuHistory.push(snapshot.cpu);
    memoryHistory.push(
      snapshot.memory ? (snapshot.memory[0] / snapshot.memory[1]) * 100 : null,
    );
    if (cpuHistory.length > 60) cpuHistory.shift();
    if (memoryHistory.length > 60) memoryHistory.shift();
    $("#sample-state")!.textContent = snapshot.monitoring_error
      ? "Monitoring unavailable: " + snapshot.monitoring_error
      : controllerError
        ? "Focus status unavailable"
        : (snapshot.demo ? "Demo data" : "Live monitoring") +
          (focusActive() ? " · Focus Mode active" : " · Read only") +
          " · Updates every 1s";
    $("#focus-footer")!.textContent = focusLabel();
    if (page === "overview") updateOverview();
    if (page === "processes") updateProcesses();
    const paths = [
      ...new Set(
        snapshot.processes
          .map((p) => p.executable_path)
          .filter((p): p is string => !!p),
      ),
    ].filter((p) => !icons.has(p));
    for (let i = 0; i < paths.length; i += 8) {
      await Promise.all(
        paths.slice(i, i + 8).map(async (path) => {
          icons.set(path, null);
          try {
            const icon = await invoke<string | null>("get_icon", { path });
            icons.set(path, icon);
            if (icon)
              document
                .querySelectorAll<HTMLElement>("[data-icon-path]")
                .forEach((el) => {
                  if (el.dataset.iconPath === path)
                    el.innerHTML = '<img alt="" src="' + icon + '">';
                });
          } catch {
            /* Initial-letter fallback remains visible. */
          }
        }),
      );
    }
  } catch (e) {
    $("#sample-state")!.textContent = "Monitoring unavailable";
    notify("Could not refresh processes: " + e);
  } finally {
    refreshing = false;
  }
}
async function boot() {
  render();
  try {
    if (native) store = await invoke<Store>("load_profiles");
    canSave = true;
  } catch (e) {
    notify("Could not load profiles: " + e);
  }
  render();
  await refresh();
  setInterval(() => void refresh(), 1000);
}
void boot();
