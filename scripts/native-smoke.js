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
    const typeCommand = async (terminal, command) => {
      terminal.focus();
      for (const key of command) {
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

    stage = "resize-left";
    const leftBefore = document
      .querySelector('[data-rusterm-dock-zone="left"]')
      .getBoundingClientRect().width;
    const leftHandle = document.querySelector(
      '[data-rusterm-dock-zone="left"] .dock-resize-handle',
    );
    const handleRect = leftHandle.getBoundingClientRect();
    leftHandle.dispatchEvent(
      new MouseEvent("mousedown", {
        button: 0,
        clientX: handleRect.left + 1,
        clientY: handleRect.top + 30,
        bubbles: true,
        cancelable: true,
      }),
    );
    const activeHandle = await waitFor(
      () =>
        document.querySelector(
          '[data-rusterm-dock-zone="left"] .dock-resize-handle.active',
        ),
      "left resize did not start",
    );
    const resizeOverlay = activeHandle.previousElementSibling;
    resizeOverlay.dispatchEvent(
      new MouseEvent("mousemove", {
        button: 0,
        buttons: 1,
        clientX: handleRect.left + 41,
        clientY: handleRect.top + 30,
        bubbles: true,
        cancelable: true,
      }),
    );
    resizeOverlay.dispatchEvent(
      new MouseEvent("mouseup", {
        button: 0,
        clientX: handleRect.left + 41,
        clientY: handleRect.top + 30,
        bubbles: true,
        cancelable: true,
      }),
    );
    await waitFor(
      () =>
        document
          .querySelector('[data-rusterm-dock-zone="left"]')
          .getBoundingClientRect().width >
        leftBefore + 20,
      "left dock width did not grow",
    );

    stage = "drag-remote-files";
    const sourceTab = document.querySelector(
      '[title="Drag to reorder or move Remote files"]',
    );
    const sourceRect = sourceTab.getBoundingClientRect();
    const rightTabs = document.querySelector(
      '[data-rusterm-dock-zone="right"] [data-rusterm-dock-tabs="true"]',
    );
    const targetRect = rightTabs.getBoundingClientRect();
    sourceTab.dispatchEvent(
      new MouseEvent("mousedown", {
        button: 0,
        clientX: sourceRect.left + sourceRect.width / 2,
        clientY: sourceRect.top + sourceRect.height / 2,
        bubbles: true,
        cancelable: true,
      }),
    );
    await sleep(120);
    document.dispatchEvent(
      new MouseEvent("mousemove", {
        button: 0,
        buttons: 1,
        clientX: targetRect.right - 10,
        clientY: targetRect.top + targetRect.height / 2,
        bubbles: true,
        cancelable: true,
      }),
    );
    await sleep(80);
    document.dispatchEvent(
      new MouseEvent("mouseup", {
        button: 0,
        clientX: targetRect.right - 10,
        clientY: targetRect.top + targetRect.height / 2,
        bubbles: true,
        cancelable: true,
      }),
    );
    await waitFor(
      () =>
        document.querySelector(
          '[data-rusterm-dock-zone="right"] [title="Drag to reorder or move Remote files"]',
        ),
      "Remote files did not move to the right dock",
    );
    await waitFor(
      () =>
        window._rusterm_dock_drag_remove == null &&
        document.body.style.userSelect !== "none",
      "dock drag listener or text-selection suppression remained active",
    );
    if (!document.querySelector("#main"))
      throw new Error("center workspace disappeared after dock move");

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
    stage = "dangerous-command-cancel";
    await typeCommand(mainTerminal, "mkfs.ext4 /dev/sda");
    await waitFor(
      () => document.body.textContent.includes("⚠ 高危命令确认"),
      "dangerous command confirmation did not open",
    );
    await clickButton("取消");
    await waitFor(
      () => !document.body.textContent.includes("⚠ 高危命令确认"),
      "dangerous command confirmation did not close after cancel",
    );
    mainTerminal.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "c",
        code: "KeyC",
        ctrlKey: true,
        bubbles: true,
        cancelable: true,
      }),
    );
    await sleep(100);
    await sendCommand(
      mainTerminal,
      "printf RUSTERM_AFTER_CANCEL_OK",
      "RUSTERM_AFTER_CANCEL_OK",
    );

    stage = "comparison-broadcast";
    await clickTitle("Open a local shell (zsh/bash)");
    await sleep(500);
    await clickTitle(
      "Distribute — toggle split + fill panes with all sessions (off = tab tiling)",
    );
    await clickTitle(
      "Distribute — toggle split + fill panes with all sessions (off = tab tiling)",
    );
    const workspaceTerminals = await waitFor(() => {
      const terminals = [
        ...document.querySelectorAll(
          '#terminal-content [id^="terminal-input-"]',
        ),
      ];
      return terminals.length >= 2 ? terminals : null;
    }, "two workspace terminals did not render after distribute");
    await clickTitle("Toggle comparison mode (sync scroll + broadcast input)");
    await waitFor(
      () => document.querySelector(".compare-btn-on"),
      "comparison mode did not turn on",
    );
    await sendCommand(
      workspaceTerminals[0],
      "printf RUSTERM_COMPARE_E2E_OK",
      "RUSTERM_COMPARE_E2E_OK",
    );
    await waitFor(
      () =>
        workspaceTerminals.every((terminal) =>
          terminal.textContent.includes("RUSTERM_COMPARE_E2E_OK"),
        ),
      "comparison command was not broadcast to every workspace terminal",
    );

    stage = "toggle-floating-pane-move";
    const paneHandle = await waitFor(
      () => document.querySelector(".pane-drag-handle"),
      "floating pane move handle did not render",
    );
    const paneHandleRect = paneHandle.getBoundingClientRect();
    const startX = paneHandleRect.left + paneHandleRect.width / 2;
    const startY = paneHandleRect.top + paneHandleRect.height / 2;
    for (const type of ["mousedown", "mouseup", "click"]) {
      paneHandle.dispatchEvent(
        new MouseEvent(type, {
          button: 0,
          buttons: type === "mousedown" ? 1 : 0,
          clientX: startX,
          clientY: startY,
          bubbles: true,
          cancelable: true,
          composed: true,
        }),
      );
    }
    await waitFor(
      () =>
        typeof window._rusterm_pane_move_remove === "function" &&
        document.body.style.userSelect === "none",
      "first pane-handle click did not start move mode",
    );
    const firstPaneWindow = () =>
      document.querySelector(".pane-drag-handle")?.closest(".pane-title-bar")
        ?.parentElement;
    const beforeMoveRect = firstPaneWindow().getBoundingClientRect();
    const movedX = startX + 70;
    const movedY = startY + 45;
    document.dispatchEvent(
      new MouseEvent("mousemove", {
        button: 0,
        buttons: 0,
        clientX: movedX,
        clientY: movedY,
        bubbles: true,
        cancelable: true,
      }),
    );
    await waitFor(() => {
      const rect = firstPaneWindow()?.getBoundingClientRect();
      return (
        rect &&
        (Math.abs(rect.left - beforeMoveRect.left) > 20 ||
          Math.abs(rect.top - beforeMoveRect.top) > 20)
      );
    }, "pane did not follow a button-free mousemove after the first click");
    document.body.dispatchEvent(
      new MouseEvent("mousedown", {
        button: 0,
        buttons: 1,
        clientX: movedX,
        clientY: movedY,
        bubbles: true,
        cancelable: true,
        composed: true,
      }),
    );
    if (window.__rusterm_pane_move_done !== true) {
      throw new Error(
        "second primary press did not stop move mode immediately",
      );
    }
    for (const type of ["mouseup", "click"]) {
      document.body.dispatchEvent(
        new MouseEvent(type, {
          button: 0,
          buttons: 0,
          clientX: movedX,
          clientY: movedY,
          bubbles: true,
          cancelable: true,
          composed: true,
        }),
      );
    }
    await waitFor(
      () =>
        window._rusterm_pane_move_remove == null &&
        document.body.style.userSelect !== "none",
      "second primary press did not stop move mode or clean up listeners",
    );
    await sleep(80);
    const stoppedRect = firstPaneWindow().getBoundingClientRect();
    document.dispatchEvent(
      new MouseEvent("mousemove", {
        button: 0,
        buttons: 0,
        clientX: movedX + 90,
        clientY: movedY + 60,
        bubbles: true,
        cancelable: true,
      }),
    );
    await sleep(120);
    const afterStopRect = firstPaneWindow().getBoundingClientRect();
    if (
      Math.abs(afterStopRect.left - stoppedRect.left) > 1 ||
      Math.abs(afterStopRect.top - stoppedRect.top) > 1
    ) {
      throw new Error(
        "pane kept following the pointer after the second primary press",
      );
    }

    const currentMainTerminal = document.getElementById(mainTerminal.id);
    if (!currentMainTerminal)
      throw new Error("main terminal disappeared after distribute");

    stage = "open-bottom-shell";
    await ensureZone("bottom", "Show or hide Send, Shell, and Transfers");
    await activateDockTab("Shell");
    await clickButton("Start local shell");
    const bottomTerminal = await waitFor(
      () =>
        document.querySelector(
          '[data-rusterm-dock-zone="bottom"] [id^="terminal-input-"]',
        ),
      "embedded shell did not open",
    );
    const bottomTerminalId = bottomTerminal.id;
    await sleep(750);
    if (bottomTerminalId === mainTerminal.id)
      throw new Error("main and bottom shell reused a DOM id");
    await sendCommand(
      bottomTerminal,
      "printf RUSTERM_BOTTOM_E2E_OK",
      "RUSTERM_BOTTOM_E2E_OK",
    );
    if (currentMainTerminal.textContent.includes("RUSTERM_BOTTOM_E2E_OK")) {
      throw new Error("bottom shell input leaked into main terminal");
    }

    stage = "hide-show-bottom";
    await clickTitle("Hide bottom dock");
    await waitFor(
      () => !document.querySelector('[data-rusterm-dock-zone="bottom"]'),
      "bottom dock did not hide",
    );
    await clickTitle("Show or hide Send, Shell, and Transfers");
    const restoredBottomTerminal = await waitFor(
      () =>
        document.querySelector(
          '[data-rusterm-dock-zone="bottom"] [id^="terminal-input-"]',
        ),
      "bottom shell did not restore after dock reopen",
    );
    if (restoredBottomTerminal.id !== bottomTerminalId) {
      throw new Error("bottom shell session changed after hide/show");
    }
    if (!restoredBottomTerminal.textContent.includes("RUSTERM_BOTTOM_E2E_OK")) {
      throw new Error("bottom shell scrollback was lost after hide/show");
    }

    stage = "terminate-bottom";
    await clickTitle("Terminate embedded shell");
    await waitFor(
      () =>
        !document.querySelector(
          '[data-rusterm-dock-zone="bottom"] [id^="terminal-input-"]',
        ),
      "embedded shell remained mounted after terminate",
    );
    await waitFor(
      () => buttonByText("Start local shell"),
      "embedded shell placeholder did not return",
    );
    if (!document.getElementById(mainTerminal.id))
      throw new Error("terminating bottom shell closed main terminal");

    await sleep(1000);
    return "ok";
  } catch (error) {
    return `error:${stage}:${error && error.message ? error.message : String(error)}:${error && error.stack ? error.stack : "no-stack"}`;
  }
})();
