(() => {
  "use strict";

  document.querySelectorAll("[data-reload]").forEach((button) => {
    button.addEventListener("click", () => window.location.reload());
  });

  document.querySelectorAll("[data-dismiss-flash]").forEach((button) => {
    button.addEventListener("click", () => {
      const flash = button.closest("[data-flash]");
      if (flash) flash.remove();
    });
  });

  const busyOverlay = document.createElement("div");
  busyOverlay.className = "busy-overlay";
  busyOverlay.setAttribute("role", "status");
  busyOverlay.setAttribute("aria-live", "polite");
  busyOverlay.innerHTML = '<span class="busy-spinner" aria-hidden="true"></span><strong>正在执行操作…</strong><small>PolicyKit 或 libvirt 操作可能需要一些时间，请勿关闭页面。</small>';
  document.body.appendChild(busyOverlay);

  document.querySelectorAll("form[data-busy]").forEach((form) => {
    form.addEventListener("submit", (event) => {
      const prompt = form.dataset.confirm;
      if (prompt && !window.confirm(prompt)) {
        event.preventDefault();
        return;
      }
      form.setAttribute("aria-busy", "true");
      setTimeout(() => {
        busyOverlay.classList.add("is-visible");
        form.querySelectorAll("button").forEach((button) => {
          button.disabled = true;
        });
      }, 0);
    });
  });

  const networkSelect = document.querySelector("#network-interface");
  const networkRadios = document.querySelectorAll('input[name="network_mode"]');
  const updateNetworkOptions = () => {
    if (!networkSelect || networkRadios.length === 0) return;
    const selectedMode = document.querySelector('input[name="network_mode"]:checked');
    const bridge = selectedMode && selectedMode.value === "bridge";
    let selectedDisabled = false;
    Array.from(networkSelect.options).forEach((option) => {
      const wired = option.dataset.wired === "true";
      option.disabled = bridge && option.value !== "" && !wired;
      if (option.selected && option.disabled) selectedDisabled = true;
    });
    if (selectedDisabled) {
      const firstWired = Array.from(networkSelect.options).find((option) => option.value !== "" && option.dataset.wired === "true");
      networkSelect.value = firstWired ? firstWired.value : "";
    }
  };
  networkRadios.forEach((radio) => radio.addEventListener("change", updateNetworkOptions));
  updateNetworkOptions();

  const live = (name) => document.querySelector(`[data-live="${name}"]`);
  const updateText = (name, value) => {
    const node = live(name);
    if (node && value !== undefined && value !== null) node.textContent = value;
  };
  const setStateDot = (stateClass) => {
    const dot = document.querySelector("[data-live-state-dot]");
    if (!dot) return;
    dot.classList.remove("ok", "warn", "off", "danger", "error");
    dot.classList.add(stateClass || "off");
  };

  const refreshStatus = async () => {
    if (document.hidden) return;
    try {
      const response = await fetch("/api/status", {
        headers: { Accept: "application/json" },
        credentials: "same-origin",
        cache: "no-store",
      });
      if (response.status === 401) {
        window.location.assign("/login");
        return;
      }
      const data = await response.json().catch(() => null);
      if (!data) return;
      if (!data.ok) {
        updateText("vm-label", "无法读取");
        updateText("vm-pill", "libvirt 异常");
        setStateDot("danger");
        return;
      }
      updateText("vm-label", data.vm.label);
      updateText("vm-pill", data.vm.active ? "运行中" : (data.vm.defined ? "已就绪" : "未配置"));
      updateText("cpu-percent", data.vm.cpuPercent);
      updateText("vm-memory", data.vm.memoryCurrent);
      updateText("vm-memory-max", data.vm.memoryMax);
      setStateDot(data.vm.stateClass);
    } catch (_error) {
      // A transient failure should not disrupt the page; the next poll retries.
    }
  };

  if (document.querySelector("[data-live-state-dot]")) {
    window.setInterval(refreshStatus, 5000);
    document.addEventListener("visibilitychange", () => {
      if (!document.hidden) refreshStatus();
    });
  }
})();
