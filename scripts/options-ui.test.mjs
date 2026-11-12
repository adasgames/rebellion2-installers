import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

const template = readFileSync(
  new URL("../launcher/ui/options.html", import.meta.url),
  "utf8",
);
const script = template
  .match(/<script>([\s\S]*?)<\/script>/)[1]
  .replaceAll("@@ACT@@", "https://launcher.invalid/act");

function optionsDialog(audio = true) {
  const elements = new Map();
  for (const id of [
    "launcher-options",
    "launcher-options-form",
    "graphics-api",
    "options-error",
    "options-save",
    ...(audio ? ["audio-backend"] : []),
  ]) {
    const element = {
      value: "",
      textContent: "",
      disabled: false,
      open: false,
      listeners: {},
      addEventListener(name, listener) {
        this.listeners[name] = listener;
      },
      focus() {},
      showModal() {
        this.open = true;
      },
      close() {
        this.open = false;
      },
    };
    elements.set(id, element);
  }
  const window = { location: { href: "" } };
  const document = { getElementById: (id) => elements.get(id) ?? null };
  vm.runInNewContext(script, { window, document });
  return { elements, window };
}

test("opening options restores both saved selections", () => {
  const { elements, window } = optionsDialog();
  window.rebOpenOptions("vulkan", "pipewire", "");
  assert.equal(elements.get("graphics-api").value, "vulkan");
  assert.equal(elements.get("audio-backend").value, "pipewire");
  assert.equal(elements.get("launcher-options").open, true);
});

test("saving options submits graphics and audio together", () => {
  const { elements, window } = optionsDialog();
  window.rebOpenOptions("vulkan", "system", "");
  elements.get("audio-backend").value = "pipewire";
  let prevented = false;
  elements.get("launcher-options-form").listeners.submit({
    preventDefault() {
      prevented = true;
    },
  });
  assert.equal(prevented, true);
  assert.equal(
    new URL(window.location.href).searchParams.get("choice"),
    "save-options:vulkan:pipewire",
  );
  assert.equal(elements.get("options-save").disabled, true);
});

test("platforms without an audio selector retain automatic audio", () => {
  const { elements, window } = optionsDialog(false);
  window.rebOpenOptions("metal", "auto", "");
  elements
    .get("launcher-options-form")
    .listeners.submit({ preventDefault() {} });
  assert.equal(
    new URL(window.location.href).searchParams.get("choice"),
    "save-options:metal:auto",
  );
});

test("a save failure keeps both selections and the dialog open", () => {
  const { elements, window } = optionsDialog();
  window.rebOpenOptions("vulkan", "pipewire", "");
  window.rebOptionsSaved("Could not save options.");
  assert.equal(elements.get("launcher-options").open, true);
  assert.equal(
    elements.get("options-error").textContent,
    "Could not save options.",
  );
  assert.equal(elements.get("audio-backend").value, "pipewire");
  assert.equal(elements.get("options-save").disabled, false);
  window.rebOptionsSaved("");
  assert.equal(elements.get("launcher-options").open, false);
});
