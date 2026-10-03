// The "Copy the prompt" button. Without JavaScript or the Clipboard API the button stays hidden
// and the prompt can still be selected and copied by hand.
"use strict";

for (const button of document.querySelectorAll("button[data-copy]")) {
  const source = document.getElementById(button.dataset.copy);
  const status = button.closest("figure")?.querySelector(".copy-status");
  if (!source || !navigator.clipboard) continue;
  button.hidden = false;
  button.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(source.textContent);
      button.textContent = "Copied";
      if (status) status.textContent = "The prompt is on your clipboard. Paste it into your AI assistant.";
    } catch {
      if (status) status.textContent = "Couldn't copy. Select the prompt and copy it by hand.";
    }
  });
}
