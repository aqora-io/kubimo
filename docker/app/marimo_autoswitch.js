(function () {
  var skipParam = __SKIP_PARAM__;
  var readyPath = __READY_PATH__;
  var includeCode = __INCLUDE_CODE__;
  var iframe = document.createElement("iframe");
  var storage = null;
  try {
    storage = window.sessionStorage;
  } catch (e) {
    storage = null;
  }
  var hideStyle = document.createElement("style");
  hideStyle.textContent =
    '[data-testid="static-notebook-banner"], [data-testid="watermark"] {' +
    "display: none !important;" +
    "}" +
    ".marimo-cached-pending {" +
    "opacity: 0.45;" +
    "filter: grayscale(0.25);" +
    "transition: opacity 200ms ease, filter 200ms ease;" +
    "}";
  (document.head || document.documentElement).appendChild(hideStyle);

  var showCodeParam = "show-code";
  var showCodePreference = false;

  function updateSearchParam(url, key, value) {
    var current = url.searchParams.get(key);
    if (current === value) {
      return false;
    }
    url.searchParams.set(key, value);
    return true;
  }

  function updatePageShowCode(showCode) {
    if (!includeCode) {
      return;
    }
    var url = new URL(window.location.href);
    if (!updateSearchParam(url, showCodeParam, showCode ? "true" : "false")) {
      return;
    }
    if (window.history && window.history.replaceState) {
      window.history.replaceState(null, "", url.toString());
    }
  }

  function updateIframeShowCode(showCode) {
    if (!includeCode || !iframe.src) {
      return;
    }
    var iframeUrl = new URL(iframe.src);
    if (
      !updateSearchParam(iframeUrl, showCodeParam, showCode ? "true" : "false")
    ) {
      return;
    }
    iframe.src = iframeUrl.toString();
  }

  function setShowCodePreference(showCode) {
    showCodePreference = showCode;
    updatePageShowCode(showCode);
    updateIframeShowCode(showCode);
  }

  function initShowCodePreference() {
    var url = new URL(window.location.href);
    var param = url.searchParams.get(showCodeParam);
    if (param === "true") {
      showCodePreference = true;
      return;
    }
    if (param === "false") {
      showCodePreference = false;
      return;
    }
    showCodePreference = false;
    if (includeCode) {
      url.searchParams.set(showCodeParam, "false");
      if (window.history && window.history.replaceState) {
        window.history.replaceState(null, "", url.toString());
      }
    }
  }

  initShowCodePreference();

  function isLoadedStatus(status) {
    return status === "idle" || status === "disabled-transitively";
  }

  var lastStatuses = null;
  var pendingReadyListener = false;

  function applyDimming(statuses) {
    if (!document.body) {
      return false;
    }
    var cells = document.querySelectorAll(".marimo-cell[data-cell-id]");
    var hasStatuses = statuses && typeof statuses === "object";
    cells.forEach(function (cell) {
      var cellId = cell.getAttribute("data-cell-id");
      var status = hasStatuses ? statuses[cellId] : null;
      if (!isLoadedStatus(status)) {
        cell.classList.add("marimo-cached-pending");
        return;
      }
      cell.classList.remove("marimo-cached-pending");
    });
    return true;
  }

  function updateDimming(statuses) {
    lastStatuses = statuses;
    if (document.readyState !== "loading") {
      applyDimming(statuses);
      return;
    }
    if (pendingReadyListener) {
      return;
    }
    pendingReadyListener = true;
    document.addEventListener(
      "DOMContentLoaded",
      function () {
        pendingReadyListener = false;
        applyDimming(lastStatuses);
      },
      { once: true },
    );
  }

  updateDimming(null);

  function generateSessionId() {
    if (window.crypto && typeof window.crypto.randomUUID === "function") {
      return window.crypto.randomUUID();
    }
    return (
      "session-" +
      Math.random().toString(36).slice(2) +
      "-" +
      Date.now().toString(36)
    );
  }
  iframe.setAttribute("title", "marimo");
  iframe.style.position = "fixed";
  iframe.style.top = "0";
  iframe.style.left = "0";
  iframe.style.width = "100%";
  iframe.style.height = "100%";
  iframe.style.border = "0";
  iframe.style.zIndex = "9999";
  iframe.style.opacity = "0";
  iframe.style.pointerEvents = "none";
  iframe.style.background = "white";
  iframe.style.transition = "opacity 200ms ease";

  var url = new URL(window.location.href);
  var sessionKey = "marimo_session_id:" + window.location.pathname;
  var sessionId = url.searchParams.get("session_id");
  if (!sessionId && storage) {
    sessionId = storage.getItem(sessionKey);
  }
  if (!sessionId) {
    sessionId = generateSessionId();
  }
  if (storage) {
    storage.setItem(sessionKey, sessionId);
  }
  url.searchParams.set("session_id", sessionId);
  url.searchParams.set("force_replay", "1");
  url.searchParams.set(skipParam, "1");
  if (includeCode) {
    url.searchParams.set(showCodeParam, showCodePreference ? "true" : "false");
  }
  iframe.src = url.toString();
  document.body.appendChild(iframe);

  var readyUrl = new URL(readyPath, window.location.origin);
  readyUrl.searchParams.set("path", window.location.pathname);
  readyUrl.searchParams.set("session_id", sessionId);

  var start = Date.now();
  var maxWaitMs = 120000;
  if (includeCode) {
    document.addEventListener(
      "click",
      function (event) {
        var target = event.target;
        if (!target || !target.closest) {
          return;
        }
        var action = target.closest(
          '[data-testid="notebook-action-show-code"]',
        );
        if (!action) {
          return;
        }
        setShowCodePreference(!showCodePreference);
      },
      true,
    );
  }
  function poll() {
    fetch(readyUrl.toString(), { cache: "no-store" })
      .then(function (resp) {
        return resp.json();
      })
      .then(function (data) {
        updateDimming(data ? data.cell_statuses : null);
        if (data && data.ready) {
          iframe.style.opacity = "1";
          iframe.style.pointerEvents = "auto";
          return;
        }
        if (Date.now() - start < maxWaitMs) {
          setTimeout(poll, 500);
        }
      })
      .catch(function () {
        if (Date.now() - start < maxWaitMs) {
          setTimeout(poll, 1000);
        }
      });
  }
  poll();
})();
