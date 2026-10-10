// Theme: the saved choice wins, otherwise follow the system. Basecoat styles the `dark` class.
(function () {
  // Purge snapshots saved by older versions, which may contain one-time API tokens.
  try { sessionStorage.removeItem('htmx-history-cache'); } catch (e) { /* storage blocked */ }
  var saved;
  try { saved = localStorage.getItem('themeMode'); } catch (e) { /* storage blocked */ }
  var dark = saved ? saved === 'dark' : window.matchMedia('(prefers-color-scheme: dark)').matches;
  document.documentElement.classList.toggle('dark', dark);
})();

// The clipboard API only exists on HTTPS and localhost; over plain HTTP fall back to selecting the text.
function copyFrom(source) {
  var text = source.value || source.textContent.trim();
  var fallback = function () {
    if (source.select) source.select();
    document.execCommand('copy');
  };
  if (!navigator.clipboard) return Promise.resolve(fallback());
  return navigator.clipboard.writeText(text).catch(fallback);
}

// Buttons that call into Basecoat (inline onclick handlers would be blocked by the CSP).
document.addEventListener('click', function (event) {
  if (event.target.closest('[data-theme-toggle]')) window.basecoat.theme.toggle();
  var sidebarToggle = event.target.closest('[data-sidebar-toggle]');
  if (sidebarToggle) document.getElementById('sidebar').toggle();
  var copy = event.target.closest('[data-copy]');
  if (copy) {
    copyFrom(document.getElementById(copy.dataset.copy)).then(function () {
      copy.dataset.copied = '';
      setTimeout(function () { delete copy.dataset.copied; }, 1500);
    });
  }
});

// The server emits this event only after an alias was created and its row was swapped in.
document.addEventListener('aliasCreated', function (event) {
  if (event.target.matches('form[data-reset]')) event.target.reset();
});

// hx-confirm opens the Basecoat dialog instead of the browser's confirm(). The element that asks
// can set data-confirm-title, data-confirm-ok and data-confirm-variant ("primary" or "destructive").
document.addEventListener('htmx:confirm', function (event) {
  if (!event.detail.question) return;
  event.preventDefault();
  var asker = event.detail.elt.dataset;
  var dialog = document.getElementById('confirm-dialog');
  var ok = dialog.querySelector('[data-dialog-ok]');
  dialog.querySelector('#confirm-title').textContent = asker.confirmTitle || 'Are you sure?';
  dialog.querySelector('#confirm-text').textContent = event.detail.question;
  ok.textContent = asker.confirmOk || 'Continue';
  if (asker.confirmVariant === 'primary') ok.removeAttribute('data-variant');
  else ok.setAttribute('data-variant', 'destructive');
  ok.onclick = function () {
    dialog.close();
    event.detail.issueRequest(true);
  };
  dialog.querySelector('[data-dialog-cancel]').onclick = function () { dialog.close(); };
  dialog.onclick = function (click) { if (click.target === dialog) dialog.close(); };
  dialog.showModal();
});
