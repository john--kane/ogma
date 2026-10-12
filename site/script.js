const status = document.getElementById("copy-status");
let statusTimer;

for (const button of document.querySelectorAll("[data-copy]")) {
	button.hidden = false;
	button.addEventListener("click", async () => {
		const code = document.getElementById(button.dataset.copy);
		button.disabled = true;
		try {
			if (!navigator.clipboard?.writeText)
				throw new Error("Clipboard unavailable");
			await navigator.clipboard.writeText(code.textContent);
			status.textContent = "Copied. Ready for your terminal.";
		} catch {
			const range = document.createRange();
			range.selectNodeContents(code);
			const selection = window.getSelection();
			selection.removeAllRanges();
			selection.addRange(range);
			status.textContent =
				"Clipboard unavailable. The command is selected; copy it manually.";
		} finally {
			button.disabled = false;
			clearTimeout(statusTimer);
			statusTimer = setTimeout(() => {
				status.textContent = "";
			}, 6000);
		}
	});
}
