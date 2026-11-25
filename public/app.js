import {
  state,
  ready,
  can,
  escape as e,
  icon,
  button,
  link,
  href,
  main,
  toast,
  head,
  empty,
  formError,
  closeModal,
} from "./console.js";
import {
  overview,
  issuesView,
  compose,
  audience,
  person,
  delivery,
  reports,
  connections,
  settings,
  operators,
} from "./views.js";
import { toolsPage, transfers } from "./tools.js";

const menus = [
  [
    "Publishing",
    [
      ["overview", "Overview", "desk", "dashboard_get"],
      ["issues", "Messages", "issue", "issues_list"],
      ["audience", "Audience", "audience", "subscribers_list"],
      ["delivery", "Delivery", "plane", "campaigns_list"],
      ["reports", "Reports", "stats", "report_get"],
    ],
  ],
  [
    "Workspace",
    [
      ["connections", "MCP connections", "plug", null],
      ["tools", "Operation explorer", "tool", null],
      ["transfers", "File transfers", "transfer", "file_read"],
      ["settings", "Settings", "settings", "settings_get"],
    ],
  ],
];
const byRoute = {
  overview: () => overview(),
  issues: (_, p) => issuesView(p),
  compose: (id) => compose(id),
  audience: (id, p) => (id ? person(id) : audience(p)),
  delivery: (id, p) => delivery(id, p),
  reports: (_, p) => reports(p),
  connections: () => connections(),
  settings: () => settings(),
  operators: (_, p) => operators(p),
  tools: (name, p) => toolsPage(name, p),
  transfers: () => transfers(),
};
const labels = {
  overview: "Overview",
  issues: "Messages",
  compose: "Message editor",
  audience: "Audience",
  delivery: "Delivery",
  reports: "Reports",
  connections: "MCP connections",
  settings: "Settings",
  operators: "Operator allowlist",
  tools: "Operation explorer",
  transfers: "File transfers",
};
let serial = 0,
  renderedOnce = false;
const side = document.getElementById("sidebar"),
  menu = document.getElementById("mobile-menu");
menu.addEventListener("click", () => {
  const isOpen = side.classList.toggle("open");
  menu.setAttribute("aria-expanded", String(isOpen));
});
document.addEventListener("click", (ev) => {
  if (ev.target.closest('[data-action="close-modal"]')) closeModal();
});
document.getElementById("modal").addEventListener("click", (ev) => {
  const rect = ev.currentTarget.getBoundingClientRect();
  if (
    ev.target === ev.currentTarget &&
    (ev.clientX < rect.left ||
      ev.clientX > rect.right ||
      ev.clientY < rect.top ||
      ev.clientY > rect.bottom)
  )
    closeModal();
});
function sidebar(tab) {
  const p = state.settings?.settings;
  const card = document.getElementById("publication-select");
  card.hidden = false;
  card.innerHTML = `<div class="pub-avatar">${icon("book")}</div><div><strong>${e(p?.publication_name || "Workspace")}</strong><small>Publishing workspace</small></div>`;
  document.getElementById("navigation").innerHTML = menus
    .map(
      ([title, items]) =>
        `<div class="nav-group"><span class="nav-title">${title}</span><div>${items
          .filter(([, , , perm]) => !perm || can(perm))
          .map(
            ([path, label, sym]) =>
              `<a class="nav-item ${tab === path ? "active" : ""}" href="${href(path)}" ${tab === path ? 'aria-current="page"' : ""}>${icon(sym)}${label}</a>`,
          )
          .join("")}</div></div>`,
    )
    .join("");
  const email = state.session.email || "Operator";
  const local = email.split("@")[0];
  const badge = local
    .split(/[._-]/)
    .slice(0, 2)
    .map((x) => x.charAt(0).toUpperCase())
    .join("");
  document.getElementById("identity").innerHTML =
    `<span class="identity-avatar">${e(badge || "O")}</span><div class="identity-label"><strong title="${e(email)}">${e(email)}</strong><small>Operator account</small></div>` + (state.session.email_login ? '<form method="post" action="/login/logout"><button class="text-button">Sign out</button></form>' : "");
  document.getElementById("top-right").innerHTML =
    `<a class="text-button" href="${href("connections")}">${icon("plug")} MCP</a>${state.session.local ? '<span class="context-pill"><span class="context-dot"></span>Local instance</span>' : ""}`;
}
function showFailure(err) {
  main().innerHTML =
    head(
      err.status === 401 ? "Sign in required" : "Console unavailable",
      "Access",
    ) +
    `<section class="form-panel">${formError(err)}${button("Try again", "retry", "btn primary")}</section>`;
  document
    .querySelector('[data-action="retry"]')
    ?.addEventListener("click", () => location.reload());
}
function setPage(view) {
  main().innerHTML = view.html;
  view.mount?.();
  main().setAttribute("aria-busy", "false");
  if (renderedOnce) main().focus({ preventScroll: true });
  renderedOnce = true;
}
async function render() {
  const seq = ++serial;
  const raw = location.hash.slice(1) || "/overview";
  const [route, query = ""] = raw.split("?"),
    parts = route.split("/").filter(Boolean).map(decodeURIComponent);
  const tab = parts[0] || "overview",
    sub = parts[1] || "",
    params = new URLSearchParams(query),
    fn = byRoute[tab];
  document.getElementById("current-page").textContent =
    labels[tab] || "Unknown page";
  document.title = (labels[tab] || "Workspace") + " · newsman";
  if (!state.session) return;
  side.classList.remove("open");
  menu.setAttribute("aria-expanded", "false");
  sidebar(
    tab === "compose" ? "issues" : tab === "operators" ? "settings" : tab,
  );
  main().setAttribute("aria-busy", "true");
  try {
    const view = fn
      ? await fn(sub, params)
      : {
          html:
            head("Page unavailable", "Workspace") +
            empty(
              "Unknown page",
              "",
              link("Back to overview", href("overview"), "btn"),
            ),
        };
    if (seq !== serial) return;
    setPage(view);
    closeModal();
    window.scrollTo({ top: 0, behavior: "instant" });
  } catch (err) {
    if (seq !== serial) return;
    main().setAttribute("aria-busy", "false");
    main().innerHTML =
      head(labels[tab] || "Workspace", "Workspace") +
      `<div class="form-panel">${formError(err)}${button("Try again", "retry", "btn")}</div>`;
    document
      .querySelector('[data-action="retry"]')
      ?.addEventListener("click", render);
  }
}
window.addEventListener("hashchange", render);
async function start() {
  try {
    await ready();
    await render();
  } catch (err) {
    main().setAttribute("aria-busy", "false");
    showFailure(err);
  }
}
start();
