import assert from "node:assert/strict";
import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { mkdir } from "node:fs/promises";

const native = process.argv.includes("--native");
let server;
let browser;
const errors = [];
try {
  let context, page;
  if (native) {
    browser = await chromium.connectOverCDP("http://127.0.0.1:9223");
    context = browser.contexts()[0];
    page =
      context.pages().find((p) => p.url().includes("tauri.localhost")) ??
      context.pages()[0];
    await page.bringToFront();
  } else {
    server = spawn(
      process.execPath,
      [
        "node_modules/vite/bin/vite.js",
        "--host",
        "127.0.0.1",
        "--port",
        "1421",
        "--strictPort",
      ],
      { stdio: "ignore", windowsHide: true },
    );
    for (let attempt = 0; attempt < 60; attempt++) {
      try {
        if ((await fetch("http://127.0.0.1:1421")).ok) break;
      } catch {}
      await new Promise((r) => setTimeout(r, 250));
    }
    browser = await chromium.launch({ channel: "msedge", headless: true });
    context = await browser.newContext({
      viewport: { width: 1280, height: 860 },
    });
    await context.addInitScript(() => {
      window.isTauri = true;
      const profile = {
        id: "test",
        name: "Cyberpunk 2077",
        executable_path: "C:\\Games\\Cyberpunk2077.exe",
        game_priority: "AboveNormal",
        reduce_background_memory: false,
        managed_executables: [],
        excluded_executables: [],
        restore_when_game_exits: true,
      };
      let store = {
        schema_version: 1,
        selected_profile: "test",
        profiles: [profile],
      };
      window.__TAURI_INTERNALS__ = {
        invoke: async (command, args) => {
          if (command === "load_profiles") return structuredClone(store);
          if (command === "save_profiles") {
            store = structuredClone(args.store);
            window.__saved = store;
            return;
          }
          if (command === "get_icon") return null;
          if (command === "get_focus_status")
            return {
              state: { kind: "idle" },
              target: null,
              profile_name: null,
              outcomes: [],
              errors: [],
              applied_changes: 0,
            };
          if (command === "apply_profile")
            return {
              state: { kind: "idle" },
              target: null,
              profile_name: args.profile?.name ?? null,
              outcomes: [],
              errors: [],
              applied_changes: 0,
            };
          if (command === "get_snapshot")
            return {
              cpu: 42,
              memory: [13 * 1073741824, 32 * 1073741824],
              demo: false,
              processes: [
                {
                  identity: { pid: 101, creation_time_100ns: "1001" },
                  name: "Cyberpunk2077.exe",
                  executable_path: "C:\\Games\\Cyberpunk2077.exe",
                  cpu_percent: 32,
                  working_set_bytes: 8000000000,
                  capability: "Candidate",
                },
                {
                  identity: { pid: 102, creation_time_100ns: "1002" },
                  name: "chrome.exe",
                  executable_path: "C:\\Program Files\\Chrome\\chrome.exe",
                  cpu_percent: 5,
                  working_set_bytes: 600000000,
                  capability: "Candidate",
                },
                {
                  identity: { pid: 103, creation_time_100ns: "1003" },
                  name: "System.exe",
                  executable_path: "C:\\Windows\\System.exe",
                  cpu_percent: 1,
                  working_set_bytes: 100000000,
                  capability: "SystemProtected",
                },
              ],
            };
          if (command === "choose_executable")
            return {
              ...profile,
              id: "added",
              name: "New game",
              executable_path: "C:\\Games\\New.exe",
            };
          throw new Error(command);
        },
      };
    });
    page = await context.newPage();
    page.on("pageerror", (error) => { console.error("Startup error:", String(error)); errors.push(String(error)); });
    page.on("console", (message) => { if(message.type()==="error") console.error("Browser:",message.text()); });
    await page.goto("http://127.0.0.1:1421");
  }
  page.on("pageerror", (error) => errors.push(String(error)));
  await page.locator("#cpu-value").waitFor();
  await page.waitForFunction(
    () => document.querySelector("#cpu-value")?.textContent !== "—",
  );
  assert(
    await page
      .getByRole("button", { name: "Start Focus Mode", exact: true })
      .isDisabled(),
  );
  await page.locator('[data-page="processes"]').click();
  await page.waitForFunction(
    () =>
      document.querySelectorAll("#process-body tr .process-name").length > 0,
  );
  const count = await page.locator("#process-body tr .process-name").count();
  await page.locator("#search").fill("no-process-matches-this-foqus-query");
  assert.match(await page.locator("#process-body").innerText(), /No matching/);
  await page.locator("#search").fill("");
  await page.locator('[data-sort="memory"]').click();
  await page.locator("#process-filter").selectOption("protected");
  const badges = await page.locator("#process-body .badge").allTextContents();
  assert(badges.every((text) => text === "System protected"));
  await page.locator("#process-filter").selectOption("all");
  await page
    .locator('#process-body tr')
    .first()
    .locator('[data-policy="manage"]')
    .click();
  await page.waitForFunction(
    () => window.__saved?.profiles[0].managed_executables.length === 1,
  );
  assert.match(
    await page.locator("#process-body").innerText(),
    /Saved · Manage/,
  );
  await page
    .locator('#process-body tr')
    .first()
    .locator('[data-policy="auto"]')
    .click();
  await page.waitForFunction(
    () => window.__saved?.profiles[0].managed_executables.length === 0,
  );
  assert.match(await page.locator("#process-body").innerText(), /Not applied/);
  await mkdir("artifacts", { recursive: true });
  for (const width of [1000, 1280]) {
    await page.setViewportSize({ width, height: 760 });
    for (const name of ["overview", "processes", "profiles"]) {
      await page.locator('[data-page="' + name + '"]').click();
      await page.waitForTimeout(100);
      const overflow = await page
        .locator("#page")
        .evaluate((el) => el.scrollWidth > el.clientWidth + 1);
      assert(!overflow, name + " overflows at " + width);
      await page.screenshot({
        path:
          "artifacts/" +
          (native ? "native-" : "") +
          name +
          "-" +
          width +
          ".png",
      });
    }
  }
  if (!native) {
    assert.match(
      await page.locator("#apply-state").innerText(),
      /next time Focus Mode starts/,
    );
    await page.locator("#memory-setting").check();
    await page.waitForFunction(
      () => window.__saved?.profiles[0].reduce_background_memory === true,
    );
    await page.locator('[data-action="exceptions"]').click();
    await page.locator("[data-exception]").first().selectOption("exclude");
    await page.locator('[data-action="save-exceptions"]').click();
    await page.waitForFunction(
      () => window.__saved?.profiles[0].excluded_executables.length === 1,
    );
    await page.locator('[data-action="add-profile"]').first().click();
    await page.waitForFunction(() => window.__saved?.profiles.length === 2);
    await page.locator('[data-action="remove-profile"]').click();
    await page.locator('[data-action="confirm-remove"]').click();
    await page.waitForFunction(() => window.__saved?.profiles.length === 1);
    await page.locator('[data-page="processes"]').click();
    await page.locator("#search").fill("<img src=x onerror=alert(1)>");
    assert.equal(await page.locator("#process-body img").count(), 0);
  }
  assert.deepEqual(errors, []);
  if (native) {
    const result = await page.evaluate(async () => {
      const snapshot = await window.__TAURI_INTERNALS__.invoke("get_snapshot");
      const profiles = await window.__TAURI_INTERNALS__.invoke("load_profiles");
      const process =
        snapshot.processes.find((p) => p.name === "foqus-app.exe") ??
        snapshot.processes.find((p) => p.executable_path);
      const icon = process
        ? await window.__TAURI_INTERNALS__.invoke("get_icon", {
            path: process.executable_path,
          })
        : null;
      return {
        identityType: typeof process?.identity.creation_time_100ns,
        schema: profiles.schema_version,
        icon: !!icon?.startsWith("data:image/png;base64,"),
      };
    });
    assert.equal(result.identityType, "string");
    assert.equal(result.schema, 1);
    assert.equal(result.icon, true);
  }
  console.log(
    JSON.stringify(
      {
        mode: native ? "Tauri native" : "mock IPC",
        processes: count,
        pages: 3,
        widths: [1000, 1280],
        errors,
      },
      null,
      2,
    ),
  );
} finally {
  if (browser) await browser.close();
  server?.kill();
}
