// Helm board behaviour: progressive enhancement over server-rendered HTML.
// Without this script every form still works through plain posts and redirects.
(() => {
  'use strict';

  const FETCH_HEADERS = { 'X-Helm-Request': 'fetch' };

  // --- Theme toggle (every page) -------------------------------------------------------

  const THEMES = ['auto', 'light', 'dark'];
  const THEME_NAMES = { auto: 'Thème : système', light: 'Thème : clair', dark: 'Thème : sombre' };
  const themeToggle = document.getElementById('theme-toggle');

  function currentTheme() {
    const theme = document.documentElement.dataset.theme;
    return theme === 'light' || theme === 'dark' ? theme : 'auto';
  }

  function applyTheme(theme) {
    if (theme === 'auto') {
      delete document.documentElement.dataset.theme;
    } else {
      document.documentElement.dataset.theme = theme;
    }
    try {
      localStorage.setItem('helm-theme', theme);
    } catch {
      // Storage unavailable: the choice lasts for this page only.
    }
    themeToggle.textContent = THEME_NAMES[theme];
  }

  if (themeToggle) {
    themeToggle.textContent = THEME_NAMES[currentTheme()];
    themeToggle.hidden = false;
    themeToggle.addEventListener('click', () => {
      applyTheme(THEMES[(THEMES.indexOf(currentTheme()) + 1) % THEMES.length]);
    });
  }

  // --- Board ---------------------------------------------------------------------------

  const board = document.getElementById('board');
  if (!board) return;

  const dialog = document.getElementById('card-dialog');
  const dialogBody = document.getElementById('card-dialog-body');
  const liveRegion = document.getElementById('live-region');
  const statusBox = document.getElementById('connection-status');
  const statusText = document.getElementById('connection-text');
  document.getElementById('shortcuts').hidden = false;

  let dragged = null;
  let dropped = false;
  let refreshing = false;
  let refreshPending = false;
  let lastOpenedCardId = null;

  const announce = (message) => {
    liveRegion.textContent = message;
  };

  async function post(url, params) {
    const response = await fetch(url, {
      method: 'POST',
      headers: { ...FETCH_HEADERS, 'Content-Type': 'application/x-www-form-urlencoded' },
      body: params,
    });
    if (!response.ok) {
      throw new Error((await response.text()) || `Erreur ${response.status}`);
    }
  }

  const columns = () => [...board.querySelectorAll('.column')];
  const cardsOf = (column) => [...column.querySelectorAll('.card')];
  const columnName = (column) => column.querySelector('.column__title').textContent;
  const cardTitle = (card) => card.querySelector('.card__title').textContent;

  function updateCounts() {
    for (const column of columns()) {
      column.querySelector('[data-count]').textContent = cardsOf(column).length;
    }
  }

  function focusCard(cardId) {
    board.querySelector(`.card[data-card-id="${CSS.escape(cardId)}"] .card__link`)?.focus();
  }

  // Re-renders card lists from the server. Quick-add inputs are left untouched so a title
  // being typed survives a refresh triggered by another tab.
  async function refresh() {
    if (refreshing || dragged) {
      refreshPending = true;
      return;
    }
    refreshing = true;
    try {
      const response = await fetch('/board', { headers: FETCH_HEADERS });
      if (!response.ok) return;
      const template = document.createElement('template');
      template.innerHTML = await response.text();
      const next = template.content.getElementById('board');
      if (!next || dragged) {
        refreshPending = Boolean(dragged);
        return;
      }
      const focusedCardId = document.activeElement?.closest?.('.card')?.dataset.cardId;
      const nextColumns = [...next.querySelectorAll('.column')];
      const current = columns();
      const sameColumns =
        current.length === nextColumns.length &&
        current.every((column, i) => column.dataset.columnId === nextColumns[i].dataset.columnId);
      if (sameColumns) {
        current.forEach((column, i) => {
          column.querySelector('.column__cards').replaceWith(nextColumns[i].querySelector('.column__cards'));
          column.querySelector('.column__header').replaceWith(nextColumns[i].querySelector('.column__header'));
        });
      } else {
        board.replaceChildren(...next.children);
      }
      if (focusedCardId && !dialog.open) focusCard(focusedCardId);
    } catch {
      // Offline: the SSE reconnection triggers another refresh.
    } finally {
      refreshing = false;
      if (refreshPending) {
        refreshPending = false;
        refresh();
      }
    }
  }

  // Persists where `card` currently sits in the DOM.
  async function commitMove(card) {
    const column = card.closest('.column');
    const position = cardsOf(column).indexOf(card);
    updateCounts();
    try {
      await post(
        `/cards/${card.dataset.cardId}/move`,
        new URLSearchParams({ column_id: column.dataset.columnId, position }),
      );
      announce(`« ${cardTitle(card)} » déplacée dans ${columnName(column)}, position ${position + 1}.`);
    } catch (error) {
      announce(`Déplacement impossible : ${error.message}`);
    }
    refresh();
  }

  // --- Drag and drop -------------------------------------------------------------------

  board.addEventListener('dragstart', (event) => {
    const card = event.target.closest?.('.card');
    if (!card) return;
    dragged = card;
    dropped = false;
    event.dataTransfer.effectAllowed = 'move';
    event.dataTransfer.setData('text/plain', cardTitle(card));
    // Defer the style so the drag image is taken from the unfaded card.
    requestAnimationFrame(() => dragged?.classList.add('is-dragging'));
  });

  board.addEventListener('dragover', (event) => {
    if (!dragged) return;
    const list = event.target.closest?.('.column')?.querySelector('.column__cards');
    if (!list) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = 'move';
    const before = [...list.querySelectorAll('.card')].find((card) => {
      if (card === dragged) return false;
      const box = card.getBoundingClientRect();
      return event.clientY < box.top + box.height / 2;
    });
    if (before) {
      if (before.previousElementSibling !== dragged) list.insertBefore(dragged, before);
    } else if (list.lastElementChild !== dragged) {
      list.append(dragged);
    }
  });

  board.addEventListener('drop', (event) => {
    if (!dragged) return;
    event.preventDefault();
    dropped = true;
  });

  board.addEventListener('dragend', () => {
    if (!dragged) return;
    const card = dragged;
    dragged = null;
    card.classList.remove('is-dragging');
    if (dropped) {
      commitMove(card);
    } else {
      // Cancelled (Escape or dropped outside): restore the server's order.
      refresh();
    }
  });

  // --- Keyboard ------------------------------------------------------------------------

  function quickAddInput(column) {
    return column.querySelector('.quick-add input[name="title"]');
  }

  function focusInColumn(column, index) {
    const cards = cardsOf(column);
    if (cards.length === 0) {
      quickAddInput(column).focus();
    } else {
      cards[Math.min(index, cards.length - 1)].querySelector('.card__link').focus();
    }
  }

  function navigate(card, key) {
    const column = card.closest('.column');
    const cards = cardsOf(column);
    const index = cards.indexOf(card);
    const all = columns();
    const columnIndex = all.indexOf(column);
    if (key === 'ArrowUp' && index > 0) {
      cards[index - 1].querySelector('.card__link').focus();
    } else if (key === 'ArrowDown') {
      if (index < cards.length - 1) cards[index + 1].querySelector('.card__link').focus();
      else quickAddInput(column).focus();
    } else if (key === 'ArrowLeft' && columnIndex > 0) {
      focusInColumn(all[columnIndex - 1], index);
    } else if (key === 'ArrowRight' && columnIndex < all.length - 1) {
      focusInColumn(all[columnIndex + 1], index);
    }
  }

  function moveWithKeyboard(card, key) {
    const column = card.closest('.column');
    const list = column.querySelector('.column__cards');
    const all = columns();
    const columnIndex = all.indexOf(column);
    if (key === 'ArrowUp' && card.previousElementSibling) {
      list.insertBefore(card, card.previousElementSibling);
    } else if (key === 'ArrowDown' && card.nextElementSibling) {
      list.insertBefore(card.nextElementSibling, card);
    } else if (key === 'ArrowLeft' && columnIndex > 0) {
      all[columnIndex - 1].querySelector('.column__cards').append(card);
    } else if (key === 'ArrowRight' && columnIndex < all.length - 1) {
      all[columnIndex + 1].querySelector('.column__cards').append(card);
    } else {
      return;
    }
    // Moving a node drops its focus.
    card.querySelector('.card__link').focus();
    commitMove(card);
  }

  board.addEventListener('keydown', (event) => {
    if (!event.key.startsWith('Arrow') || event.ctrlKey || event.metaKey || event.shiftKey) return;
    const card = event.target.closest?.('.card');
    if (!card || !event.target.matches('.card__link')) return;
    event.preventDefault();
    if (event.altKey) moveWithKeyboard(card, event.key);
    else navigate(card, event.key);
  });

  document.addEventListener('keydown', (event) => {
    if (event.key.toLowerCase() !== 'n' || event.ctrlKey || event.metaKey || event.altKey) return;
    if (dialog.open || event.target.closest?.('input, textarea, select, [contenteditable]')) return;
    const column = document.activeElement?.closest?.('.column') ?? columns()[0];
    if (!column) return;
    event.preventDefault();
    quickAddInput(column).focus();
  });

  // --- Card dialog ---------------------------------------------------------------------

  board.addEventListener('click', async (event) => {
    const link = event.target.closest?.('.card__link');
    if (!link || event.button !== 0 || event.ctrlKey || event.metaKey || event.shiftKey || event.altKey) return;
    event.preventDefault();
    try {
      const response = await fetch(link.href, { headers: FETCH_HEADERS });
      if (!response.ok) throw new Error((await response.text()) || `Erreur ${response.status}`);
      dialogBody.innerHTML = await response.text();
    } catch (error) {
      announce(`Ouverture impossible : ${error.message}`);
      refresh();
      return;
    }
    lastOpenedCardId = link.closest('.card').dataset.cardId;
    dialog.showModal();
    dialogBody.querySelector('input[name="title"]')?.focus();
  });

  let pressedOnBackdrop = false;
  dialog.addEventListener('mousedown', (event) => {
    pressedOnBackdrop = event.target === dialog;
  });

  dialog.addEventListener('click', async (event) => {
    const deleteLink = event.target.closest('[data-delete-card]');
    if (deleteLink) {
      // Without this script the link leads to a server-rendered confirmation page.
      event.preventDefault();
      if (deleteLink.dataset.submitting || !confirm(deleteLink.dataset.confirm)) return;
      const errorBox = dialogBody.querySelector('.form__error');
      errorBox.hidden = true;
      deleteLink.dataset.submitting = 'true';
      try {
        await post(deleteLink.href, '');
      } catch (error) {
        errorBox.textContent = `Suppression impossible : ${error.message}`;
        errorBox.hidden = false;
        return;
      } finally {
        delete deleteLink.dataset.submitting;
      }
      dialog.close();
      refresh();
    } else if (event.target.closest('[data-close-dialog]')) {
      event.preventDefault();
      dialog.close();
    } else if (event.target === dialog && pressedOnBackdrop) {
      // A click on the backdrop lands on the <dialog> element itself.
      dialog.close();
    }
  });

  dialog.addEventListener('close', () => {
    dialogBody.replaceChildren();
    if (lastOpenedCardId) focusCard(lastOpenedCardId);
  });

  // --- Forms ---------------------------------------------------------------------------

  document.addEventListener('submit', async (event) => {
    const form = event.target;
    if (!form.matches('form[data-async]')) return;
    event.preventDefault();
    // A second submit while the first is in flight would create a duplicate.
    if (form.dataset.submitting) return;

    const inDialog = dialog.contains(form);
    const errorBox = (inDialog ? dialogBody : form).querySelector('.form__error');
    if (errorBox) errorBox.hidden = true;
    const title = form.querySelector('input[name="title"]');
    const sentTitle = title.value;
    form.dataset.submitting = 'true';
    try {
      await post(form.action, new URLSearchParams(new FormData(form)));
    } catch (error) {
      if (errorBox) {
        errorBox.textContent = error.message;
        errorBox.hidden = false;
      } else {
        announce(error.message);
      }
      return;
    } finally {
      delete form.dataset.submitting;
    }
    if (inDialog) {
      dialog.close();
    } else {
      announce(`Carte « ${sentTitle.trim()} » ajoutée.`);
      // Keep whatever was typed while the request was in flight.
      if (title.value === sentTitle) form.reset();
      title.focus();
    }
    refresh();
  });

  // --- Live updates --------------------------------------------------------------------

  function setConnection(state, text) {
    statusBox.dataset.state = state;
    statusBox.hidden = false;
    statusText.textContent = text;
  }

  const events = new EventSource('/events');
  events.addEventListener('open', () => {
    setConnection('live', 'En direct');
    // Nothing is replayed: catch up on writes made before this connection was established.
    refresh();
  });
  events.addEventListener('error', () => setConnection('offline', 'Hors ligne'));
  events.addEventListener('board', refresh);
})();
