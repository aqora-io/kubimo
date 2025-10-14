function waitForElement(id, callback) {
  const check = () => {
    const el = document.getElementById(id);
    if (el) {
      try {
        callback(el);
      } catch (e) {
        console.error(e);
      }
      return true;
    }
    return false;
  };
  if (check()) return;

  const mo = new MutationObserver(() => {
    if (check()) {
      mo.disconnect();
    }
  });
  mo.observe(document.body, { childList: true, subtree: true });
}

function watchChildrenHeights(container, onChange) {
  let lastHeight = -1;

  const getChildrenHeight = () =>
    Array.from(container.children).reduce(
      (sum, child) => sum + child.offsetHeight,
      0,
    );

  const measure = () => {
    const total = getChildrenHeight();
    if (total !== lastHeight) {
      lastHeight = total;
      onChange(total);
    }
  };

  const ro = new ResizeObserver(measure);
  const observeChildren = () => {
    Array.from(container.children).forEach((child) => ro.observe(child));
    measure();
  };
  observeChildren();

  const mo = new MutationObserver(() => {
    ro.disconnect();
    observeChildren();
  });
  mo.observe(container, {
    childList: true,
    subtree: false,
  });

  return () => {
    mo.disconnect();
    ro.disconnect();
  };
}

window.addEventListener("load", () => {
  waitForElement("App", (appEl) => {
    appEl.setAttribute("data-command", "run");
    watchChildrenHeights(appEl, (height) => {
      window.parent.postMessage({ type: "AppHeight", height }, "*");
    });
  });
});
