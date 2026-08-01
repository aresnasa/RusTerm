return (async () => {
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
    const setter = Object.getOwnPropertyDescriptor(
      HTMLInputElement.prototype,
      "value",
    ).set;
    const input = await waitFor(
      () => document.querySelector('input[placeholder="Enter password"]'),
      "unlock input did not render on existing-config startup",
    );
    setter.call(input, "rusterm-e2e");
    input.dispatchEvent(
      new InputEvent("input", {
        bubbles: true,
        cancelable: true,
        data: "rusterm-e2e",
        inputType: "insertText",
      }),
    );
    const unlock = await waitFor(
      () =>
        [...document.querySelectorAll("button")].find(
          (button) =>
            button.textContent.trim() === "Unlock" && !button.disabled,
        ),
      "Unlock button did not enable",
    );
    unlock.click();
    await waitFor(
      () => document.querySelector("#main"),
      "workspace did not render after unlock",
    );

    const remoteFiles = await waitFor(
      () =>
        document.querySelector(
          '[data-rusterm-dock-zone="right"] [title="Drag to reorder or move Remote files"]',
        ),
      "cross-zone dock layout did not persist across restart",
    );
    if (!remoteFiles) throw new Error("Remote files tab missing after restart");
    const leftWidth = document
      .querySelector('[data-rusterm-dock-zone="left"]')
      .getBoundingClientRect().width;
    if (leftWidth < 260)
      throw new Error(`resized left dock width was not restored: ${leftWidth}`);
    if (!document.querySelector('[data-rusterm-dock-zone="bottom"]')) {
      throw new Error("bottom dock visibility was not restored");
    }
    if (!document.querySelector("#main"))
      throw new Error("center workspace missing after restart");
    return "ok";
  } catch (error) {
    return `error:${error && error.stack ? error.stack : String(error)}`;
  }
})();
