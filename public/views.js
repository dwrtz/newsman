import {
  state,
  call,
  can,
  whenAllowed,
  head,
  href,
  link,
  button,
  icon,
  escape as e,
  empty,
  metricBand,
  checks,
  field,
  badge,
  human,
  dateTime,
  timeInput,
  detailPairs,
  showModal,
  closeModal,
  toast,
  formError,
  describeError,
  safePreviewFrame,
  copyText,
} from "./console.js";

const input = (f) => Object.fromEntries(new FormData(f));
const bindings =
  (pairs = []) =>
  () => {
    for (const [s, event, handler] of pairs) {
      const node = document.querySelector(s);
      if (node) node.addEventListener(event, handler);
    }
  };
const okRoute = (hash) => {
  if (location.hash === hash) window.dispatchEvent(new Event("hashchange"));
  else location.hash = hash;
};
const tLink = (name, label, params = {}) => {
  if (!can(name)) return "";
  const query = new URLSearchParams(params).toString();
  return link(
    label,
    href("tools", name) + (query ? "?" + query : ""),
    "text-button",
  );
};
const createLink = () =>
  can("issue_create")
    ? link("New message", href("compose"), "btn primary large", "plus")
    : "";
function proseHtml(value) {
  return String(value || "")
    .trim()
    .split(/\n\s*\n/)
    .filter(Boolean)
    .map((p) => "<p>" + e(p).replaceAll("\n", "<br>") + "</p>")
    .join("\n");
}
const pub = () => state.settings?.settings.publication_name || "Workspace";
const table = (headers, rows) =>
  `<div class="table-wrap"><table class="data-table"><thead><tr>${headers.map((c) => `<th scope="col">${e(c)}</th>`).join("")}</tr></thead><tbody>${rows.join("")}</tbody></table></div>`;
const panel = (title, contents, action = "") =>
  `<section class="panel"><div class="panel-title"><h2>${e(title)}</h2>${action}</div>${contents}</section>`;
const card = (title, content) =>
  `<section class="sub-panel"><h2>${e(title)}</h2>${content}</section>`;
const smallBtn = (label, act) => button(label, act, "btn small");
const formActions = (title) =>
  `<div class="actions"><button class="btn primary" type="submit">${e(title)}</button></div><div data-error></div>`;
const itemRows = (items) =>
  items
    .map(
      (r) =>
        `<a class="issue-row" href="${href("compose", r.id)}"><span class="mini-icon">${icon("issue")}</span><div class="item-lines"><strong>${e(r.title)}</strong><small>Updated ${e(dateTime(r.updated_at, true))}</small></div><span class="trailing">v${e(r.version)}</span>${icon("next")}</a>`,
    )
    .join("");
const stdRoute = (part, p) => href(part) + (p.size ? "?" + p.toString() : "");
function pagination(base, params, result) {
  if (!result.next_cursor && !params.get("cursor")) return "";
  const p = new URLSearchParams(params);
  p.delete("cursor");
  const start = stdRoute(base, p);
  if (result.next_cursor) p.set("cursor", result.next_cursor);
  return `<div class="next-page">${params.has("cursor") ? link("First page", start, "text-button", "back") : "<span></span>"}${result.next_cursor ? link("Next page", stdRoute(base, p), "text-button", "next") : ""}</div>`;
}
const optional = (v) => (v?.length ? v : undefined);
const searchBar = (q, placeholder, options, current) =>
  `<form id="filter-form"><label class="searchbox">${icon("search")}<input name="search" aria-label="${e(placeholder)}" placeholder="${e(placeholder)}" value="${e(q)}"></label>${options ? `<select name="state" aria-label="Filter status"><option value="">All statuses</option>${options.map((x) => `<option value="${x}" ${x === current ? "selected" : ""}>${e(human(x))}</option>`).join("")}</select>` : ""}<button type="submit" class="btn">Filter</button></form>`;
function bindFilter(base) {
  return bindings([
    [
      "#filter-form",
      "submit",
      (ev) => {
        ev.preventDefault();
        const p = new URLSearchParams();
        for (const [k, v] of new FormData(ev.currentTarget)) if (v) p.set(k, v);
        okRoute(stdRoute(base, p));
      },
    ],
  ]);
}
function submit(selector, handler) {
  const f = document.querySelector(selector);
  if (!f) return;
  f.addEventListener("submit", async (event) => {
    event.preventDefault();
    const btn = event.submitter;
    const msg = f.querySelector("[data-error]");
    if (msg) msg.textContent = "";
    if (btn) btn.disabled = true;
    try {
      await handler(input(f), f, event);
    } catch (err) {
      describeError(msg, err);
    } finally {
      if (btn) btn.disabled = false;
    }
  });
}
function act(name, handler) {
  document.querySelectorAll(`[data-action="${name}"]`).forEach((node) =>
    node.addEventListener("click", async () => {
      node.disabled = true;
      try {
        await handler(node);
      } catch (err) {
        showModal("Action unavailable", formError(err));
      } finally {
        node.disabled = false;
      }
    }),
  );
}
function art(title) {
  return `<div class="mail-art" aria-hidden="true"><div class="envelope"><div class="env-upper"><span class="env-brand">newsman.</span><span class="env-stamp">${icon("mail")}</span></div><span class="env-lines"></span><div class="env-address">${e(title)}</div><div class="env-stripe"></div></div></div>`;
}
export async function overview() {
  const [dashboard, issues] = await Promise.all([
    call("dashboard_get"),
    whenAllowed("issues_list", { limit: 4 }, { items: [] }),
  ]);
  const fresh = issues.items[0];
  const issue =
    fresh && can("issue_get")
      ? await call("issue_get", { id: fresh.id })
      : null;
  const title =
    issue?.draft.title || fresh?.title || "Your next message starts here";
  const caption =
    issue?.draft.preheader ||
    (issue?.draft.subject !== title && issue?.draft.subject) ||
    issue?.draft.plain_text ||
    (fresh
      ? ""
      : "Write a draft, preview the email, and choose when to send it.");
  const feature = `<section class="feature"><div class="feature-content"><span class="eyebrow">${issue ? e(human(issue.state) + " message") : fresh ? "Latest message" : "Message editor"}</span><h2>${e(title)}</h2><p class="lead">${e(caption)}</p>${fresh || can("issue_create") ? `<a class="continue" href="${fresh ? href("compose", fresh.id) : href("compose")}">${fresh ? "Open message" : "Create a message"} ${icon("arrow")}</a>` : ""}</div>${art(pub())}</section>`;
  return {
    html:
      head("Publishing desk", "Overview", createLink()) +
      feature +
      metricBand(dashboard.metrics) +
      `<div class="overview-bottom">${panel("Recent messages", issues.items.length ? itemRows(issues.items) : empty("No messages yet", "Start with a draft in the message editor."), can("issues_list") ? link("All messages", href("issues"), "text-button", "arrow") : "")}${panel("System checks", checks(dashboard.checks), can("health_get") ? link("Details", href("tools", "health_get"), "text-button") : "")}</div>`,
  };
}
export async function issuesView(params) {
  const q = params.get("search") || "",
    s = params.get("state") || "",
    res = await call("issues_list", {
      search: optional(q),
      state: optional(s),
      cursor: optional(params.get("cursor")),
      limit: 25,
    });
  return {
    html:
      head("Messages", "Publishing", createLink()) +
      `<div class="toolbar">${searchBar(q, "Search messages", ["draft", "approved", "historical", "archived"], s)}${tLink("issue_import", "Import message bundle")}</div>` +
      panel(
        "All messages",
        res.items.length
          ? table(
              ["Message", "Revision", "Updated"],
              res.items.map(
                (r) =>
                  `<tr><td><a class="cell-main" href="${href("compose", r.id)}">${e(r.title)}</a>${r.subject !== r.title ? `<span class="cell-sub">${e(r.subject)}</span>` : ""}</td><td>v${e(r.version)}</td><td>${e(dateTime(r.updated_at))}</td></tr>`,
              ),
            ) + pagination("issues", params, res)
          : empty(
              "No matching messages",
              "Create a message or change the search.",
            ),
      ),
    mount: bindFilter("issues"),
  };
}
export async function compose(id) {
  const doc = id ? await call("issue_get", { id }) : null,
    draft = doc?.draft || {
      title: "",
      subject: "",
      preheader: "",
      html: "",
      plain_text: "",
    };
  const canEdit = can(doc ? "issue_update" : "issue_create");
  const prose = !doc || draft.html === proseHtml(draft.plain_text);
  const subject = draft.subject,
    preview =
      doc && can("issue_render")
        ? button("Preview email", "preview", "btn", "mail")
        : "";
  const editor = `<section class="form-panel"><h2>Draft message</h2><form id="draft-form" class="form-grid"><fieldset class="editor-fields form-grid" ${canEdit ? "" : "disabled"}>
    ${field("title", "Internal message title", draft.title, { required: true, max: 160 })}
    ${field("subject", "Email subject", subject, { required: true, max: 180 })}
    ${field("preheader", "Preview text", draft.preheader, { max: 500, help: "Shown beside the subject in supporting inboxes." })}
    ${field("body_format", "Body format", prose ? "prose" : "html", {
      options: [
        ["prose", "Plain text with paragraph formatting"],
        ["html", "Custom HTML and plain text"],
      ],
    })}
    <div id="prose-panel" class="form-grid" ${prose ? "" : "hidden"}>${field("body", "Email body", draft.plain_text, { rows: 8 })}</div>
    <div id="markup-panel" class="form-grid" ${prose ? "hidden" : ""}>${field("html", "Email HTML", draft.html, { rows: 8, code: true })}${field("plain_text", "Plain-text version", draft.plain_text, { rows: 4 })}</div>
    <details><summary>Template and image</summary><div class="form-grid">
    ${field("template_id", "Template ID (optional)", draft.template_id || "")}
    ${field("hero_asset_id", "Hero image file ID (optional)", draft.hero_asset_id || "")}
    ${field("hero_alt", "Image alt text", draft.hero_alt || "", { max: 500 })}
    ${can("file_begin") ? link("Manage file transfers", href("transfers"), "text-button", "upload") : ""}</div></details>
    </fieldset>${canEdit ? formActions(doc ? "Save revision" : "Save draft") : ""}</form></section>`;
  let meta = card(
    "Message details",
    detailPairs([
      ["State", doc ? human(doc.state) : "Unsaved draft"],
      ["Revision", doc ? "v" + doc.version : "—"],
      ["Workspace", pub()],
    ]),
  );
  if (doc)
    meta += card(
      "Delivery",
      [
        can("send_test") ? button("Send test", "test", "btn", "mail") : "",
        can("send_prepare")
          ? button("Review send", "review", "btn primary", "plane")
          : "",
      ].some(Boolean)
        ? `<p>Test this revision or review its audience and sending time.</p><div class="actions">${can("send_test") ? button("Send test", "test", "btn", "mail") : ""}${can("send_prepare") ? button("Review send", "review", "btn primary", "plane") : ""}</div>`
        : "<p>Your account can read this message.</p>",
    );
  if (doc)
    meta += card(
      "Message tools",
      `<div class="stack compact">${[
        ["issue_versions", "Version history"],
        ["archive_bundle", "Export archive bundle"],
        ["issue_archive", "Archive message"],
      ]
        .map(([key, text]) => tLink(key, text, { id, version: doc.version }))
        .join("")}</div>`,
    );
  return {
    html:
      head(doc ? "Message editor" : "New message", "Publishing", preview) +
      `<div class="two-col"><div>${editor}</div><div class="stack">${meta}</div></div>`,
    mount() {
      const f = document.getElementById("draft-form"),
        fields = f.elements;
      const toggleBody = () => {
        const useProse = fields.body_format.value === "prose";
        document.getElementById("prose-panel").hidden = !useProse;
        document.getElementById("markup-panel").hidden = useProse;
        fields.body.required = useProse;
        fields.html.required = !useProse;
        fields.plain_text.required = !useProse;
      };
      fields.body_format.addEventListener("change", toggleBody);
      toggleBody();
      f.addEventListener("input", () => {
        if (!doc) return;
        for (const actName of ["preview", "test", "review"]) {
          const action = document.querySelector(`[data-action="${actName}"]`);
          if (action) {
            action.disabled = true;
            action.title = "Save this revision before continuing.";
          }
        }
        const m = f.querySelector("[data-error]");
        if (m)
          m.textContent =
            "Unsaved changes. Save the revision to preview or send it.";
      });
      submit("#draft-form", async (values) => {
        const { body, body_format, html, plain_text, ...metadata } = values;
        const edited = {
          ...draft,
          ...metadata,
          html: body_format === "prose" ? proseHtml(body) : html,
          plain_text: body_format === "prose" ? body : plain_text,
        };
        for (const key of ["template_id", "hero_asset_id", "hero_alt"])
          if (!edited[key]) delete edited[key];
        const args = { draft: edited };
        if (doc) {
          args.id = id;
          args.expected_version = doc.version;
        }
        const saved = await call(doc ? "issue_update" : "issue_create", args);
        toast("Draft saved.");
        okRoute(href("compose", saved.id));
      });
      if (!doc) return;
      act("preview", async () => {
        const rendered = await call("issue_render", {
          id,
          version: doc.version,
        });
        showModal(
          "Email preview",
          `<p><strong>${e(rendered.subject)}</strong></p>${detailPairs([
            ["From", rendered.from_address],
            ["Reply to", rendered.reply_to],
            ["Revision", doc.version],
          ])}<div class="box-gap" id="email-frame"></div><details><summary>Plain-text email</summary><p class="preview-text">${e(rendered.plain_text)}</p></details>${(rendered.warnings || []).map((x) => `<p class="notice">${e(x)}</p>`).join("")}`,
        );
        safePreviewFrame(document.getElementById("email-frame"), rendered.html);
      });
      act("test", () => {
        showModal(
          "Send a test email",
          `<form id="test-form" class="form-grid">${field("recipient", "Allowlisted test recipient", "", { type: "email", required: true })}${formActions("Send test email")}</form>`,
        );
        submit("#test-form", async (data) => {
          const job = await call("send_test", {
            issue_id: id,
            issue_version: doc.version,
            recipient: data.recipient,
          });
          closeModal();
          toast("Test job queued: " + job.id);
        });
      });
      act("review", () => startSend(doc));
    },
  };
}
function startSend(doc) {
  showModal(
    "Review delivery",
    `<form id="plan-form" class="form-grid"><p>Prepare a plan for the current revision and eligible subscribers.</p>${field("tags", "Required tags", "", { help: "Separate tags with commas. Leave empty for all active subscribers." })}${field("due_at", "Delivery time", "", { type: "datetime-local", help: "Your local time. Leave empty to send after approval." })}${formActions("Prepare plan")}</form>`,
  );
  submit("#plan-form", async (values) => {
    const tags = values.tags
      .split(",")
      .map((x) => x.trim())
      .filter(Boolean);
    const plan = await call("send_prepare", {
      issue_id: doc.id,
      issue_version: doc.version,
      audience: {
        statuses: ["active"],
        ...(tags.length ? { tags_all: tags } : {}),
      },
      ...(values.due_at
        ? { due_at: new Date(values.due_at).toISOString() }
        : {}),
    });
    showModal(
      "Approve delivery",
      `<p><strong>${e(plan.subject)}</strong></p>${detailPairs([
        ["From", plan.from_address],
        ["Reply to", plan.reply_to],
        ["Due", dateTime(plan.due_at)],
        ["Expires", dateTime(plan.expires_at)],
        ["Cost estimate", JSON.stringify(plan.cost)],
      ])}${metricBand(plan.counts)}${(plan.warnings || []).map((x) => `<p class="notice">${e(x)}</p>`).join("")}<details><summary>Plan digests</summary>${detailPairs(
        [
          ["Plan", plan.digest],
          ["Message", plan.issue_digest],
          ["Audience", plan.eligibility_digest],
        ],
      )}</details><form id="commit-form" class="box-gap">${formActions("Approve and schedule delivery")}</form>`,
    );
    submit("#commit-form", async () => {
      const result = await call("send_commit", {
        plan_id: plan.id,
        plan_digest: plan.digest,
      });
      closeModal();
      toast("Delivery approved.");
      okRoute(href("delivery", result.id));
    });
  });
}
export async function audience(params) {
  const q = params.get("search") || "",
    s = params.get("state") || "",
    res = await call("subscribers_list", {
      search: optional(q),
      filter: s ? { statuses: [s] } : undefined,
      cursor: optional(params.get("cursor")),
      limit: 25,
    });
  const rows = res.items.map(
    (r) =>
      `<tr><td><a class="cell-main" href="${href("audience", r.id)}">${e(r.email || "Erased record")}</a></td><td>${badge(r.status)}</td><td>${e(r.tags.join(", ") || "—")}</td><td>${e(dateTime(r.confirmed_at, true))}</td></tr>`,
  );
  return {
    html:
      head(
        "Audience",
        "Publishing",
        can("file_begin")
          ? link("Import audience", href("transfers"), "btn", "upload")
          : "",
      ) +
      `<div class="toolbar">${searchBar(q, "Search subscribers", ["pending", "active", "unsubscribed", "held", "suppressed", "erased", "test_only"], s)}${tLink("segments_list", "Saved segments")}</div>` +
      panel(
        "Subscribers",
        res.items.length
          ? table(["Email", "Status", "Tags", "Confirmed"], rows) +
              pagination("audience", params, res)
          : empty(
              "No matching subscribers",
              s || q
                ? "Try another search or status."
                : "Readers appear here when they subscribe or are imported.",
            ),
      ),
    mount: bindFilter("audience"),
  };
}
export async function person(id) {
  const [r, events] = await Promise.all([
    call("subscriber_get", { id }),
    whenAllowed("subscriber_history", { id, limit: 25 }, { items: [] }),
  ]);
  const controls = can("subscriber_tags_set")
    ? `<form id="tag-form" class="form-grid">${field("tags", "Tags", r.tags.join(", "), { help: "Separate tags with commas." })}${formActions("Save tags")}</form>`
    : "";
  return {
    html:
      head(
        "Subscriber",
        "Audience",
        link("Audience", href("audience"), "btn", "back"),
      ) +
      `<div class="two-col"><div class="stack">${card(
        r.email || "Erased record",
        detailPairs([
          ["Status", human(r.status)],
          ["Confirmed", dateTime(r.confirmed_at)],
          ["Consent source", r.evidence_kind || "—"],
          ["First campaign", r.first_touch?.utm_campaign || "—"],
          ["Suppression reason", r.suppression_reason || "—"],
        ]),
      )}${panel(
        "History",
        events.items.length
          ? table(
              ["Event", "When"],
              events.items.map(
                (x) =>
                  `<tr><td>${e(human(x.kind))}</td><td>${e(dateTime(x.at))}</td></tr>`,
              ),
            )
          : empty("No events recorded"),
      )}</div><div class="stack">${controls ? card("Tags", controls) : ""}${can("subscriber_suppress") ? card("Subscriber actions", button("Suppress subscriber", "suppress", "btn") + " " + tLink("subscriber_erase", "Erase reader data", { id, version: r.version })) : ""}</div></div>`,
    mount() {
      submit("#tag-form", async (values) => {
        await call("subscriber_tags_set", {
          id,
          expected_version: r.version,
          tags: values.tags
            .split(",")
            .map((x) => x.trim())
            .filter(Boolean),
        });
        toast("Tags saved.");
        okRoute(href("audience", id));
      });
      act("suppress", () => {
        showModal(
          "Suppress subscriber",
          `<form id="suppress-form" class="form-grid"><p>Stop future delivery to this reader.</p>${field("reason", "Reason", "", { required: true })}${formActions("Confirm suppression")}</form>`,
        );
        submit("#suppress-form", async (v) => {
          await call("subscriber_suppress", {
            id,
            expected_version: r.version,
            reason: v.reason,
          });
          closeModal();
          okRoute(href("audience", id));
        });
      });
    },
  };
}
export async function delivery(id, params) {
  if (id) {
    const c = await call("campaign_get", { id });
    return {
      html:
        head(
          "Campaign detail",
          "Delivery",
          link("All campaigns", href("delivery"), "btn", "back"),
        ) +
        card(
          "Campaign",
          detailPairs([
            ["ID", c.id],
            ["Message", c.issue_id],
            ["State", human(c.state)],
            ["Due", dateTime(c.due_at)],
            ["Revision", c.version],
          ]) +
            metricBand(c.counts) +
            `<div class="actions">${link("Open message", href("compose", c.issue_id), "btn")}${can("send_cancel") ? button("Stop remaining sends", "cancel", "btn") : ""}${tLink("attempts_list", "Inspect attempts", { id })}</div>`,
        ),
      mount() {
        act("cancel", () => {
          showModal(
            "Stop remaining sends",
            `<form id="stop-form" class="form-grid">${field("reason", "Reason", "", { required: true })}${formActions("Stop remaining sends")}</form>`,
          );
          submit("#stop-form", async (v) => {
            await call("send_cancel", {
              id,
              expected_version: c.version,
              reason: v.reason,
            });
            closeModal();
            okRoute(href("delivery", id));
          });
        });
      },
    };
  }
  const res = await call("campaigns_list", {
    limit: 25,
    cursor: optional(params.get("cursor")),
  });
  return {
    html:
      head("Delivery", "Publishing", tLink("health_get", "System health")) +
      panel(
        "Campaigns",
        res.items.length
          ? table(
              ["Campaign", "Status", "Due"],
              res.items.map(
                (c) =>
                  `<tr><td><a class="cell-main" href="${href("delivery", c.id)}">${e(c.id)}</a></td><td>${badge(c.state)}</td><td>${e(dateTime(c.due_at))}</td></tr>`,
              ),
            ) + pagination("delivery", params, res)
          : empty(
              "No campaigns yet",
              "Open a message to prepare its audience and delivery time.",
              link("View messages", href("issues"), "text-button", "arrow"),
            ),
      ),
  };
}
export async function reports(params) {
  const now = Date.now(),
    from = params.get("from") || new Date(now - 30 * 86400000).toISOString(),
    to = params.get("to") || new Date(now).toISOString(),
    kind = params.get("kind") || "edition",
    id = params.get("issue_id") || "";
  const result = await call("report_get", {
    kind,
    from,
    to,
    issue_id: optional(id),
    limit: 100,
    cursor: optional(params.get("cursor")),
  });
  return {
    html:
      head("Reports", "Publication") +
      `<section class="form-panel"><form id="report-filters" class="form-grid columns">${field("kind", "Report", kind, { options: [["edition", "Messages"], "links", "acquisition", "cohort", "usage"] })}${field("issue_id", "Message ID (optional)", id)}${field("from", "From", timeInput(from), { type: "datetime-local", required: true })}${field("to", "Through", timeInput(to), { type: "datetime-local", required: true })}${formActions("Run report")}</form></section><div class="box-gap">${panel(
        "Results",
        result.items.length
          ? table(
              ["Metric", "Value", "Unit", "Source"],
              result.items.map(
                (r) =>
                  `<tr><td>${e(human(r.label))}</td><td>${e(r.value ?? r.unavailable_reason ?? "—")}</td><td>${e(r.unit)}</td><td>${e(r.source || "—")}</td></tr>`,
              ),
            ) + pagination("reports", params, result)
          : empty(
              "No metrics for this window",
              "Change the report or its date range.",
            ),
      )}</div>`,
    mount() {
      submit("#report-filters", async (v) => {
        const p = new URLSearchParams({
          kind: v.kind,
          from: new Date(v.from).toISOString(),
          to: new Date(v.to).toISOString(),
        });
        if (v.issue_id) p.set("issue_id", v.issue_id);
        okRoute(stdRoute("reports", p));
      });
    },
  };
}
export async function connections() {
  const grants = await whenAllowed(
    "grants_list",
    { limit: 100 },
    { items: [] },
  );
  const path = state.registry.operator_path || "/mcp",
    url = new URL(path, location.origin).href;
  const operatorOps = state.registry.operations.filter(
    (op) => op.surface === "operator",
  );
  const scopeRows = [
    ["read", "Inspect messages, audience and delivery."],
    ["write", "Edit drafts and subscriber records."],
    ["send", "Test, schedule, send and stop delivery."],
    ["admin", "Manage access, settings and transfers."],
  ]
    .map(([scope, detail]) => {
      const count = operatorOps.filter((op) => op.scope === scope).length;
      return `<div class="scope-row"><div><code>${e(scope)}</code><small>${e(detail)}</small></div><span>${e(count)}</span></div>`;
    })
    .join("");
  return {
    html:
      head("MCP connections", "Workspace") +
      `<div class="stack"><section class="connection-card"><span class="eyebrow">Remote MCP server</span><h2>Operator endpoint</h2><div class="code-bar"><code>${e(url)}</code>${button("Copy URL", "copy-endpoint", "copy-button", "copy")}</div></section><div class="two-col"><div class="stack">${card("Connect a client", `<p>Add the endpoint to your MCP client using Streamable HTTP. Start authorization, sign in with an allowlisted identity, and grant the scopes the client needs. Grant read, write, send and admin for full access. The web console and MCP use the same operations.</p><div class="actions">${link("Explore tools", href("tools"), "btn", "tool")}</div>`)}${panel(
        "Authorized clients",
        grants.items.length
          ? table(
              ["Client", "Scopes", "Access"],
              grants.items.map(
                (g) =>
                  `<tr><td>${e(g.client)}</td><td>${e(g.scopes.join(", "))}</td><td>${g.active && can("grants_revoke") ? button("Revoke", "revoke", "btn small", "", `data-id="${e(g.id)}" data-version="${e(g.version)}"`) : e(g.active ? "Active" : "Revoked")}</td></tr>`,
              ),
            )
          : empty(
              "No authorized clients",
              "Clients appear here after the operator grants access.",
            ),
      )}</div>${card("Operator tools", `<div class="tool-tally"><strong>${operatorOps.length}</strong><span>operator actions</span></div><div class="scope-grants">${scopeRows}</div><p class="field-caption box-gap">Session scopes</p><div class="scope-list">${(state.session.scopes || []).map((s) => `<code>${e(s)}</code>`).join("")}</div>`)}</div></div>`,
    mount() {
      act("copy-endpoint", () => copyText(url));
      act("revoke", (node) => {
        showModal(
          "Revoke client access",
          `<p>This client loses its current grant.</p>${button("Revoke grant", "confirm-revoke", "btn primary")}`,
        );
        act("confirm-revoke", async () => {
          await call("grants_revoke", {
            id: node.dataset.id,
            expected_version: Number(node.dataset.version),
          });
          closeModal();
          toast("Grant revoked.");
          okRoute(href("connections"));
        });
      });
    },
  };
}
export async function settings() {
  const config = await call("settings_get"),
    s = config.settings;
  state.settings = config;
  return {
    html:
      head(
        "Settings",
        "Workspace",
        can("operators_list")
          ? link("Operator allowlist", href("operators"), "btn", "shield")
          : "",
      ) +
      `<section class="form-panel"><h2>Sender settings</h2><form id="settings-form" class="form-grid columns">${field("publication_name", "Workspace name", s.publication_name, { required: true })}${field("time_zone", "Time zone", s.time_zone, { required: true })}${field("public_origin", "Public site origin", s.public_origin, { type: "url", required: true })}${field("from_address", "Sender address", s.from_address, { type: "email", required: true })}${field("reply_to", "Reply-to address", s.reply_to, { type: "email", required: true })}${field("consent_version", "Consent wording version", s.consent_version, { required: true })}${field("archive_lag_days", "Archive lag in days", s.archive_lag_days, { type: "number", min: 0, required: true })}${field("send_budget", "Send budget", s.send_budget, { type: "number", min: 0, required: true, help: "A budget of 0 pauses production sends." })}${field("postal_footer", "Postal footer", s.postal_footer, { rows: 2, wide: true })}${field("test_recipients", "Allowlisted test recipients", (s.test_recipients || []).join(", "), { wide: true, help: "Separate addresses with commas." })}<label class="bool wide"><input type="checkbox" name="click_tracking" ${s.click_tracking ? "checked" : ""}> Enable click tracking</label>${can("settings_update") ? formActions("Save settings") : ""}</form></section>`,
    mount() {
      submit("#settings-form", async (v) => {
        await call("settings_update", {
          expected_version: config.version,
          settings: {
            ...s,
            ...v,
            archive_lag_days: Number(v.archive_lag_days),
            send_budget: Number(v.send_budget),
            click_tracking: v.click_tracking === "on",
            test_recipients: v.test_recipients
              .split(",")
              .map((a) => a.trim())
              .filter(Boolean),
          },
        });
        state.settings = await call("settings_get");
        toast("Settings saved.");
        okRoute(href("settings"));
      });
    },
  };
}

export async function operators(params) {
  const list = await call("operators_list", {
    limit: 50,
    cursor: optional(params.get("cursor")),
  });
  const rows = list.items.map(
    (item) =>
      `<tr><td class="cell-main">${e(item.email)}</td><td>${badge(item.state)}</td><td>${can("operators_remove") && item.state !== "removed" ? button("Remove", "remove-operator", "btn small", "", `data-id="${e(item.id)}" data-version="${Number(item.version)}" data-email="${e(item.email)}"`) : ""}</td></tr>`,
  );
  return {
    html:
      head(
        "Operator allowlist",
        "Workspace",
        link("Settings", href("settings"), "btn", "back"),
      ) +
      `<div class="two-col"><div>${panel("Allowed operators", list.items.length ? table(["Identity", "State", "Access"], rows) + pagination("operators", params, list) : empty("No entries recorded"))}</div>${can("operators_add") ? `<section class="form-panel"><h2>Allow an identity</h2><form id="operator-form" class="form-grid">${field("email", "Verified email address", "", { required: true, type: "email", help: "Admits this identity to the operator console and MCP authorization." })}${formActions("Add operator")}</form></section>` : ""}</div>`,
    mount() {
      submit("#operator-form", async (args) => {
        await call("operators_add", { email: args.email });
        toast("Operator added.");
        okRoute(href("operators"));
      });
      act("remove-operator", (node) => {
        showModal(
          "Remove operator",
          `<p>Revoke operator access for <strong>${e(node.dataset.email)}</strong>?</p>${button("Remove operator", "confirm-remove-operator", "btn primary")}`,
        );
        act("confirm-remove-operator", async () => {
          await call("operators_remove", {
            id: node.dataset.id,
            expected_version: Number(node.dataset.version),
          });
          closeModal();
          toast("Operator removed.");
          okRoute(href("operators"));
        });
      });
    },
  };
}
