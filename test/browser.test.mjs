import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { chromium } from "playwright-core";
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

    const signup = await fetch(base + "/api/newsletter/subscribe", {
      method: "POST",
      redirect: "manual",
      headers: { Origin: base },
      body: new URLSearchParams({ email: "reader@example.test" }),
    });
    assert.equal(signup.status, 303);
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
