return (async () => {
  let stage = "boot";
  try {
    const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    const waitFor = async (probe, message, timeout = 15000) => {
      const deadline = Date.now() + timeout;
      while (Date.now() < deadline) {
        const value = probe();
        if (value) return value;
        await sleep(50);
      }
      throw new Error(message);
    };
    const inputValueSetter = Object.getOwnPropertyDescriptor(
      HTMLInputElement.prototype,
      "value",
    ).set;
    const fill = (selector, value) => {
      const input = document.querySelector(selector);
      if (!input) throw new Error(`missing input ${selector}`);
      inputValueSetter.call(input, value);
      input.dispatchEvent(
        new InputEvent("input", {
          bubbles: true,
          cancelable: true,
          data: value,
          inputType: "insertText",
        }),
      );
    };
    const buttonByText = (text) =>
      [...document.querySelectorAll("button")].find(
        (button) => button.textContent.trim() === text,
      );
    const clickButton = async (text) => {
      const button = await waitFor(
        () => buttonByText(text),
        `button not found: ${text}`,
      );
      button.click();
      await sleep(80);
      return button;
    };
    const clickText = async (selector, text) => {
      const element = await waitFor(
        () =>
          [...document.querySelectorAll(selector)].find(
            (entry) => entry.textContent.trim() === text,
          ),
        `${selector} text not found: ${text}`,
      );
      element.click();
      await sleep(80);
      return element;
    };
    const parseColor = (value) => {
      const channels = value.match(/[\d.]+/g)?.map(Number);
      if (!channels || channels.length < 3)
        throw new Error(`unable to parse computed color: ${value}`);
      return {
        red: channels[0],
        green: channels[1],
        blue: channels[2],
        alpha: channels[3] ?? 1,
      };
    };
    const luminance = ({ red, green, blue }) => {
      const linear = [red, green, blue].map((channel) => {
        const normalized = channel / 255;
        return normalized <= 0.04045
          ? normalized / 12.92
          : ((normalized + 0.055) / 1.055) ** 2.4;
      });
      return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2];
    };
    const contrastRatio = (foreground, background) => {
      const foregroundLuminance = luminance(parseColor(foreground));
      const backgroundLuminance = luminance(parseColor(background));
      const lighter = Math.max(foregroundLuminance, backgroundLuminance);
      const darker = Math.min(foregroundLuminance, backgroundLuminance);
      return (lighter + 0.05) / (darker + 0.05);
    };
    const clickTitle = async (title) => {
      const element = await waitFor(
        () => document.querySelector(`[title="${title}"]`),
        `title not found: ${title}`,
      );
      element.dispatchEvent(
        new MouseEvent("click", {
          button: 0,
          bubbles: true,
          cancelable: true,
          composed: true,
        }),
      );
      await sleep(80);
      return element;
    };
    const ensureZone = async (zone, toggleTitle) => {
      let element = document.querySelector(
        `[data-rusterm-dock-zone="${zone}"]`,
      );
      if (!element) {
        await clickTitle(toggleTitle);
        try {
          element = await waitFor(
            () => document.querySelector(`[data-rusterm-dock-zone="${zone}"]`),
            `${zone} dock did not open`,
            3000,
          );
        } catch (error) {
          const zones = [
            ...document.querySelectorAll("[data-rusterm-dock-zone]"),
          ].map((entry) => entry.getAttribute("data-rusterm-dock-zone"));
          const toggle = document.querySelector(`[title="${toggleTitle}"]`);
          throw new Error(
            `${error.message}; zones=${JSON.stringify(zones)}; toggle=${toggle ? toggle.outerHTML : "missing"}`,
          );
        }
      }
      return element;
    };
    const activateDockTab = async (label) => {
      const tab = await clickTitle(`Drag to reorder or move ${label}`);
      await waitFor(
        () =>
          tab.classList.contains("active") ||
          document.querySelector(
            `[title="Drag to reorder or move ${label}"].active`,
          ),
        `${label} dock tab did not activate`,
      );
    };
    const typeKeys = async (terminal, text) => {
      terminal.focus();
      for (const key of text) {
        terminal.dispatchEvent(
          new KeyboardEvent("keydown", {
            key,
            code: key === " " ? "Space" : "",
            bubbles: true,
            cancelable: true,
          }),
        );
        await sleep(2);
      }
    };
    const typeCommand = async (terminal, command) => {
      await typeKeys(terminal, command);
      terminal.dispatchEvent(
        new KeyboardEvent("keydown", {
          key: "Enter",
          code: "Enter",
          bubbles: true,
          cancelable: true,
        }),
      );
    };
    const sendCommand = async (terminal, command, marker) => {
      await typeCommand(terminal, command);
      await waitFor(
        () => terminal.textContent.includes(marker),
        `terminal did not render marker ${marker}`,
      );
    };

    stage = "wait-first-run";
    await waitFor(
      () => document.querySelector('input[placeholder="Enter password"]'),
      "first-run password input did not render",
    );
    stage = "fill-password";
    fill('input[placeholder="Enter password"]', "rusterm-e2e");
    fill('input[placeholder="Confirm password"]', "rusterm-e2e");
    const createButton = await waitFor(
      () => buttonByText("Create & Unlock"),
      "Create & Unlock did not render",
    );
    await waitFor(
      () => !createButton.disabled,
      "Create & Unlock stayed disabled",
    );
    stage = "unlock";
    createButton.click();

    await waitFor(
      () => document.querySelector("#main"),
      "workspace did not render after unlock",
    );
    await sleep(750);

    stage = "settings-readable-with-low-contrast-skin";
    await clickText("span", "Settings");
    await waitFor(
      () => document.querySelector('[data-rusterm-settings-panel="true"]'),
      "settings panel did not open",
    );
    await clickButton("Custom");
    for (const [label, value] of [
      ["Background", "#080808"],
      ["Surface", "#101010"],
      ["Text", "#101010"],
      ["Muted text", "#101010"],
    ]) {
      fill(`[data-rusterm-skin-color="${label}"] input`, value);
    }
    await clickButton("Save");
    await waitFor(
      () => !document.querySelector('[data-rusterm-settings-overlay="true"]'),
      "settings panel did not close after saving custom skin",
    );
    await clickText("span", "Settings");
    const settingsPanel = await waitFor(
      () => document.querySelector('[data-rusterm-settings-panel="true"]'),
      "settings panel did not reopen with custom skin",
    );
    const settingsOverlay = document.querySelector(
      '[data-rusterm-settings-overlay="true"]',
    );
    const overlayStyle = getComputedStyle(settingsOverlay);
    const panelStyle = getComputedStyle(settingsPanel);
    if (parseColor(overlayStyle.backgroundColor).alpha < 0.75)
      throw new Error(
        `settings overlay is too transparent: ${overlayStyle.backgroundColor}`,
      );
    if (parseColor(panelStyle.backgroundColor).alpha < 0.98)
      throw new Error(
        `settings panel is not opaque: ${panelStyle.backgroundColor}`,
      );
    if (Number.parseInt(overlayStyle.zIndex, 10) < 1000)
      throw new Error(
        `settings overlay z-index is too low: ${overlayStyle.zIndex}`,
      );
    if (overlayStyle.pointerEvents === "none")
      throw new Error("settings overlay does not block workspace interaction");
    const heading = settingsPanel.querySelector("h3");
    const bodyCopy = settingsPanel.querySelector("p");
    const fieldLabel = [...settingsPanel.querySelectorAll("label")].find(
      (label) => label.textContent.trim() === "Outline width",
    );
    for (const [element, name] of [
      [heading, "heading"],
      [bodyCopy, "body copy"],
      [fieldLabel, "field label"],
    ]) {
      const ratio = contrastRatio(
        getComputedStyle(element).color,
        panelStyle.backgroundColor,
      );
      if (ratio < 4.5)
        throw new Error(`settings ${name} contrast is ${ratio.toFixed(2)}:1`);
    }
    const overlayRect = settingsOverlay.getBoundingClientRect();
    const cornerTarget = document.elementFromPoint(
      overlayRect.left + 4,
      overlayRect.top + 4,
    );
    if (!cornerTarget?.closest('[data-rusterm-settings-overlay="true"]'))
      throw new Error(
        "settings overlay does not cover the workspace hit target",
      );
    await clickButton("Reset default");
    await clickButton("Save");
    await waitFor(
      () => !document.querySelector('[data-rusterm-settings-overlay="true"]'),
      "settings panel did not close after restoring defaults",
    );

    stage = "ensure-docks";
    await ensureZone("left", "Show or hide the left connection/file dock");
    await ensureZone("right", "Show or hide Sessions and History");
    await ensureZone("bottom", "Show or hide Send, Shell, and Transfers");

    stage = "activate-tabs";
    for (const label of [
      "Connections",
      "Remote files",
      "Sessions",
      "History",
      "Send",
      "Shell",
      "Transfers",
    ]) {
      await activateDockTab(label);
    }

    stage = "open-main-shell";
    await clickTitle("Open a local shell (zsh/bash)");
    const mainTerminal = await waitFor(
      () => document.querySelector('#terminal-content [id^="terminal-input-"]'),
      "main local terminal did not open",
    );
    await sendCommand(
      mainTerminal,
      "printf RUSTERM_MAIN_E2E_OK",
      "RUSTERM_MAIN_E2E_OK",
    );

    await sleep(750);
    stage = "suggestion-delete-click";
    const suggestionCommand = "printf RUSTERM_SUGGEST_DELETE_E2E";
    const suggestionPrefix = "printf RUSTERM_SUGGEST_DEL";
    await sendCommand(
      mainTerminal,
      suggestionCommand,
      "RUSTERM_SUGGEST_DELETE_E2E",
    );
    await typeKeys(mainTerminal, suggestionPrefix);
    const deleteSuggestionButton = await waitFor(
      () =>
        [
          ...document.querySelectorAll(
            'button[aria-label="Remove command from history"]',
          ),
        ].find((button) =>
          button.closest(".sug-row")?.textContent.includes(suggestionCommand),
        ),
      "suggestion delete button did not render",
    );
    return "ok";
  } catch (error) {
    return `error:${stage}:${error && error.message ? error.message : String(error)}:${error && error.stack ? error.stack : "no-stack"}`;
  }
})();
