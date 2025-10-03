function waitForElement(id, callback) {
  const el = document.getElementById(id);
  if (el) {
    callback(el);
    return;
  }

  const observer = new MutationObserver(() => {
    const el = document.getElementById(id);
    if (el) {
      observer.disconnect();
      callback(el);
    }
  });

  observer.observe(document.body, { childList: true, subtree: true });
}

/**
 * Watch an element's scrollHeight and call onChange(height) only when it changes.
 * Returns a cleanup function.
 */
function watchScrollHeight(el, onChange) {
  let last = -1;
  let scheduled = false;

  const measure = () => {
    const h = el.scrollHeight;
    if (h !== last) {
      last = h;
      onChange(h);
    }
  };

  const schedule = () => {
    if (scheduled) return;
    scheduled = true;
    requestAnimationFrame(() => {
      scheduled = false;
      measure();
    });
  };

  // 1) DOM mutations inside el (added/removed nodes, text changes, attribute/style flips)
  const mo = new MutationObserver(schedule);
  mo.observe(el, {
    childList: true,
    subtree: true,
    attributes: true,
    characterData: true,
  });

  // 2) Resizes that can influence layout (viewport/body/el itself)
  const ro = new ResizeObserver(schedule);
  ro.observe(document.documentElement);
  ro.observe(document.body);
  ro.observe(el);

  // 3) Resource loads inside el (images/iframes/videos). Use capture because 'load' doesn't bubble.
  el.addEventListener("load", schedule, true);

  // 4) Font loads can reflow text
  if (document.fonts) {
    // schedule once fonts currently loading are ready
    document.fonts.ready.then(schedule).catch(() => {});
    // and also react to future font loading cycles (if supported)
    document.fonts.addEventListener?.("loadingdone", schedule);
  }

  // 5) Window resizes
  window.addEventListener("resize", schedule);

  // Initial measure
  schedule();

  // Cleanup
  return () => {
    mo.disconnect();
    ro.disconnect();
    el.removeEventListener("load", schedule, true);
    window.removeEventListener("resize", schedule);
  };
}

// Usage
window.addEventListener("load", () => {
  waitForElement("App", (el) => {
    watchScrollHeight(el, (height) => {
      window.parent.postMessage(
        { type: "AppScrollHeight", scrollHeight: height },
        "*",
      );
    });
  });
});
