(() => {
  const form = document.getElementById("gog-signin");
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const input = document.getElementById("gog-return-url");
    const submit = document.getElementById("gog-continue");
    const error = document.getElementById("gog-error");
    const returnUrl = input.value.trim();
    if (!returnUrl || submit.disabled) return;
    submit.disabled = true;
    submit.textContent = "Checking…";
    error.hidden = true;
    input.value = "";
    try {
      await window.__TAURI__.core.invoke("complete_gog_sign_in", {
        attemptId: form.dataset.attemptId,
        returnUrl,
      });
    } catch (message) {
      if (!form.isConnected) return;
      submit.disabled = false;
      submit.textContent = "Continue";
      error.textContent = String(message);
      error.hidden = false;
      input.focus();
    }
  });
})();
