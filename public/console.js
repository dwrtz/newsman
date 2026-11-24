const symbols = {
  desk: '<rect x="3" y="3" width="18" height="18" rx="2"/><path d="M3 9h18M9 9v12"/>',
  issue: '<path d="M6 3h8l4 4v14H6zM14 3v5h4M9 12h6M9 15h6"/>',
  audience:
    '<circle cx="10" cy="8" r="3"/><path d="M4 19v-2a6 6 0 0 1 12 0v2M16 5a3 3 0 0 1 0 6m2 3a4 4 0 0 1 2 4"/>',
  plane: '<path d="m3 10 18-7-7 18-3-8-8-3zm8 3 6-6"/>',
  stats: '<path d="M4 20h16M7 16v-4m5 4V5m5 11V9"/>',
  plug: '<path d="m13 6 2-3m2 7 3-3m-8-2 7 7-3 3a5 5 0 0 1-7-7l3-3zm-3 11-4 4"/>',
  tool: '<path d="m8 8-4 4 4 4m8-8 4 4-4 4m-5 2 2-12"/>',
  settings:
    '<path d="M3 6h9m4 0h5M3 12h3m4 0h11M3 18h9m4 0h5"/><circle cx="14" cy="6" r="2"/><circle cx="8" cy="12" r="2"/><circle cx="14" cy="18" r="2"/>',
  branch:
    '<circle cx="6" cy="6" r="2"/><circle cx="18" cy="6" r="2"/><circle cx="12" cy="18" r="2"/><path d="M6 8v1a6 6 0 0 0 6 6m6-7v1a6 6 0 0 1-6 6v1"/>',
  history: '<path d="M3 12a9 9 0 1 1 2 6m-2-4v-5h5m4-2v5l3 2"/>',
  check: '<path d="m5 12 4 4L19 6"/>',
  dot: '<circle cx="12" cy="12" r="3"/>',
  warning: '<path d="m12 3 9 17H3zM12 9v5m0 3h.01"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  arrow: '<path d="M4 12h15m-6-6 6 6-6 6"/>',
  next: '<path d="m9 5 7 7-7 7"/>',
  back: '<path d="m15 5-7 7 7 7"/>',
  search: '<circle cx="10" cy="10" r="6"/><path d="m15 15 5 5"/>',
  copy: '<rect x="8" y="8" width="12" height="13" rx="2"/><path d="M16 8V5a1 1 0 0 0-1-1H5a1 1 0 0 0-1 1v11h4"/>',
  edit: '<path d="m5 16 11-11 3 3L8 19l-4 1zM14 7l3 3"/>',
  mail: '<rect x="3" y="5" width="18" height="14" rx="2"/><path d="m4 6 8 6 8-6"/>',
  shield:
    '<path d="m12 2 8 3v6c0 5-3 8-8 11-5-3-8-6-8-11V5z"/><path d="m9 11 2 2 4-4"/>',
  close: '<path d="m6 6 12 12M18 6 6 18"/>',
  upload: '<path d="M12 16V3m-5 5 5-5 5 5M4 16v4h16v-4"/>',
  book: '<path d="M3 4h6a4 4 0 0 1 3 2 4 4 0 0 1 3-2h6v15h-6a4 4 0 0 0-3 2 4 4 0 0 0-3-2H3z"/>',
  transfer: '<path d="M3 7h15m-5-4 5 4-5 4m8 6H6m5-4-5 4 5 4"/>',
};
export const icon = (name) =>
  `<svg aria-hidden="true" class="icon" viewBox="0 0 24 24">${symbols[name] || symbols.dot}</svg>`;
export const escape = (value) =>
  String(value ?? "").replace(
    /[&<>"']/g,
    (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
        c
      ],
  );
export const human = (value) =>
  String(value ?? "")
    .replace(/[_-]+/g, " ")
    .replace(/^./, (s) => s.toUpperCase());
export const state = {
  session: null,
  registry: null,
  operations: new Map(),
  settings: null,
  route: "",
};
const pendingKeys = new Map();

export async function getJson(path, options = {}) {
  const response = await fetch(path, {
    credentials: "same-origin",
    ...options,
    headers: { Accept: "application/json", ...options.headers },
  });
  const data = await response.json().catch(() => null);
  if (!response.ok || data?.error) {
    const failure = data?.error || {};
    const e = new Error(
      failure.message || `Request failed (${response.status}).`,
    );
    e.code = failure.code || (response.status === 401 ? "unauthorized" : "");
    e.status = response.status;
    throw e;
  }
  if (data === null) throw new Error("The server did not return JSON.");
  return data;
}
export async function ready() {
  const [session, registry] = await Promise.all([
    getJson("/api/session"),
    getJson("/api/registry"),
  ]);
  state.session = session;
  state.registry = registry;
  state.operations = new Map(registry.operations.map((op) => [op.name, op]));
  if (can("settings_get")) {
    try {
      state.settings = await call("settings_get");
    } catch {
      state.settings = null;
    }
  }
}
export function can(name) {
  const operation = state.operations.get(name);
  return Boolean(
    operation &&
      operation.surface === "operator" &&
      (state.session?.scopes ?? []).includes(operation.scope),
  );
}
export async function call(name, input = {}) {
  const op = state.operations.get(name);
  if (!op || !can(name))
    throw new Error("Your account does not have permission for this action.");
  const args = { ...input };
  const cacheKey = JSON.stringify([name, args]);
  if (op.mode === "command" && !args.key) {
    const key = pendingKeys.get(cacheKey) || crypto.randomUUID();
    pendingKeys.set(cacheKey, key);
    args.key = key;
  }
  try {
    const result = await getJson(`/api/ops/${encodeURIComponent(name)}`, {
      method: "POST",
      headers: {
        Accept: "application/json",
        "Content-Type": "application/json",
        "X-CSRF-Token": state.session.csrf,
      },
      body: JSON.stringify(args),
    });
    pendingKeys.delete(cacheKey);
    return result;
  } catch (error) {
    if (
      error.status &&
      error.status < 500 &&
      ![408, 429].includes(error.status)
    )
      pendingKeys.delete(cacheKey);
    throw error;
  }
}
export function whenAllowed(name, args, fallback) {
  return can(name) ? call(name, args) : Promise.resolve(fallback);
}
export const main = () => document.getElementById("content");
export const href = (...parts) =>
  "#/" + parts.map((x) => encodeURIComponent(x)).join("/");
export const link = (label, route, style = "btn", symbol = "") =>
  `<a class="${style}" href="${escape(route)}">${symbol ? icon(symbol) : ""}${escape(label)}</a>`;
export const button = (label, action, style = "btn", symbol = "", attrs = "") =>
  `<button type="button" class="${style}" data-action="${escape(action)}" ${attrs}>${symbol ? icon(symbol) : ""}${escape(label)}</button>`;
export const head = (title, section = "", action = "", description = "") =>
  `<div class="page-head"><div>${section ? `<span class="eyebrow">${escape(section)}</span>` : ""}<h1>${escape(title)}</h1>${description ? `<p class="lead">${escape(description)}</p>` : ""}</div>${action}</div>`;
export const empty = (title, description = "", action = "") =>
  `<div class="empty"><strong>${escape(title)}</strong>${description ? `<p>${escape(description)}</p>` : ""}${action}</div>`;
export const badge = (label) =>
  `<span class="badge ${["active", "ready", "approved", "finished", "succeeded", "draft"].includes(label) ? "on" : ["needs_review", "held", "missing", "failed"].includes(label) ? "warn" : ""}">${escape(human(label))}</span>`;
export const formError = (error) =>
  `<p role="alert" class="notice error">${escape(error?.message || error)}</p>`;
export const dateTime = (v, short = false) => {
  if (!v) return "—";
  const d = new Date(v);
  return Number.isNaN(d.getTime())
    ? "—"
    : new Intl.DateTimeFormat(
        undefined,
        short
          ? { month: "short", day: "numeric" }
          : { dateStyle: "medium", timeStyle: "short" },
      ).format(d);
};
export const timeInput = (v) => {
  const d = new Date(v);
  return new Date(d - d.getTimezoneOffset() * 60000).toISOString().slice(0, 16);
};
export const safeOrigin = () => location.origin;
export function detailPairs(items) {
  return `<dl class="definition">${items.map(([k, v]) => `<dt>${escape(k)}</dt><dd>${escape(v ?? "—")}</dd>`).join("")}</dl>`;
}
export function metricBand(metrics) {
  if (!metrics?.length) return "";
  return `<div class="metric-band">${metrics
    .map((m) => {
      const display =
        typeof m.value === "number"
          ? Intl.NumberFormat(undefined, { maximumFractionDigits: 4 }).format(
              m.value,
            ) + (m.unit === "percent" ? "%" : "")
          : "—";
      return `<div class="metric"><span class="metric-label">${escape(human(m.label))}</span><div class="metric-value" title="${escape(m.unit)}">${escape(display)}</div>${m.unavailable_reason ? `<small>${escape(m.unavailable_reason)}</small>` : ""}</div>`;
    })
    .join("")}</div>`;
}
export function checks(rows) {
  return rows?.length
    ? rows
        .map(
          (c) =>
            `<div class="check-row"><span class="check-icon" data-state="${escape(c.state)}" aria-label="${escape(c.state)}">${icon(c.state === "ready" ? "check" : "warning")}</span><div class="item-lines"><strong>${escape(human(c.name))}</strong><small>${escape(c.detail || human(c.state))}</small></div></div>`,
        )
        .join("")
    : empty("No checks reported");
}
export function field(name, label, value = "", opts = {}) {
  const attrs = `name="${escape(name)}" ${opts.required ? "required" : ""} ${opts.max ? `maxlength="${Number(opts.max)}"` : ""}`;
  let control;
  if (opts.rows)
    control = `<textarea ${attrs} rows="${Number(opts.rows)}" class="${opts.code ? "textarea-code" : ""}" ${opts.code ? 'spellcheck="false"' : ""}>${escape(value)}</textarea>`;
  else if (opts.options)
    control = `<select ${attrs}>${opts.options
      .map((o) => {
        const [k, t] = Array.isArray(o) ? o : [o, human(o)];
        return `<option value="${escape(k)}" ${String(k) === String(value) ? "selected" : ""}>${escape(t)}</option>`;
      })
      .join("")}</select>`;
  else
    control = `<input ${attrs} type="${escape(opts.type || "text")}" value="${escape(value)}" ${opts.min != null ? `min="${Number(opts.min)}"` : ""}>`;
  return `<label class="field ${opts.wide ? "wide" : ""}"><span>${escape(label)}</span>${control}${opts.help ? `<small>${escape(opts.help)}</small>` : ""}</label>`;
}
export function showModal(title, body) {
  const dialog = document.getElementById("modal");
  document.getElementById("dialog-body").innerHTML =
    `<div class="modal-header"><h2 id="dialog-title">${escape(title)}</h2>${button("Close", "close-modal", "btn small")}</div><div class="modal-body">${body}</div>`;
  if (!dialog.open) dialog.showModal();
}
export function closeModal() {
  const dlg = document.getElementById("modal");
  if (dlg.open) dlg.close();
}
let toastTimer;
export function toast(message) {
  const el = document.getElementById("toast");
  el.hidden = false;
  el.textContent = message;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (el.hidden = true), 3500);
}
export function setResult(container, result) {
  container.innerHTML = `<h3>Result</h3><pre>${escape(JSON.stringify(result, null, 2))}</pre>`;
  container.hidden = false;
}
export function describeError(container, error) {
  if (container) {
    container.innerHTML = formError(error);
    container.hidden = false;
  } else showModal("Action unavailable", formError(error));
}
export function safePreviewFrame(target, html) {
  const f = document.createElement("iframe");
  f.className = "frame";
  f.title = "Rendered email";
  f.setAttribute("sandbox", "");
  f.referrerPolicy = "no-referrer";
  f.srcdoc = `<meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; img-src data: https: ${location.origin}/media/;"><meta name="viewport" content="width=device-width">${html}`;
  target.replaceChildren(f);
}
export async function copyText(value) {
  try {
    await navigator.clipboard.writeText(value);
    toast("Copied to clipboard.");
  } catch {
    showModal(
      "Copy text",
      `<label class="field"><span>Select and copy this value</span><textarea readonly rows="3">${escape(value)}</textarea></label>`,
    );
  }
}
