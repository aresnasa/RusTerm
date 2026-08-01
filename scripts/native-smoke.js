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

    stage = "dangerous-command-cancel";
    await typeCommand(mainTerminal, "mkfs.ext4 /dev/rusterm-e2e-nonexistent");
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
    if (bottomTerminalId === mainTerminal.id)
      throw new Error("main and bottom shell reused a DOM id");
    await sendCommand(
      bottomTerminal,
      "printf RUSTERM_BOTTOM_E2E_OK",
      "RUSTERM_BOTTOM_E2E_OK",
    );
    if (mainTerminal.textContent.includes("RUSTERM_BOTTOM_E2E_OK")) {
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
