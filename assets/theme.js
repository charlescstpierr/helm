// Applies the saved theme before first paint. Loaded synchronously from <head>; kept in its
// own file because the Content-Security-Policy forbids inline scripts.
(() => {
  try {
    const theme = localStorage.getItem('helm-theme');
    if (theme === 'light' || theme === 'dark') {
      document.documentElement.dataset.theme = theme;
    }
  } catch {
    // Storage unavailable: follow the system preference.
  }
})();
