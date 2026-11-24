import {
  state,
  call,
  can,
  head,
  href,
  link,
  button,
  icon,
  escape as e,
  empty,
  badge,
  human,
  field,
  formError,
  showModal,
  closeModal,
  toast,
  copyText,
  setResult,
  describeError,
} from "./console.js";

function schema(def) {
  return def?.$ref ? state.registry.$defs[def.$ref.split("/").pop()] : def;
}
function expand(def, visited = new Set()) {
  if (!def || typeof def !== "object") return def;
  if (Array.isArray(def)) return def.map((item) => expand(item, visited));
  if (def.$ref) {
    if (visited.has(def.$ref)) return def;
    return expand(schema(def), new Set([...visited, def.$ref]));
  }
  return Object.fromEntries(
    Object.entries(def).map(([k, v]) => [k, expand(v, visited)]),
  );
}
function example(def, name) {
  const s = schema(def) || {};
  if (name === "key") return crypto.randomUUID();
  if (s.enum) return s.enum[0];
  if (s.type === "object")
    return Object.fromEntries(
      (s.required || [])
        .filter((k) => k !== "key")
        .map((k) => [k, example(s.properties[k], k)]),
    );
  if (s.type === "array") return [];
  if (s.type === "boolean") return false;
  if (s.type === "integer" || s.type === "number") return s.minimum ?? 0;
  if (s.format === "date-time") return new Date().toISOString();
  return "";
}
function genArgs(op) {
  return example(op.inputSchema);
}
function keyClass(n) {
  return /(_remove|_erase|_suppress|_commit|_resolve|_cancel|_delete|_revoke|dispatch_set)$/.test(
    n,
  );
}
export function toolsPage(name, params) {
  const ops = Array.from(state.operations.values()).filter(
      (op) => op.surface === "operator" && can(op.name),
    ),
    chosen = ops.find((op) => op.name === name) || ops[0];
  if (!chosen)
    return {
      html:
        head("Operation explorer", "Workspace") +
        empty(
          "No available operations",
          "Contact the instance owner to check your grant.",
        ),
    };
  const groups = Object.groupBy
    ? Object.groupBy(ops, (x) => x.view)
    : ops.reduce((a, x) => ((a[x.view] ??= []).push(x), a), {});
  const id = params.get("id"),
    ver = Number(params.get("version")),
    kind = params.get("kind");
  const initial = genArgs(chosen),
    props = chosen.inputSchema.properties || {};
  if (id && props.id) initial.id = id;
  if (id && props.issue_id) initial.issue_id = id;
  if (id && props.campaign_id) initial.campaign_id = id;
  if (id && props.subscriber_id) initial.subscriber_id = id;
  if (ver && props.expected_version) initial.expected_version = ver;
  if (ver && props.version) initial.version = ver;
  if (kind && props.kind) initial.kind = kind;
  function entries(q = "") {
    return (
      Object.entries(groups)
        .map(([group, all]) => {
          const filtered = all.filter((op) =>
            [op.name, op.view, op.description]
              .join(" ")
              .toLowerCase()
              .includes(q.toLowerCase()),
          );
          return filtered.length
            ? `<div><div class="tool-section column-label">${e(group === "Issues" ? "Messages" : group)}</div>${filtered.map((op) => `<a class="tool-link ${chosen.name === op.name ? "selected" : ""}" ${chosen.name === op.name ? 'aria-current="true"' : ""} href="${href("tools", op.name)}">${e(op.name)}</a>`).join("")}</div>`
            : "";
        })
        .join("") || empty("No matching operations")
    );
  }
  const submitLabel = chosen.mode === "query" ? "Run query" : "Run command",
    docJson = JSON.stringify(initial, null, 2);
  return {
    html:
      head(
        "Operation explorer",
        "Workspace",
        link("MCP connection", href("connections"), "btn", "plug"),
      ) +
      `<div class="op-grid"><aside class="sub-panel"><label class="searchbox">${icon("search")}<input id="tool-search" type="search" aria-label="Filter operations" placeholder="Find an operation"></label><div class="op-list" id="tool-list">${entries()}</div></aside><section class="form-panel"><div class="wide-actions"><h2 class="operation-name">${e(chosen.name)}</h2><span class="badge">${e(chosen.scope)}</span></div><p class="tool-desc">${e(chosen.description)}</p><form class="form-grid" id="op-form">${field("arguments", "Arguments (JSON)", docJson, { code: true, rows: Math.min(14, Math.max(3, docJson.split("\n").length + 1)), required: true })}<div class="wide-actions"><button type="submit" class="btn primary">${e(submitLabel)}</button>${button("Copy MCP call", "copy-call", "text-button", "copy")}</div><div id="op-error"></div></form><div id="op-result" class="result box-gap" hidden></div><details class="box-gap"><summary>Input schema</summary><pre>${e(JSON.stringify(expand(chosen.inputSchema), null, 2))}</pre></details><details><summary>Output schema</summary><pre>${e(JSON.stringify(expand(chosen.outputSchema), null, 2))}</pre></details></section></div>`,
    mount() {
      document.getElementById("tool-search").addEventListener("input", (ev) => {
        document.getElementById("tool-list").innerHTML = entries(
          ev.target.value,
        );
      });
      const form = document.getElementById("op-form"),
        argsField = form.elements.arguments,
        result = document.getElementById("op-result"),
        err = document.getElementById("op-error");
      const getArgs = () => {
        const obj = JSON.parse(argsField.value);
        if (!obj || typeof obj !== "object" || Array.isArray(obj))
          throw new Error("Arguments must be a JSON object.");
        return obj;
      };
      const execute = async (args) => {
        const btn = form.querySelector("[type=submit]");
        btn.disabled = true;
        try {
          const output = await call(chosen.name, args);
          setResult(result, output);
          toast("Operation complete.");
        } catch (error) {
          describeError(err, error);
        } finally {
          btn.disabled = false;
        }
      };
      form.addEventListener("submit", (ev) => {
        ev.preventDefault();
        err.replaceChildren();
        result.hidden = true;
        try {
          const args = getArgs();
          if (chosen.mode === "command" && keyClass(chosen.name)) {
            showModal(
              "Confirm command",
              `<p>Run <code>${e(chosen.name)}</code> with the entered arguments?</p>${button("Run command", "confirm-op", "btn primary")}`,
            );
            document
              .querySelector('[data-action="confirm-op"]')
              .addEventListener(
                "click",
                () => {
                  closeModal();
                  execute(args);
                },
                { once: true },
              );
          } else execute(args);
        } catch (error) {
          describeError(err, error);
        }
      });
      document
        .querySelector('[data-action="copy-call"]')
        .addEventListener("click", () => {
          try {
            const args = getArgs();
            if (chosen.mode === "command" && !args.key)
              args.key = crypto.randomUUID();
            copyText(
              JSON.stringify(
                {
                  jsonrpc: "2.0",
                  id: 1,
                  method: "tools/call",
                  params: { name: chosen.name, arguments: args },
                },
                null,
                2,
              ),
            );
          } catch (error) {
            describeError(err, error);
          }
        });
    },
  };
}
function chunkBase64(data) {
  let s = "";
  for (let i = 0; i < data.length; i += 4096)
    s += String.fromCharCode(...data.subarray(i, i + 4096));
  return btoa(s);
}
function fromBase64(v) {
  return Uint8Array.from(atob(v), (c) => c.charCodeAt(0));
}
async function hash(bytes) {
  const h = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
  return Array.from(h, (b) => b.toString(16).padStart(2, "0")).join("");
}
const progress = (msg) => {
  const n = document.getElementById("transfer-progress");
  if (n) n.textContent = msg;
};
function fSubmit(id, handler) {
  const f = document.getElementById(id);
  if (!f) return;
  f.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const btn = ev.submitter;
    btn.disabled = true;
    const result = f.querySelector("[data-result]");
    result.replaceChildren();
    try {
      await handler(new FormData(f), result);
    } catch (err) {
      describeError(result, err);
    } finally {
      btn.disabled = false;
    }
  });
}
const toolLink = (name, text) =>
  can(name) ? link(text, href("tools", name), "text-button", "arrow") : "";
export function transfers() {
  const canUpload = ["file_begin", "file_write", "file_finish"].every(can);
  const kinds = state.registry.$defs.blob_kind.enum.filter(
    (k) =>
      can("import_prepare") ||
      ![
        "audience_import",
        "suppressions",
        "send_history",
        "historical_metrics",
        "consent_history",
      ].includes(k),
  );
  const upload = canUpload
    ? `<section class="form-panel"><h2>Upload a file</h2><form id="upload" class="form-grid">${field("kind", "File purpose", kinds[0], { options: kinds.map((k) => (k === "issue_bundle" ? [k, "Message bundle"] : k)) })}<label class="field"><span>Local file</span><input type="file" name="file" required><small>Maximum 25 MiB per file.</small></label><div><button class="btn primary" type="submit">Upload file</button></div><div data-result></div></form></section>`
    : "";
  const down = can("file_read")
    ? `<section class="form-panel"><h2>Download a file</h2><form id="download" class="form-grid">${field("id", "File ID", "", { required: true })}<div><button class="btn" type="submit">Download</button></div><div data-result></div></form></section>`
    : "";
  const canExport = can("export_create"),
    exportOp = state.operations.get("export_create"),
    kindsExport = schema(exportOp.inputSchema.properties.kind).enum || [];
  const sendExport = canExport
    ? `<section class="form-panel"><h2>Export data</h2><form id="export" class="form-grid">${field("kind", "Export", kindsExport[0], { options: kindsExport })}${field("format", "Format", "csv", { options: schema(exportOp.inputSchema.properties.format).enum })}${field("subscriber_id", "Subscriber ID (optional)", "")}<div class="actions"><button class="btn primary" type="submit">Create export</button>${toolLink("jobs_list", "View jobs")}</div><div data-result></div></form></section>`
    : "";
  return {
    html:
      head("File transfers", "Workspace") +
      `<div class="two-col"><div class="stack">${upload}${sendExport}</div><div class="stack">${down}<section class="sub-panel"><h2>Migration</h2><p>Upload source files, review the import plan, then apply that plan.</p><div class="actions">${toolLink("import_prepare", "Prepare import")}${toolLink("import_commit", "Apply plan")}</div></section></div></div><p class="progress-text box-gap" id="transfer-progress" role="status"></p>`,
    mount() {
      fSubmit("upload", async (data, result) => {
        const file = data.get("file");
        if (!file.size || file.size > 25 * 1024 * 1024)
          throw new Error("Choose a file between one byte and 25 MiB.");
        const bytes = new Uint8Array(await file.arrayBuffer()),
          full = await hash(bytes);
        const start = await call("file_begin", {
          kind: data.get("kind"),
          size: bytes.length,
          sha256: full,
        });
        let offset = 0;
        while (offset < bytes.length) {
          const chunk = bytes.subarray(offset, offset + 49152);
          const row = await call("file_write", {
            id: start.id,
            offset,
            base64: chunkBase64(chunk),
            chunk_sha256: await hash(chunk),
          });
          if (row.next_offset !== offset + chunk.length)
            throw new Error("The server returned an unexpected file offset.");
          offset = row.next_offset;
          progress(
            `Uploaded ${offset.toLocaleString()} of ${bytes.length.toLocaleString()} bytes.`,
          );
        }
        const finished = await call("file_finish", { id: start.id });
        setResult(result, finished);
        progress("File ready.");
        if (can("import_prepare"))
          result.insertAdjacentHTML(
            "beforeend",
            `<div class="box-gap">${toolLink("import_prepare", "Prepare import")}</div>`,
          );
      });
      fSubmit("download", async (data, result) => {
        const id = data.get("id"),
          pieces = [];
        let next = 0,
          size = 0,
          total = 0,
          expected = "";
        do {
          const chunk = await call("file_read", {
            id,
            offset: next,
            length: 49152,
          });
          if (
            chunk.offset !== next ||
            chunk.size > 25 * 1024 * 1024 ||
            (expected && (chunk.sha256 !== expected || size !== chunk.size))
          )
            throw new Error("The file changed during download.");
          size = chunk.size;
          expected = chunk.sha256;
          const part = fromBase64(chunk.base64 || "");
          pieces.push(part);
          total += part.length;
          next = chunk.next_offset;
          if (
            total > size ||
            (next != null && (part.length === 0 || next !== total))
          )
            throw new Error("Invalid file chunk.");
          progress(
            `Downloaded ${total.toLocaleString()} of ${size.toLocaleString()} bytes.`,
          );
        } while (next != null);
        const file = new Blob(pieces);
        if (
          total !== size ||
          (await hash(await file.arrayBuffer())) !== expected
        )
          throw new Error("The downloaded file failed verification.");
        const url = URL.createObjectURL(file),
          a = document.createElement("a");
        a.href = url;
        a.download = id;
        a.click();
        setTimeout(() => URL.revokeObjectURL(url), 1000);
        result.textContent = "Download verified.";
        progress("");
      });
      fSubmit("export", async (data, result) => {
        const args = { kind: data.get("kind"), format: data.get("format") };
        if (data.get("subscriber_id"))
          args.subscriber_id = data.get("subscriber_id");
        const job = await call("export_create", args);
        setResult(result, job);
        result.insertAdjacentHTML(
          "beforeend",
          `<div class="box-gap"><a class="text-button" href="${href("tools", "job_get")}?id=${encodeURIComponent(job.id)}">Inspect export job ${icon("arrow")}</a></div>`,
        );
      });
    },
  };
}
