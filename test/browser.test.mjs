import assert from "node:assert/strict";
import { createHash, randomBytes } from "node:crypto";
import { existsSync } from "node:fs";
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { chromium } from "playwright-core";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { startRuntime, webClient } from "../scripts/runtime.mjs";

const chromiumPath =
  process.env.CHROME_PATH ||
  (existsSync(chromium.executablePath())
    ? chromium.executablePath()
    : [
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/google-chrome",
      ].find(existsSync));

test("a browser can sign in, grant or deny a connector, and sign out", async (t) => {
  const email = "operator@publisher.test";
  const clientId = "connector-test";
  const callback = "https://connector.example.test/oauth/callback";
  const runtime = await startRuntime({ bindings: {
    OPERATOR_AUTH_MODE: "email",
    OPERATOR_EMAILS: email,
    NEWSMAN_SECRET: randomBytes(40).toString("hex"),
    OAUTH_CLIENTS: JSON.stringify({ [clientId]: [callback] }),
  } });
  t.after(() => runtime.close());
  const browser = await chromium.launch({
    executablePath: chromiumPath, headless: true, args: ["--no-sandbox"],
  });
  t.after(() => browser.close());
  const page = await browser.newPage();
  page.setDefaultTimeout(7000);
  await page.route(url => url.origin + url.pathname === callback, async route => {
    assert.equal(route.request().method(), "GET");
    assert.equal(await route.request().headerValue("referer"), null);
    await route.fulfill({ contentType: "text/plain", body: "Connector received the authorization response." });
  });

  const { origin } = runtime;
  const resource = origin + "/mcp";
  const verifier = randomBytes(48).toString("base64url");
  const params = new URLSearchParams({
    client_id: clientId, redirect_uri: callback, response_type: "code", state: "browser-check",
    scope: "read", resource, code_challenge_method: "S256",
    code_challenge: createHash("sha256").update(verifier).digest("base64url"),
  });
  async function submit(button, path, status) {
    const pending = page.waitForResponse(response =>
      response.url() === origin + path && response.request().method() === "POST");
    await page.getByRole("button", { name: button, exact: true }).click();
    const response = await pending;
    assert.equal(await response.request().headerValue("origin"), origin);
    assert.equal(response.status(), status, path + " returned the wrong status");
  }
  async function decide(button) {
    await submit(button, "/oauth/authorize", 200);
    await page.getByText("Connector received the authorization response.").waitFor();
    const returned = new URL(page.url());
    assert.equal(returned.origin + returned.pathname, callback);
    assert.equal(returned.searchParams.get("iss"), origin);
    assert.equal(returned.searchParams.get("state"), params.get("state"));
    return returned.searchParams;
  }

  await page.goto(origin + "/oauth/authorize?" + params);
  await page.getByLabel("Email address").fill(email);
  await submit("Email me a code", "/login/request", 200);
  const db = await runtime.mf.getD1Database("DB");
  const captured = await db.prepare(
    "SELECT json_extract(body,'$.text') AS message FROM newsman_records WHERE kind='capture' AND json_extract(body,'$.to')=?"
  ).bind(email).first("message");
  await page.getByLabel("Email code").fill(captured.match(/\b\d{8}\b/)[0]);
  await submit("Sign in", "/login/verify", 303);
  const approved = await decide("Allow");
  assert.ok(approved.get("code"));
  const exchanged = await fetch(origin + "/oauth/token", {
    method: "POST",
    body: new URLSearchParams({
      grant_type: "authorization_code", client_id: clientId, redirect_uri: callback,
      code: approved.get("code"), code_verifier: verifier, resource,
    }),
  });
  assert.equal(exchanged.status, 200);
  const { access_token } = await exchanged.json();
  const client = new Client({ name: "browser-test", version: "1" });
  t.after(() => client.close());
  await client.connect(new StreamableHTTPClientTransport(new URL(resource), {
    requestInit: { headers: { Authorization: "Bearer " + access_token } },
  }));
  const issues = await client.callTool({ name: "issues_list", arguments: {} });
  assert.equal(issues.isError ?? false, false);
  assert.deepEqual(JSON.parse(issues.content[0].text).items, []);

  params.set("state", "denial-check");
  await page.goto(origin + "/oauth/authorize?" + params);
  const denied = await decide("Deny");
  assert.equal(denied.get("error"), "access_denied");
  assert.equal(denied.has("code"), false);
  await page.goto(origin);
  await submit("Sign out", "/login/logout", 303);
  await page.getByRole("button", { name: "Email me a code" }).waitFor();
  assert.equal((await page.request.get(origin + "/api/session")).status(), 401);
});

test("a connector can redirect to its canonical host after browser consent", async (t) => {
  const received = [];
  // Mirror Grok's published www callback and its 308 to a different origin entirely on loopback.
  const connector = createServer((req, res) => {
    const url = new URL(req.url, "http://" + req.headers.host);
    if (url.pathname !== "/oauth/callback") {
      res.writeHead(404).end();
      return;
    }
    received.push({ host: url.hostname, method: req.method, referer: req.headers.referer });
    if (url.hostname === "localhost") {
      url.hostname = "127.0.0.1";
      res.writeHead(308, { Location: url.href }).end();
    } else {
      res.writeHead(200, { "Content-Type": "text/plain" }).end("Connector callback complete");
    }
  });
  await new Promise(resolve => connector.listen(0, "127.0.0.1", resolve));
  t.after(() => new Promise(resolve => connector.close(resolve)));
  const callback = "http://localhost:" + connector.address().port + "/oauth/callback";
  const clientId = "redirecting-connector";
  const runtime = await startRuntime({ bindings: {
    OAUTH_CLIENTS: JSON.stringify({ [clientId]: [callback] }),
  } });
  t.after(() => runtime.close());
  const browser = await chromium.launch({ executablePath: chromiumPath, args: ["--no-sandbox"] });
  t.after(() => browser.close());
  const page = await browser.newPage();
  page.setDefaultTimeout(7000);
  const { origin } = runtime;
  const resource = origin + "/mcp";
  const verifier = randomBytes(48).toString("base64url");

  for (const choice of ["Allow", "Deny"]) {
    const state = randomBytes(16).toString("hex");
    const params = new URLSearchParams({
      response_type: "code", client_id: clientId, redirect_uri: callback, state, scope: "read", resource,
      code_challenge_method: "S256", code_challenge: createHash("sha256").update(verifier).digest("base64url"),
    });
    const consentPage = await page.goto(origin + "/oauth/authorize?" + params);
    assert.match(await consentPage.headerValue("content-security-policy"), /form-action 'self';/);
    const submitted = page.waitForRequest(r => r.url() === origin + "/oauth/authorize" && r.method() === "POST");
    await page.getByRole("button", { name: choice, exact: true }).click();
    await page.getByText("Connector callback complete").waitFor();
    const returned = new URL(page.url()).searchParams;
    assert.equal(returned.get("iss"), origin);
    assert.equal(returned.get("state"), state);
    if (choice === "Allow") {
      assert.ok(returned.get("code"));
      const exchange = () => fetch(origin + "/oauth/token", { method: "POST", body: new URLSearchParams({
        grant_type: "authorization_code", client_id: clientId, redirect_uri: callback,
        code: returned.get("code"), code_verifier: verifier, resource,
      }) });
      assert.ok((await (await exchange()).json()).access_token);
      assert.equal((await (await exchange()).json()).error, "invalid_grant");
      const replay = await page.request.post(origin + "/oauth/authorize", {
        headers: { Accept: "text/html", Origin: origin, "Content-Type": "application/x-www-form-urlencoded" },
        data: (await submitted).postData(),
      });
      assert.equal(replay.status(), 400);
      assert.match(await replay.text(), /Return to your connector and start a new connection/);
    } else {
      assert.equal(returned.get("error"), "access_denied");
      assert.equal(returned.has("code"), false);
    }
  }
  assert.deepEqual(received.map(r => r.host), ["localhost", "127.0.0.1", "localhost", "127.0.0.1"]);
  assert.ok(received.every(r => r.method === "GET" && r.referer === undefined));
});

test(
  "operator console uses the Worker for editing, audience, transfers and MCP tools",
  { timeout: 90000 },
  async (t) => {
    assert.ok(
      chromiumPath,
      "Install Chromium or set CHROME_PATH for the browser suite.",
    );
    const runtime = await startRuntime();
    t.after(() => runtime.close());
    const browser = await chromium.launch({
      executablePath: chromiumPath,
      headless: true,
      args: ["--no-sandbox"],
    });
    t.after(() => browser.close());
    const context = await browser.newContext({
      viewport: { width: 1280, height: 880 },
      acceptDownloads: true,
    });
    const page = await context.newPage();
    page.setDefaultTimeout(7000);
    const issues = [];
    page.on("pageerror", (error) => issues.push(error.message));
    const base = runtime.origin;
    const web = await webClient(base);
    const registry = await fetch(base + "/api/registry").then((x) => x.json());
    const wait = async (selector) => page.locator(selector).first().waitFor();
    async function open(path, expectedSelector) {
      await page.goto(base + "/#/" + path);
      await wait(expectedSelector);
      await page.locator('main[aria-busy="false"]').waitFor();
    }

    await open("overview", ".feature");
    assert.match(await page.locator("main").textContent(), /No messages yet/);
    assert.equal((await web("issues_list")).items.length, 0);
    await page.getByRole("link", { name: "New message", exact: true }).click();
    await page.getByLabel("Internal message title").fill("Workshop dispatch");
    await page.getByLabel("Email subject").fill("A new field workshop");
    await page.getByLabel("Preview text").fill("Details from the studio.");
    await page.getByLabel("Email body").fill("The doors open on Friday.");
    await page.getByRole("button", { name: "Save draft", exact: true }).click();
    await page
      .getByRole("button", { name: "Save revision", exact: true })
      .waitFor();
    const saved = (await web("issues_list")).items[0];
    assert.equal(saved.title, "Workshop dispatch");
    await page.getByLabel("Email subject").fill("Workshop registration opens");
    await page
      .getByRole("button", { name: "Save revision", exact: true })
      .click();
    await page.waitForFunction(() =>
      document.querySelector(".definition")?.textContent.includes("v2"),
    );
    assert.equal(
      (await web("issue_get", { id: saved.id })).draft.subject,
      "Workshop registration opens",
    );
    await page
      .getByRole("button", { name: "Preview email", exact: true })
      .click();
    await page
      .getByRole("dialog")
      .getByRole("heading", { name: "Email preview" })
      .waitFor();
    await page
      .frameLocator("iframe.frame")
      .getByText("The doors open on Friday.")
      .waitFor();
    await page.getByRole("button", { name: "Close", exact: true }).click();

    await page.goto(base + "/subscribe/");
    await page.getByLabel("Email address").fill("reader@example.test");
    const signup = page.waitForResponse(response =>
      response.url() === base + "/api/newsletter/subscribe" && response.request().method() === "POST");
    await page.getByRole("button", { name: "Subscribe", exact: true }).click();
    assert.equal((await signup).status(), 303);
    await open("audience", "#filter-form");
    await page
      .getByRole("textbox", { name: "Search subscribers" })
      .fill("reader@example.test");
    await page.getByRole("button", { name: "Filter", exact: true }).click();
    await page
      .getByRole("link", { name: "reader@example.test", exact: true })
      .click();
    await page.getByLabel("Tags", { exact: false }).fill("studio, release");
    const changed = page.waitForResponse(
      (r) =>
        r.url().endsWith("/api/ops/subscriber_tags_set") &&
        r.request().method() === "POST",
    );
    await page.getByRole("button", { name: "Save tags" }).click();
    const tagged = await changed;
    assert.equal(tagged.status(), 200);
    const person = (
      await web("subscribers_list", { search: "reader@example.test" })
    ).items[0];
    assert.deepEqual([...person.tags].sort(), ["release", "studio"]);

    await open("tools/dashboard_get", "#op-form");
    const visible = await page.locator(".tool-link").allTextContents();
    assert.deepEqual(
      visible.sort(),
      registry.operations
        .filter((o) => o.surface === "operator")
        .map((o) => o.name)
        .sort(),
    );
    await page.getByRole("button", { name: "Run query" }).click();
    await wait("#op-result pre");
    assert.ok(
      Array.isArray(
        JSON.parse(await page.locator("#op-result pre").textContent()).metrics,
      ),
    );
    await page.getByLabel("Arguments (JSON)").fill("[");
    await page.getByRole("button", { name: "Run query" }).click();
    await wait('#op-error [role="alert"]');

    await open("transfers", "#upload");
    const bytes = Buffer.from(
      JSON.stringify({ format_version: 1, notes: "x".repeat(50000) }),
    );
    await page.getByLabel("File purpose").selectOption("issue_bundle");
    await page.locator("#upload input[type=file]").setInputFiles({
      name: "issue.json",
      mimeType: "application/json",
      buffer: bytes,
    });
    await page
      .getByRole("button", { name: "Upload file", exact: true })
      .click();
    await wait("#upload [data-result] pre");
    const file = JSON.parse(
      await page.locator("#upload [data-result] pre").textContent(),
    );
    await page.getByLabel("File ID", { exact: true }).fill(file.id);
    const transfer = page.waitForEvent("download");
    await page.getByRole("button", { name: "Download", exact: true }).click();
    const downloaded = await transfer;
    assert.deepEqual(await readFile(await downloaded.path()), bytes);

    await open("operators", "#operator-form");
    await page
      .getByLabel("Verified email address")
      .fill("editing@example.test");
    await page.getByRole("button", { name: "Add operator" }).click();
    const added = page
      .getByRole("row")
      .filter({ hasText: "editing@example.test" });
    await added.waitFor();
    await added.getByRole("button", { name: "Remove", exact: true }).click();
    await page
      .getByRole("dialog")
      .getByRole("button", { name: "Remove operator", exact: true })
      .click();
    await page.waitForFunction(() => !document.querySelector("dialog[open]"));
    assert.equal(
      (await web("operators_list")).items.find(
        (op) => op.email === "editing@example.test",
      )?.state,
      "removed",
    );
    await open("connections", ".connection-card");
    assert.equal(
      await page.locator(".code-bar code").textContent(),
      base + "/mcp",
    );
    assert.ok(await page.getByRole("button", { name: "Copy URL" }).isEnabled());
    assert.equal(
      Number(await page.locator(".tool-tally strong").textContent()),
      registry.operations.filter((o) => o.surface === "operator").length,
    );
    for (const scope of ["read", "write", "send", "admin"]) {
      assert.ok(
        await page
          .locator(".scope-grants")
          .getByText(scope, { exact: true })
          .isVisible(),
      );
    }
    for (const width of [1280, 820, 390, 320]) {
      await page.setViewportSize({ width, height: 800 });
      for (const [path, selector] of [
        ["overview", ".feature"],
        ["issues", "#filter-form"],
        ["audience", "#filter-form"],
        ["compose/" + saved.id, "#draft-form"],
        ["delivery", ".panel"],
        ["reports", "#report-filters"],
        ["settings", "#settings-form"],
        ["operators", "#operator-form"],
        ["connections", ".connection-card"],
        ["tools/send_prepare", "#op-form"],
        ["transfers", "#upload"],
      ]) {
        await open(path, selector);
        assert.ok(
          await page.evaluate(
            () => document.documentElement.scrollWidth <= window.innerWidth + 1,
          ),
          "Horizontal overflow on " + path + " at " + width + "px",
        );
      }
    }
    await page.getByRole("button", { name: "Menu", exact: true }).click();
    await page
      .getByRole("navigation", { name: "Main navigation" })
      .getByRole("link", { name: "MCP connections" })
      .click();
    await wait(".connection-card");
    assert.equal(
      await page
        .getByRole("button", { name: "Menu", exact: true })
        .getAttribute("aria-expanded"),
      "false",
    );
    assert.deepEqual(issues, []);
  },
);
