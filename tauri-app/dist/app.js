const { invoke } = window.__TAURI__.core;

/** Sends a `Command` — `data` omitted entirely for unit variants (no
 * fields), matching serde's adjacently-tagged representation. */
function send(type, data) {
  const cmd = data === undefined ? { type } : { type, data };
  return invoke("send_command", { cmd }).catch((err) => {
    console.error("send_command failed:", type, err);
  });
}

const state = {
  screen: "login",
  loggingIn: false,
  rooms: [],
  selectedRoom: null,
  selectedSpace: null,
  spaceChildren: {}, // space_room_id -> [room_id]
  roomFilter: "",
  unreadOnly: false,
  // Index into the currently-visible (filtered) room list, i.e. the row
  // arrow-key navigation currently has "selected" — separate from
  // `selectedRoom` since a keyboard highlight moving through search
  // results shouldn't open each room it passes over. -1 = nothing
  // highlighted (mouse-only use never touches this).
  roomListActiveIndex: -1,
  // Room IDs in the order they're actually rendered by the last
  // `renderRooms()` call (post filter/space/unread-only), kept in sync so
  // arrow-key navigation and Enter-to-open agree with what's on screen.
  visibleRoomIds: [],
  timelines: {}, // room_id -> [TimelineEvent]
  // Rooms whose full timeline has actually been requested via
  // `LoadTimeline`/gotten an `Event::Timeline` reply — *not* the same as
  // `timelines[roomId]` existing, since a live `NewMessage` for a room
  // that's never been opened yet also creates that key (pushing just that
  // one message into a fresh `[]`). Without this, opening a room for the
  // first time after a live message had already arrived for it skipped
  // `LoadTimeline` entirely (`timelines[roomId]` looked "already loaded")
  // and showed only that one message instead of the room's history.
  timelineLoaded: new Set(), // room_id
  reachedStart: new Set(), // room_id
  paginationInFlight: new Set(),
  imageCache: {}, // mxc_uri -> data: URL
  imageRequested: new Set(),
  imageMime: {}, // mxc_uri -> sender-declared MIME type (content.info.mimetype), if any
  imageEncryption: {}, // mxc_uri -> JSON `EncryptedFile` (media_encryption), if the room is encrypted
  // mxc_uri -> `true`, or the sniffed MIME string once known — set once
  // `ImageBytes` comes back for an mxc with no declared mime and the
  // bytes turn out to be a format WebKitGTK can't decode either way.
  imageUnviewable: {},
  imagePacks: {}, // room_id -> [{shortcode, url, pack_name}] — custom emoji/meme, see Command::ListImagePacks
  roomMembers: {}, // room_id -> [[user_id, display_name]]
  notificationModes: {}, // room_id -> mode
  summaries: {}, // room_id -> text
  summarizing: false,

  pendingReply: null, // { roomId, threadId, eventId, preview }
  pendingEdit: null, // { roomId, threadId, eventId }
  pendingImage: null, // { roomId, threadId, dataUrl, bytes, filename, mime }
  composeMentions: [], // [{userId, displayName}] selected via autocomplete
  threadComposeMentions: [], // same, for the thread panel's compose box
  sending: false,

  rightPanel: null, // {kind:'threads-list', scope: roomId|null} | {kind:'thread', roomId, root, events} | {kind:'security'}
  threadsByRoom: {}, // room_id -> [TimelineEvent] (thread roots)
  // room_id -> true once `Command::ListThreads`/`LoadMoreThreads` has
  // reached that room's oldest thread — stops issuing further
  // `LoadMoreThreads` requests for it, and hides its "load more" row.
  threadsListReachedEnd: new Set(),
  threadsListPaginationInFlight: new Set(),
  unreadThreads: new Set(), // "room_id|thread_root_id"
  threadCompose: { sending: false },
  threadPaginationReachedStart: new Set(),

  verificationEmojis: null,
  recoveryStatus: null,
  recoveryKeyInput: "",
  // Set right before switching rooms to follow a matrix.to link — once
  // that room's `Timeline` event lands, scroll to this event and clear it.
  pendingScrollTarget: null, // { roomId, eventId }
};

// ---- DOM refs ----
const el = {
  loginScreen: document.getElementById("login-screen"),
  chatScreen: document.getElementById("chat-screen"),
  loginHomeserver: document.getElementById("login-homeserver"),
  loginUsername: document.getElementById("login-username"),
  loginPassword: document.getElementById("login-password"),
  loginError: document.getElementById("login-error"),
  loginSubmit: document.getElementById("login-submit"),
  loginOauth: document.getElementById("login-oauth"),
  spacePicker: document.getElementById("space-picker"),
  roomFilter: document.getElementById("room-filter"),
  btnUnreadOnly: document.getElementById("btn-unread-only"),
  roomListItems: document.getElementById("room-list-items"),
  btnSecurity: document.getElementById("btn-security"),
  btnCreateRoom: document.getElementById("btn-create-room"),
  btnGlobalThreads: document.getElementById("btn-global-threads"),
  btnShortcuts: document.getElementById("btn-shortcuts"),
  btnBackToRooms: document.getElementById("btn-back-to-rooms"),
  timelineTitle: document.getElementById("timeline-title"),
  timelineHeaderActions: document.getElementById("timeline-header-actions"),
  btnMarkRead: document.getElementById("btn-mark-read"),
  btnRoomThreads: document.getElementById("btn-room-threads"),
  btnRoomMenu: document.getElementById("btn-room-menu"),
  roomMenu: document.getElementById("room-menu"),
  btnInvite: document.getElementById("btn-invite"),
  btnLeaveRoom: document.getElementById("btn-leave-room"),
  notificationMode: document.getElementById("notification-mode"),
  btnSummarize: document.getElementById("btn-summarize"),
  summaryBox: document.getElementById("summary-box"),
  timeline: document.getElementById("timeline"),
  btnJumpLatest: document.getElementById("btn-jump-latest"),
  replyIndicator: document.getElementById("reply-indicator"),
  editIndicator: document.getElementById("edit-indicator"),
  mentionSuggestions: document.getElementById("mention-suggestions"),
  pendingImagePreview: document.getElementById("pending-image-preview"),
  composeRow: document.getElementById("compose-row"),
  composeInput: document.getElementById("compose-input"),
  composeToolbar: document.getElementById("compose-toolbar"),
  composeSend: document.getElementById("compose-send"),
  btnAttach: document.getElementById("btn-attach"),
  btnMeme: document.getElementById("btn-meme"),
  memePicker: document.getElementById("meme-picker"),
  fileInput: document.getElementById("file-input"),
  importKeysFileInput: document.getElementById("import-keys-file-input"),
  sidePanel: document.getElementById("side-panel"),
};

// =========================================================================
// Login
// =========================================================================

el.loginSubmit.addEventListener("click", () => {
  state.loggingIn = true;
  el.loginError.style.display = "none";
  el.loginSubmit.disabled = true;
  el.loginSubmit.textContent = "signing in...";
  send("LoginPassword", {
    homeserver: el.loginHomeserver.value,
    username: el.loginUsername.value,
    password: el.loginPassword.value,
  });
});

el.loginOauth.addEventListener("click", () => {
  state.loggingIn = true;
  el.loginError.style.display = "none";
  el.loginOauth.disabled = true;
  el.loginOauth.textContent = "opening browser...";
  send("LoginOAuth", { homeserver: el.loginHomeserver.value });
});

function showLoginError(msg) {
  state.loggingIn = false;
  el.loginSubmit.disabled = false;
  el.loginSubmit.textContent = "sign in";
  el.loginOauth.disabled = false;
  el.loginOauth.textContent = "continue with sso / oauth";
  el.loginError.textContent = msg;
  el.loginError.style.display = "block";
}

function enterChat() {
  state.screen = "chat";
  el.loginScreen.classList.add("hidden");
  el.chatScreen.classList.add("active");
  // The room list otherwise just sits blank until the first "Rooms" event
  // — which needs a full initial sync to complete first, easily a few
  // seconds (longer on a flaky connection, e.g. the retry-heavy path
  // right after an Android OAuth login) — with nothing to tell the user
  // whether that's still in progress or the app is just stuck.
  // `renderRooms()` overwrites this `innerHTML` outright the moment real
  // data shows up, so nothing further needs to clear it back out.
  el.roomListItems.innerHTML = `<div style="padding:12px;color:var(--text-weak);font-size:12px;text-align:center;">loading rooms...</div>`;
}

// =========================================================================
// Room list / spaces / create room
// =========================================================================

// Debounced — `renderRooms()` rebuilds every visible row from scratch, and
// on an account with thousands of rooms that's real work (not just paint;
// see `.room-row`'s `content-visibility` for the paint side of this).
// Running it on every single keystroke made fast typing itself feel
// laggy, one rebuild behind each character. A short debounce lets a burst
// of keystrokes settle before paying for the rebuild once.
let roomFilterDebounceTimer = null;
el.roomFilter.addEventListener("input", () => {
  state.roomFilter = el.roomFilter.value;
  clearTimeout(roomFilterDebounceTimer);
  roomFilterDebounceTimer = setTimeout(() => {
    // A fresh search invalidates whichever row was keyboard-highlighted —
    // the result set is about to change under it. `renderRooms()` clamps
    // this back down to -1 if it turns out there are no matches at all,
    // so arrow keys work immediately without an extra keypress to "enter"
    // the list.
    state.roomListActiveIndex = 0;
    renderRooms();
  }, 120);
});

el.roomFilter.addEventListener("keydown", (e) => {
  if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    e.preventDefault();
    if (state.visibleRoomIds.length === 0) return;
    const delta = e.key === "ArrowDown" ? 1 : -1;
    const base = state.roomListActiveIndex === -1 ? (delta === 1 ? -1 : 0) : state.roomListActiveIndex;
    state.roomListActiveIndex =
      (base + delta + state.visibleRoomIds.length) % state.visibleRoomIds.length;
    renderRooms();
    el.roomListItems
      .querySelector(".room-row.kbd-active")
      ?.scrollIntoView({ block: "nearest" });
  } else if (e.key === "Enter") {
    const roomId = state.visibleRoomIds[state.roomListActiveIndex] ?? state.visibleRoomIds[0];
    if (roomId) selectRoom(roomId);
  } else if (e.key === "Escape") {
    if (el.roomFilter.value) {
      el.roomFilter.value = "";
      state.roomFilter = "";
      state.roomListActiveIndex = -1;
      renderRooms();
    } else {
      el.roomFilter.blur();
    }
  }
});

el.btnUnreadOnly.addEventListener("click", () => {
  state.unreadOnly = !state.unreadOnly;
  el.btnUnreadOnly.classList.toggle("selected", state.unreadOnly);
  renderRooms();
});

/** Vietnamese-aware "search ignoring accents" — `normalize("NFD")` peels
 * off every combining diacritic (Latin base letters only) but leaves "đ"
 * alone, since it's its own base codepoint rather than "d" + a combining
 * mark, so that gets a manual substitution first. */
function stripDiacritics(s) {
  return s
    .replace(/đ/g, "d")
    .replace(/Đ/g, "D")
    .normalize("NFD")
    .replace(/[\u0300-\u036f]/g, "");
}
function normalizeForSearch(s) {
  return stripDiacritics(s).toLowerCase();
}

/** Space picker ("tags") — the `[ all ]` / space-name row of filter tabs
 * above the room search box. Uses a roving `tabindex` (one real tab stop,
 * Left/Right moves it) rather than every button being tabbable, the
 * standard pattern for a tab-like button group — see its keydown handler
 * below. */
function renderSpacePicker() {
  const spaces = state.rooms.filter((r) => r.is_space && !r.is_invite);
  el.spacePicker.innerHTML = "";
  if (spaces.length === 0) return;

  const entries = [{ id: null, label: "[ all ]", title: "all rooms" }, ...spaces.map((s) => ({
    id: s.room_id,
    label: s.name,
    title: s.name,
  }))];

  for (const entry of entries) {
    const btn = document.createElement("button");
    btn.textContent = entry.label;
    btn.title = entry.title;
    const isSelected = state.selectedSpace === entry.id;
    btn.className = isSelected ? "selected" : "";
    btn.tabIndex = isSelected ? 0 : -1;
    btn.dataset.spaceId = entry.id ?? "";
    btn.addEventListener("click", () => selectSpace(entry.id));
    el.spacePicker.appendChild(btn);
  }
}

// Left/Right cycles the space tabs and switches immediately (tablist
// behavior, not "move focus then press Enter/Space to activate" — with
// only a handful of spaces, activating on arrow alone is faster and
// matches how the room-list arrow keys below work too).
el.spacePicker.addEventListener("keydown", (e) => {
  if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
  e.preventDefault();
  const buttons = [...el.spacePicker.querySelectorAll("button")];
  if (buttons.length === 0) return;
  const currentIndex = Math.max(
    0,
    buttons.findIndex((b) => b.dataset.spaceId === (state.selectedSpace ?? "")),
  );
  const delta = e.key === "ArrowRight" ? 1 : -1;
  const next = buttons[(currentIndex + delta + buttons.length) % buttons.length];
  selectSpace(next.dataset.spaceId || null);
  // `selectSpace` re-renders the picker (fresh buttons), so re-query
  // rather than reuse `next` before restoring keyboard focus to it.
  el.spacePicker.querySelector(`[data-space-id="${CSS.escape(next.dataset.spaceId)}"]`)?.focus();
});

function selectSpace(spaceId) {
  state.selectedSpace = spaceId;
  if (spaceId && !state.spaceChildren[spaceId]) {
    send("ListSpaceChildren", { space_room_id: spaceId });
  }
  renderRooms();
}

function renderRooms() {
  renderSpacePicker();
  el.roomListItems.innerHTML = "";
  const filter = normalizeForSearch(state.roomFilter.trim());
  const spaceFilter = state.selectedSpace ? state.spaceChildren[state.selectedSpace] : null;
  state.visibleRoomIds = [];

  for (const room of state.rooms) {
    if (room.is_invite) {
      const row = document.createElement("div");
      row.className = "invite-row";
      row.innerHTML = `<div><b>invite:</b> ${escapeHtml(room.name)}</div>
        <div class="actions">
          <button data-accept="${room.room_id}">accept</button>
          <button data-decline="${room.room_id}">decline</button>
        </div>`;
      el.roomListItems.appendChild(row);
      continue;
    }
    if (room.is_space) continue;
    if (filter && !normalizeForSearch(room.name).includes(filter)) continue;
    if (spaceFilter && !spaceFilter.includes(room.room_id)) continue;
    if (state.unreadOnly && !(room.unread_count > 0)) continue;

    const rowIndex = state.visibleRoomIds.length;
    state.visibleRoomIds.push(room.room_id);

    const row = document.createElement("div");
    row.className =
      "room-row" +
      (room.room_id === state.selectedRoom ? " selected" : "") +
      (rowIndex === state.roomListActiveIndex ? " kbd-active" : "");

    const nameEl = document.createElement("div");
    nameEl.className = "room-name" + (room.unread_count > 0 ? " unread" : "");
    nameEl.textContent = (room.is_encrypted ? "[e] " : "") + room.name;

    row.appendChild(nameEl);
    if (room.unread_count > 0) {
      const badge = document.createElement("span");
      badge.className = "room-badge";
      badge.textContent = room.unread_count > 99 ? "99+" : String(room.unread_count);
      row.appendChild(badge);
    }
    row.addEventListener("click", () => selectRoom(room.room_id));
    row.addEventListener("mouseenter", () => {
      state.roomListActiveIndex = rowIndex;
      el.roomListItems.querySelector(".room-row.kbd-active")?.classList.remove("kbd-active");
      row.classList.add("kbd-active");
    });
    el.roomListItems.appendChild(row);
  }

  if (state.roomListActiveIndex >= state.visibleRoomIds.length) {
    state.roomListActiveIndex = state.visibleRoomIds.length - 1;
  }

  if (state.unreadOnly && el.roomListItems.children.length === 0) {
    el.roomListItems.innerHTML = `<div style="padding:12px;color:var(--text-weak);font-size:12px;text-align:center;">no unread rooms</div>`;
  }

  el.roomListItems.querySelectorAll("[data-accept]").forEach((btn) => {
    btn.addEventListener("click", () => send("AcceptInvite", { room_id: btn.dataset.accept }));
  });
  el.roomListItems.querySelectorAll("[data-decline]").forEach((btn) => {
    btn.addEventListener("click", () => send("DeclineInvite", { room_id: btn.dataset.decline }));
  });
}

el.btnCreateRoom.addEventListener("click", () => {
  showDialog(`
    <h3>create room</h3>
    <label>name</label>
    <input type="text" id="dlg-room-name" />
    <div class="checkbox-row"><input type="checkbox" id="dlg-room-public" /><label for="dlg-room-public">public</label></div>
    <div class="checkbox-row"><input type="checkbox" id="dlg-room-space" /><label for="dlg-room-space">this is a space (groups other rooms, not for messages)</label></div>
    <div class="actions">
      <button id="dlg-cancel">cancel</button>
      <button id="dlg-create">create</button>
    </div>
  `);
  document.getElementById("dlg-cancel").addEventListener("click", closeDialog);
  document.getElementById("dlg-create").addEventListener("click", () => {
    const name = document.getElementById("dlg-room-name").value.trim();
    if (!name) return;
    send("CreateRoom", {
      name,
      is_public: document.getElementById("dlg-room-public").checked,
      is_space: document.getElementById("dlg-room-space").checked,
    });
    closeDialog();
  });
});

function showDialog(html) {
  closeDialog();
  const backdrop = document.createElement("div");
  backdrop.className = "dialog-backdrop";
  backdrop.id = "active-dialog";
  backdrop.innerHTML = `<div class="dialog-box">${html}</div>`;
  backdrop.addEventListener("click", (e) => {
    if (e.target === backdrop) closeDialog();
  });
  document.body.appendChild(backdrop);
}
function closeDialog() {
  document.getElementById("active-dialog")?.remove();
}

// =========================================================================
// Room selection / header actions
// =========================================================================

function selectRoom(roomId) {
  state.selectedRoom = roomId;
  // Narrow (phone-width) layout shows one pane at a time — see the
  // `#chat-screen.room-open` rules in style.css. Harmless no-op class on
  // desktop widths, where CSS never looks at it.
  el.chatScreen.classList.add("room-open");
  // Keep the keyboard highlight in sync with the actual selection, so a
  // mouse click doesn't leave a stale `.kbd-active` outline sitting on
  // whatever row arrow keys last visited, and so arrow keys right after a
  // mouse click resume from the room that's now open rather than there.
  const idx = state.visibleRoomIds.indexOf(roomId);
  if (idx !== -1) state.roomListActiveIndex = idx;
  state.rightPanel = null;
  el.roomMenu.style.display = "none";
  cancelReply();
  cancelEdit();
  const room = state.rooms.find((r) => r.room_id === roomId);
  el.timelineTitle.textContent = room ? room.name : roomId;
  el.timelineHeaderActions.style.display = "flex";
  el.composeRow.style.display = "flex";
  renderRooms();
  renderSidePanel();
  if (!state.timelineLoaded.has(roomId)) {
    el.timeline.innerHTML = `<div id="timeline-placeholder">loading messages...</div>`;
    send("LoadTimeline", { room_id: roomId });
  } else {
    renderTimeline();
  }
  send("MarkRoomRead", { room_id: roomId });
  if (!state.roomMembers[roomId]) send("ListMembers", { room_id: roomId });
  if (!state.imagePacks[roomId]) send("ListImagePacks", { room_id: roomId });
  updateNotificationModeUi();
  send("GetNotificationMode", { room_id: roomId });
  updateThreadsButtonBadge();
}

function closeRoomMenu() {
  el.roomMenu.style.display = "none";
}
el.btnBackToRooms.addEventListener("click", () => {
  el.chatScreen.classList.remove("room-open");
});
el.btnRoomMenu.addEventListener("click", (e) => {
  e.stopPropagation();
  el.roomMenu.style.display = el.roomMenu.style.display === "none" ? "flex" : "none";
});
document.addEventListener("click", (e) => {
  if (el.roomMenu.style.display !== "none" && !el.roomMenu.contains(e.target) && e.target !== el.btnRoomMenu) {
    closeRoomMenu();
  }
});

el.btnInvite.addEventListener("click", () => {
  closeRoomMenu();
  if (!state.selectedRoom) return;
  const roomId = state.selectedRoom;
  showDialog(`
    <h3>invite to room</h3>
    <label>matrix user id</label>
    <input type="text" id="dlg-invite-user" placeholder="@user:server" />
    <div class="actions">
      <button id="dlg-cancel">cancel</button>
      <button id="dlg-invite">invite</button>
    </div>
  `);
  document.getElementById("dlg-cancel").addEventListener("click", closeDialog);
  const submit = () => {
    const userId = document.getElementById("dlg-invite-user").value.trim();
    if (!userId) return;
    send("InviteUser", { room_id: roomId, user_id: userId });
    showToast(`invite sent to ${userId}`);
    closeDialog();
  };
  document.getElementById("dlg-invite").addEventListener("click", submit);
  document.getElementById("dlg-invite-user").addEventListener("keydown", (e) => {
    if (e.key === "Enter") submit();
  });
  document.getElementById("dlg-invite-user").focus();
});

el.btnLeaveRoom.addEventListener("click", () => {
  closeRoomMenu();
  if (!state.selectedRoom) return;
  send("LeaveRoom", { room_id: state.selectedRoom });
  state.selectedRoom = null;
  state.rightPanel = null;
  el.timelineTitle.textContent = "select a conversation";
  el.timelineHeaderActions.style.display = "none";
  el.composeRow.style.display = "none";
  renderSidePanel();
});

el.notificationMode.addEventListener("change", () => {
  if (!state.selectedRoom) return;
  send("SetNotificationMode", { room_id: state.selectedRoom, mode: el.notificationMode.value });
});

function updateNotificationModeUi() {
  const mode = state.notificationModes[state.selectedRoom];
  if (mode) el.notificationMode.value = mode;
}

el.btnSummarize.addEventListener("click", () => {
  closeRoomMenu();
  if (!state.selectedRoom || state.summarizing) return;
  state.summarizing = true;
  el.btnSummarize.textContent = "summarizing...";
  el.btnSummarize.disabled = true;
  send("Summarize", { room_id: state.selectedRoom });
});

function renderSummary() {
  const text = state.summaries[state.selectedRoom];
  if (text) {
    el.summaryBox.style.display = "block";
    el.summaryBox.innerHTML = `<div class="summary-title">SUMMARY</div><div>${escapeHtml(text)}</div>`;
  } else {
    el.summaryBox.style.display = "none";
  }
}

// =========================================================================
// Timeline rendering
// =========================================================================

function renderTimeline() {
  const events = state.timelines[state.selectedRoom] || [];
  el.timeline.innerHTML = "";

  if (!state.reachedStart.has(state.selectedRoom) && events.length > 0) {
    const row = document.createElement("div");
    row.id = "load-more-row";
    if (state.paginationInFlight.has(state.selectedRoom)) {
      row.textContent = "loading older messages...";
    } else {
      const btn = document.createElement("button");
      btn.textContent = "load more messages";
      btn.addEventListener("click", () => paginateBack(state.selectedRoom));
      row.appendChild(btn);
    }
    el.timeline.appendChild(row);
  }

  if (events.length === 0) {
    const p = document.createElement("div");
    p.id = "timeline-placeholder";
    p.textContent = "no messages yet";
    el.timeline.appendChild(p);
    updateJumpLatestVisibility();
    return;
  }
  let prev = null;
  for (const event of events) {
    const grouped = isGrouped(prev, event);
    el.timeline.appendChild(renderMessage(event, { roomId: state.selectedRoom, threadId: null }, { grouped }));
    prev = event;
  }
  el.timeline.scrollTop = el.timeline.scrollHeight;
  updateJumpLatestVisibility();
}

/** Consecutive messages from the same sender, close together in time and
 * not interrupted by a reply-quote (which needs its own header to stay
 * legible), get folded into one visual block — no repeated name/avatar
 * line, Discord/Slack/Element-style. */
function isGrouped(prev, event) {
  if (!prev) return false;
  if (prev.sender !== event.sender) return false;
  if (event.reply_to_event_id) return false;
  if (prev.msg_type === "deleted" || event.msg_type === "deleted") return false;
  return Math.abs(event.timestamp - prev.timestamp) < 5 * 60 * 1000;
}

/** Re-renders exactly the DOM row for one event, in place — no
 * `renderTimeline()` teardown, so the rest of the timeline (and wherever
 * the user has scrolled to) is left completely untouched. Used for
 * edits/deletes and an image finishing its download, both of which used
 * to force a full rebuild-and-jump-to-bottom for a change to a single
 * message. Falls back to a full render if the row isn't there to patch
 * (shouldn't normally happen — caller already checked the room matches). */
function rerenderMessageInPlace(roomId, eventId) {
  if (roomId !== state.selectedRoom) return;
  const events = state.timelines[roomId] || [];
  const idx = events.findIndex((e) => e.event_id === eventId);
  if (idx === -1) return;
  const row = el.timeline.querySelector(`[data-event-id="${CSS.escape(eventId)}"]`);
  if (!row) {
    renderTimeline();
    return;
  }
  const grouped = isGrouped(idx > 0 ? events[idx - 1] : null, events[idx]);
  row.replaceWith(renderMessage(events[idx], { roomId, threadId: null }, { grouped }));
}

/** Appends exactly one new message at the bottom instead of tearing down
 * and rebuilding the whole timeline (what `renderTimeline()` used to be
 * the only option for) — keeps scroll position stable if the user has
 * scrolled up to read history, only following new messages down to the
 * bottom when they were already there. */
function appendMessage(roomId, event) {
  if (roomId !== state.selectedRoom) return;
  const events = state.timelines[roomId] || [];
  // The room's very first message: the "no messages yet" placeholder is
  // still in the DOM and needs the full render to clear it away.
  if (events.length <= 1) {
    renderTimeline();
    return;
  }
  const wasNearBottom =
    el.timeline.scrollHeight - el.timeline.scrollTop - el.timeline.clientHeight < 120;
  const grouped = isGrouped(events[events.length - 2], event);
  el.timeline.appendChild(renderMessage(event, { roomId, threadId: null }, { grouped }));
  if (wasNearBottom) el.timeline.scrollTop = el.timeline.scrollHeight;
  updateJumpLatestVisibility();
}

function paginateBack(roomId) {
  if (state.reachedStart.has(roomId) || state.paginationInFlight.has(roomId)) return;
  state.paginationInFlight.add(roomId);
  // Swap the "load more messages" button for "loading..." without a full
  // `renderTimeline()` — that unconditionally scrolls to the bottom, which
  // is exactly wrong here: this only ever fires because the user just
  // scrolled *up* near the top to trigger it.
  const row = document.getElementById("load-more-row");
  if (row) row.textContent = "loading older messages...";
  send("PaginateBack", { room_id: roomId });
}

el.timeline.addEventListener("scroll", () => {
  if (el.timeline.scrollTop < 80) paginateBack(state.selectedRoom);
  updateJumpLatestVisibility();
});

/** Shows/hides the floating "jump to latest message" button (works the
 * same way — tap/click, no hover needed — on every platform this ships
 * on) based on how far the timeline is scrolled from its bottom. Called
 * after anything that can move the scroll position or add content:
 * scrolling itself, a fresh `renderTimeline()`, and a new message
 * arriving via `appendMessage()` while scrolled up to read history. */
function updateJumpLatestVisibility() {
  const farFromBottom =
    el.timeline.scrollHeight - el.timeline.scrollTop - el.timeline.clientHeight > 300;
  el.btnJumpLatest.style.display = farFromBottom ? "flex" : "none";
}

el.btnJumpLatest.addEventListener("click", () => {
  // Not `behavior: "smooth"` — confirmed on Android, an animated scroll
  // through many rows that use `content-visibility: auto` (every
  // `.msg-row`, see its CSS) visibly glitches there: rows becoming
  // visible mid-animation render with stale/overlapping content for a
  // frame or two, since that WebView's layout doesn't keep up with each
  // intermediate scroll position the way it does with a single instant
  // jump. A plain `scrollTop` assignment resolves in one paint instead.
  el.timeline.scrollTop = el.timeline.scrollHeight;
  updateJumpLatestVisibility();
});

/** Looks for an already-loaded message by event ID — everywhere a reply
 * could plausibly be pointing at: the main timeline, the currently open
 * thread panel (its root *and* its replies), and the cached threads list.
 * A reply inside a thread pointing at another message *in that same
 * thread* previously only ever fell back to a bare "replying to a
 * message" — `state.timelines[roomId]` (the only place this used to
 * look) never contains thread replies, those live in
 * `state.rightPanel.events` instead. */
function findEvent(roomId, eventId) {
  const inTimeline = (state.timelines[roomId] || []).find((e) => e.event_id === eventId);
  if (inTimeline) return inTimeline;

  const rp = state.rightPanel;
  if (rp?.kind === "thread" && rp.roomId === roomId) {
    if (rp.root.event_id === eventId) return rp.root;
    const inThread = rp.events.find((e) => e.event_id === eventId);
    if (inThread) return inThread;
  }

  return (state.threadsByRoom[roomId] || []).find((e) => e.event_id === eventId);
}

const QUICK_REACTIONS = ["👍", "❤️", "😂", "😮", "😢", "🙏"];

/** Opens the fixed-emoji quick-picker anchored to `anchorEl`, calling
 * `toggle(emoji)` on a pick. Shared by the always-visible reaction pills
 * (clicking "+" next to them) and the hover-only "add a reaction" corner
 * trigger on bubbles with none yet — one picker implementation either
 * way, just invoked from a different spot. */
function openReactionPicker(anchorEl, toggle) {
  document.querySelectorAll(".reaction-picker").forEach((p) => p.remove());
  const picker = document.createElement("div");
  picker.className = "reaction-picker";
  for (const emoji of QUICK_REACTIONS) {
    const opt = document.createElement("button");
    opt.textContent = emoji;
    opt.addEventListener("click", (ev) => {
      ev.stopPropagation();
      toggle(emoji);
      picker.remove();
    });
    picker.appendChild(opt);
  }
  anchorEl.appendChild(picker);
  setTimeout(() => {
    document.addEventListener("click", () => picker.remove(), { once: true });
  }, 0);
}

/** Existing reaction pills only (emoji + count, highlighted if the
 * logged-in user is a reactor) — returns `null` when there are none, so
 * a bubble with no reactions doesn't reserve a row of space for an empty
 * list (that's what the corner "add reaction" trigger in `renderMessage`
 * is for instead, which only shows up on hover). Shared by the main
 * timeline and the thread panel — same `renderMessage()` call, same
 * `Command::ToggleReaction` either way. */
function renderReactions(event, ctx) {
  if (!event.reactions || event.reactions.length === 0) return null;
  const wrap = document.createElement("div");
  wrap.className = "reactions";

  const toggle = (emoji) => {
    send("ToggleReaction", { room_id: ctx.roomId, event_id: event.event_id, emoji });
  };

  for (const r of event.reactions) {
    const pill = document.createElement("button");
    pill.className = "reaction-pill" + (r.by_me ? " mine" : "");
    pill.textContent = `${r.emoji} ${r.count}`;
    pill.title = r.by_me ? "click to remove your reaction" : "click to react";
    pill.addEventListener("click", () => toggle(r.emoji));
    wrap.appendChild(pill);
  }

  const addBtn = document.createElement("button");
  addBtn.className = "reaction-add";
  addBtn.title = "add reaction";
  addBtn.textContent = "+";
  addBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    openReactionPicker(wrap, toggle);
  });
  wrap.appendChild(addBtn);

  return wrap;
}

function renderMessage(event, ctx, opts = {}) {
  const side = event.is_own ? "own" : "other";
  const row = document.createElement("div");
  row.className = `msg-row ${side}` + (opts.grouped ? " grouped" : "");
  row.dataset.eventId = event.event_id;

  const col = document.createElement("div");
  col.className = "msg-col";

  const bubble = document.createElement("div");
  bubble.className = `bubble ${side}` + (event.mentions_me ? " mentioned" : "");

  // Sender is secondary info now — small and weak, and only on the first
  // bubble of a consecutive-from-the-same-person group. Own messages never
  // show it (it's always "you"; the right-alignment already says that).
  if (!event.is_own && !opts.grouped) {
    const sender = document.createElement("div");
    sender.className = "sender";
    sender.textContent = event.sender_name;
    sender.style.color = senderColor(event.sender);
    bubble.appendChild(sender);
  }

  if (event.reply_to_event_id) {
    const preview = event.reply_to_preview || findReplyPreview(ctx.roomId, event.reply_to_event_id);
    const rp = document.createElement("div");
    rp.className = "reply-preview";
    rp.textContent = "↩ " + (preview || "replying to a message");
    // Click jumps to (and briefly highlights) the original message, so
    // "replying to" is a real, followable relationship, not just a
    // static caption — scoped to whichever container this message is
    // rendered in (main timeline vs. thread panel), since that's the only
    // place the target could actually be loaded.
    rp.addEventListener("click", (e) => {
      e.stopPropagation();
      scrollToMessage(ctx.threadId ? "thread-messages" : "timeline", event.reply_to_event_id);
    });
    bubble.appendChild(rp);
  }

  // Some backend paths (thread replies specifically, fetched via raw
  // `/relations` JSON rather than matrix-sdk-ui's typed timeline) lump
  // every attachment into `msg_type: "file"` regardless of the real
  // type — a second line of defense, on top of the backend fix, so an
  // image doesn't just silently render as its filename in plain text.
  const looksLikeImage = /\.(png|jpe?g|gif|webp|bmp)$/i.test(event.body || "");
  if ((event.msg_type === "image" || (event.msg_type === "file" && looksLikeImage)) && event.media_url) {
    if (event.media_mime) state.imageMime[event.media_url] = event.media_mime;
    if (event.media_encryption) state.imageEncryption[event.media_url] = event.media_encryption;
    const cached = state.imageCache[event.media_url];
    const declaredMime = event.media_mime;
    const knownUnviewable =
      (declaredMime && UNVIEWABLE_IMAGE_MIMES.has(declaredMime) && declaredMime) ||
      state.imageUnviewable[event.media_url];

    if (knownUnviewable) {
      // WebKitGTK has no decoder for this format (HEIC/HEIF from an
      // iPhone camera, TIFF, ...) — an `<img>` here would just be the
      // browser's own broken-image icon, no different from not handling
      // it at all. Offer to open it in the system's own viewer instead,
      // same as the "open externally" path already used for videos.
      const placeholder = document.createElement("div");
      placeholder.className = "image-placeholder";
      placeholder.textContent = `🖼 photo format not viewable here (${knownUnviewable}) — click to open`;
      placeholder.addEventListener("click", () => {
        send("OpenMediaExternally", {
          mxc_uri: event.media_url,
          filename: event.body || "image",
          media_encryption: state.imageEncryption[event.media_url] || null,
        });
      });
      bubble.appendChild(placeholder);
    } else if (cached) {
      const wrap = document.createElement("div");
      wrap.className = "media-wrap";
      const img = document.createElement("img");
      img.alt = event.body || "image";
      img.src = cached;
      wrap.appendChild(img);
      wrap.appendChild(makeDownloadButton(event));
      bubble.appendChild(wrap);
    } else {
      // A bare `<img src="">` on a not-yet-loaded (or failed) image shows
      // the browser's own broken-image icon plus the alt text — which is
      // exactly the "just shows image.png" symptom. This is a proper
      // loading/failed placeholder instead, and — once the 3 automatic
      // retries in the `ImageFetchFailed` handler are exhausted — a click
      // target to try again by hand rather than being stuck forever.
      const failed = (imageFetchAttempts[event.media_url] || 0) > 0;
      const placeholder = document.createElement("div");
      placeholder.className = "image-placeholder" + (failed ? " failed" : "");
      placeholder.textContent = failed ? "⟳ image failed to load — click to retry" : "loading image...";
      if (failed) {
        placeholder.addEventListener("click", () => {
          delete imageFetchAttempts[event.media_url];
          state.imageRequested.delete(event.media_url);
          requestImage(event.media_url);
          placeholder.textContent = "loading image...";
          placeholder.classList.remove("failed");
        });
      }
      bubble.appendChild(placeholder);
      if (!state.imageRequested.has(event.media_url)) requestImage(event.media_url);
    }
  } else if (event.msg_type === "video" && event.media_url) {
    if (event.media_encryption) state.imageEncryption[event.media_url] = event.media_encryption;
    const attachment = document.createElement("div");
    attachment.className = "media-attachment";

    const play = document.createElement("button");
    play.className = "media-attachment-action";
    play.textContent = `▶ ${event.body || "video"}`;
    play.title = "play video";
    play.addEventListener("click", () => {
      send("PlayVideo", {
        mxc_uri: event.media_url,
        filename: event.body || "video.mp4",
        media_encryption: state.imageEncryption[event.media_url] || null,
      });
    });
    attachment.appendChild(play);
    attachment.appendChild(makeDownloadButton(event));
    bubble.appendChild(attachment);
  } else if (event.msg_type === "file" && event.media_url) {
    if (event.media_encryption) state.imageEncryption[event.media_url] = event.media_encryption;
    const attachment = document.createElement("div");
    attachment.className = "media-attachment";

    const open = document.createElement("button");
    open.className = "media-attachment-action";
    open.textContent = `📎 ${event.body || "file"}`;
    open.title = "open file";
    open.addEventListener("click", () => {
      send("OpenMediaExternally", {
        mxc_uri: event.media_url,
        filename: event.body || "file",
        media_encryption: state.imageEncryption[event.media_url] || null,
      });
    });
    attachment.appendChild(open);
    attachment.appendChild(makeDownloadButton(event));
    bubble.appendChild(attachment);
  } else if (event.msg_type === "deleted") {
    const body = document.createElement("div");
    body.className = "body deleted";
    body.textContent = "[message deleted]";
    bubble.appendChild(body);
  } else {
    const body = document.createElement("div");
    body.className = "body" + (event.msg_type === "notice" ? " notice" : "");
    body.innerHTML = renderMarkdown(event.body || "");
    bubble.appendChild(body);
  }

  const meta = document.createElement("div");
  meta.className = "meta";
  meta.textContent = new Date(event.timestamp).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  if (state.sendingLocalIds && state.sendingLocalIds.has(event.event_id)) {
    meta.textContent += " · sending...";
  }
  bubble.appendChild(meta);

  // A message with a thread stays flagged all the time, not just on
  // hover — otherwise it's easy to miss that replies exist at all.
  if (!ctx.threadId && event.thread_count) {
    const badge = document.createElement("div");
    badge.className = "thread-badge";
    badge.textContent = `🧵 ${event.thread_count} ${event.thread_count === 1 ? "reply" : "replies"} →`;
    badge.addEventListener("click", () => openThread(ctx.roomId, event));
    bubble.appendChild(badge);
  }

  const reactions = renderReactions(event, ctx);
  if (reactions) bubble.appendChild(reactions);

  // A message with no reactions yet gets a small "add reaction" icon
  // floating on the bubble's corner instead — hidden until hover (see the
  // `.reaction-trigger` CSS, same `visibility` trick as `.actions`), so it
  // costs no space at all rather than reserving an empty pill row like
  // `renderReactions` used to unconditionally do.
  if (!reactions) {
    const trigger = document.createElement("button");
    trigger.className = "reaction-trigger";
    trigger.title = "add reaction";
    trigger.textContent = "🙂+";
    trigger.addEventListener("click", (e) => {
      e.stopPropagation();
      openReactionPicker(bubble, (emoji) => {
        send("ToggleReaction", { room_id: ctx.roomId, event_id: event.event_id, emoji });
      });
    });
    bubble.appendChild(trigger);
  }

  // `.actions` only ever reveals on `:hover` (see its CSS) — there's no
  // hover on a touchscreen, so on mobile it was simply never reachable at
  // all. A tap on the bubble toggles it there instead (see the
  // `.actions-open` mobile-only CSS); harmless everywhere else, since
  // nothing there reads that class. Skipped when the tap actually landed
  // on a real interactive element inside the bubble (a link, the reply
  // preview, an image, a button) so it doesn't fight with that element's
  // own click behavior.
  bubble.addEventListener("click", (e) => {
    if (e.target.closest("a, button, img")) return;
    row.classList.toggle("actions-open");
  });

  col.appendChild(bubble);

  const actions = document.createElement("div");
  actions.className = "actions";

  // Short text labels, not full "[ reply in thread ]"-style phrases — the
  // side gutter stacks them vertically (see CSS), so each just needs to
  // fit on its own line rather than the whole row fitting side by side.
  if (!ctx.threadId && !event.thread_count) {
    const threadLabel = document.createElement("a");
    threadLabel.textContent = "[ thread ]";
    threadLabel.addEventListener("click", () => openThread(ctx.roomId, event));
    actions.appendChild(threadLabel);
  }

  const replyLink = document.createElement("a");
  replyLink.textContent = "[ reply ]";
  replyLink.addEventListener("click", () => startReply(ctx.roomId, ctx.threadId, event));
  actions.appendChild(replyLink);

  const shareLink = document.createElement("a");
  shareLink.textContent = "[ share ]";
  shareLink.addEventListener("click", () => shareMessage(ctx.roomId, event.event_id));
  actions.appendChild(shareLink);

  if (event.is_own && event.msg_type !== "image" && event.msg_type !== "deleted") {
    const editLink = document.createElement("a");
    editLink.textContent = "[ edit ]";
    editLink.addEventListener("click", () => startEdit(ctx.roomId, ctx.threadId, event));
    actions.appendChild(editLink);
  }
  if (event.is_own) {
    const delLink = document.createElement("a");
    delLink.textContent = "[ delete ]";
    delLink.addEventListener("click", () => {
      send("DeleteMessage", { room_id: ctx.roomId, event_id: event.event_id });
    });
    actions.appendChild(delLink);
  }
  // Actions sit in the gutter beside the bubble — to its left for your
  // own (right-aligned) messages, to its right for everyone else's — not
  // stacked below it. Below-the-bubble actions made every hover shift the
  // whole message list vertically (annoying while scrolling); a side
  // gutter never shifts anything, it just fades in in place. It's
  // absolutely positioned (see CSS) rather than a normal flex sibling so
  // that stacking several text labels vertically — needed for them to fit
  // at all in a narrow container like the thread panel — can't inflate a
  // short one-line message's row height either.
  col.appendChild(actions);
  row.appendChild(col);

  return row;
}

function findReplyPreview(roomId, eventId) {
  const e = findEvent(roomId, eventId);
  if (!e) return null;
  return `${e.sender_name}: ${truncate(e.body || "", 60)}`;
}

function truncate(s, n) {
  return s.length > n ? s.slice(0, n) + "…" : s;
}

// Stable per-sender color, picked from a fixed palette by hashing the
// sender's matrix ID — so the same person always gets the same color
// across rooms and sessions, without needing a server-assigned one.
const SENDER_COLORS = [
  "#6f9bd1", "#c98a4a", "#5fae82", "#c97ea0",
  "#9b82c4", "#4fa3ad", "#c97656", "#8fae4f",
];
function senderColor(senderId) {
  let hash = 0;
  for (let i = 0; i < senderId.length; i++) {
    hash = (hash * 31 + senderId.charCodeAt(i)) | 0;
  }
  return SENDER_COLORS[Math.abs(hash) % SENDER_COLORS.length];
}

const imageFetchAttempts = {}; // mxc_uri -> number of attempts so far

function requestImage(mxcUri) {
  if (state.imageRequested.has(mxcUri)) return;
  state.imageRequested.add(mxcUri);
  send("FetchImage", {
    key: mxcUri,
    mxc_uri: mxcUri,
    media_encryption: state.imageEncryption[mxcUri] || null,
  });
}

/** Small "⬇" button that saves an attachment (image/video/file) to the
 * user's Downloads folder via `Command::DownloadMedia`, distinct from
 * `PlayVideo`/`OpenMediaExternally`'s temp-file-then-open behavior — this
 * one is meant to leave a real copy behind. */
function makeDownloadButton(event) {
  const btn = document.createElement("button");
  btn.className = "download-btn";
  btn.title = "download";
  btn.textContent = "⬇";
  btn.addEventListener("click", (e) => {
    e.stopPropagation();
    send("DownloadMedia", {
      mxc_uri: event.media_url,
      filename: event.body || "download",
      media_encryption: state.imageEncryption[event.media_url] || null,
    });
  });
  return btn;
}

// WebKitGTK (this app's webview) has no built-in decoder for these —
// showing them as an `<img>` just gets the browser's own broken-image
// icon. PNG/JPEG/GIF/BMP/WEBP all decode fine and aren't listed here.
const UNVIEWABLE_IMAGE_MIMES = new Set([
  "image/heic", "image/heif", "image/heic-sequence", "image/heif-sequence",
  "image/tiff",
]);

/** Sniffs an image format from its magic bytes — covers everything
 * `UNVIEWABLE_IMAGE_MIMES` cares about too (HEIC/HEIF/TIFF), not just the
 * viewable formats, so a sender that didn't set `content.info.mimetype`
 * still gets routed to the "open externally" placeholder instead of a
 * silently broken `<img>`. Returns null if nothing recognizable matched. */
function sniffImageMime(u8) {
  if (u8[0] === 0x89 && u8[1] === 0x50) return "image/png";
  if (u8[0] === 0xff && u8[1] === 0xd8) return "image/jpeg";
  if (u8[0] === 0x47 && u8[1] === 0x49) return "image/gif";
  if (u8[0] === 0x42 && u8[1] === 0x4d) return "image/bmp";
  if (u8[8] === 0x57 && u8[9] === 0x45 && u8[10] === 0x42 && u8[11] === 0x50) return "image/webp";
  if (u8[0] === 0x49 && u8[1] === 0x49 && u8[2] === 0x2a) return "image/tiff"; // little-endian TIFF
  if (u8[0] === 0x4d && u8[1] === 0x4d && u8[3] === 0x2a) return "image/tiff"; // big-endian TIFF
  // ISO base media file format (HEIC/HEIF/AVIF all use this container):
  // bytes 4-7 are "ftyp", followed by a 4-byte brand that says which.
  if (u8[4] === 0x66 && u8[5] === 0x74 && u8[6] === 0x79 && u8[7] === 0x70) {
    const brand = String.fromCharCode(u8[8], u8[9], u8[10], u8[11]);
    if (brand === "avif" || brand === "avis") return "image/avif";
    if (["heic", "heix", "heim", "heis", "hevc", "hevx", "mif1", "msf1"].includes(brand)) {
      return "image/heic";
    }
  }
  return null;
}

function bytesToDataUrl(bytes, mimeHint) {
  const u8 = new Uint8Array(bytes);
  const mime = mimeHint || sniffImageMime(u8) || "application/octet-stream";
  let binary = "";
  const chunk = 0x8000;
  for (let i = 0; i < u8.length; i += chunk) {
    binary += String.fromCharCode.apply(null, u8.subarray(i, i + chunk));
  }
  return `data:${mime};base64,${btoa(binary)}`;
}

// ---- Minimal markdown: **bold**, *italic*, `code`, ```blocks```, links,
// bare URLs, and paragraph breaks on blank lines. Deliberately small — just
// enough for normal chat formatting, not a full CommonMark implementation.
function renderMarkdown(text) {
  const escaped = escapeHtml(text);
  const withBlocks = escaped.replace(/```([\s\S]*?)```/g, (_, code) => `<pre>${code}</pre>`);
  const paragraphs = withBlocks.split(/\n\n+/).map((p) => {
    if (p.startsWith("<pre>")) return p;
    let html = p
      .replace(/`([^`]+)`/g, "<code>$1</code>")
      .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
      .replace(/(?<!\*)\*([^*]+)\*(?!\*)/g, "<em>$1</em>")
      .replace(/\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)/g, '<a href="$2" target="_blank">$1</a>')
      .replace(/(^|[\s(])(https?:\/\/[^\s<]+)/g, '$1<a href="$2" target="_blank">$2</a>')
      .replace(/\n/g, "<br>");
    return `<p>${html}</p>`;
  });
  return paragraphs.join("");
}

function escapeHtml(s) {
  const d = document.createElement("div");
  d.textContent = s;
  return d.innerHTML;
}

// =========================================================================
// Reply / edit
// =========================================================================

function startReply(roomId, threadId, event) {
  state.pendingReply = {
    roomId,
    threadId,
    eventId: event.event_id,
    preview: `${event.sender_name}: ${truncate(event.body || "", 60)}`,
  };
  renderReplyIndicator();
  (threadId ? document.getElementById("thread-compose-input") : el.composeInput)?.focus();
}
function cancelReply() {
  state.pendingReply = null;
  renderReplyIndicator();
}
function renderReplyIndicator() {
  const r = state.pendingReply;
  if (!r || r.threadId) {
    el.replyIndicator.style.display = "none";
  } else {
    el.replyIndicator.style.display = "flex";
    el.replyIndicator.innerHTML = `<span>↩ replying to: ${escapeHtml(r.preview)}</span><a id="cancel-reply">[ cancel ]</a>`;
    document.getElementById("cancel-reply").addEventListener("click", cancelReply);
  }

  const threadIndicator = document.getElementById("thread-reply-indicator");
  if (!threadIndicator) return;
  if (!r || !r.threadId) {
    threadIndicator.style.display = "none";
    return;
  }
  threadIndicator.style.display = "flex";
  threadIndicator.style.cssText += "padding:4px 12px;font-size:12px;color:var(--text-weak);justify-content:space-between;";
  threadIndicator.innerHTML = `<span>↩ replying to: ${escapeHtml(r.preview)}</span><a id="thread-cancel-reply">[ cancel ]</a>`;
  document.getElementById("thread-cancel-reply").addEventListener("click", cancelReply);
}

function startEdit(roomId, threadId, event) {
  state.pendingEdit = { roomId, threadId, eventId: event.event_id };
  if (threadId) {
    const box = document.getElementById("thread-compose-input");
    if (box) {
      box.value = event.body || "";
      autoResizeTextarea(box);
    }
  } else {
    el.composeInput.value = event.body || "";
    autoResizeTextarea(el.composeInput);
  }
  renderEditIndicator();
}
function cancelEdit() {
  state.pendingEdit = null;
  renderEditIndicator();
}
function renderEditIndicator() {
  const ed = state.pendingEdit;
  if (!ed || ed.threadId) {
    el.editIndicator.style.display = "none";
    return;
  }
  el.editIndicator.style.display = "flex";
  el.editIndicator.innerHTML = `<span>editing message</span><a id="cancel-edit">[ cancel ]</a>`;
  document.getElementById("cancel-edit").addEventListener("click", () => {
    cancelEdit();
    el.composeInput.value = "";
    autoResizeTextarea(el.composeInput);
  });
}

// =========================================================================
// @mention autocomplete (main compose box)
// =========================================================================

function activeMentionQuery(text, cursorPos) {
  const upToCursor = text.slice(0, cursorPos);
  const at = upToCursor.lastIndexOf("@");
  if (at === -1) return null;
  const after = upToCursor.slice(at + 1);
  if (/\s/.test(after)) return null;
  return { query: after, at };
}

/** Wires @mention autocomplete onto any compose `<input>` — shared by the
 * main compose box and the thread panel's (which gets a fresh DOM each
 * time the panel re-renders, so this gets re-called there too rather than
 * bound once). `getRoomId`/`mentionsList` are functions/arrays so each
 * caller supplies its own room and mention-tracking state. */
function wireMentionAutocomplete(inputEl, suggestionsEl, getRoomId, mentionsList) {
  inputEl.addEventListener("input", () => {
    const m = activeMentionQuery(inputEl.value, inputEl.selectionStart);
    const roomId = getRoomId();
    if (!m || !roomId) {
      suggestionsEl.style.display = "none";
      return;
    }
    const members = state.roomMembers[roomId] || [];
    const matches = members
      .filter(([, name]) => name.toLowerCase().includes(m.query.toLowerCase()))
      .slice(0, 6);
    if (matches.length === 0) {
      suggestionsEl.style.display = "none";
      return;
    }
    suggestionsEl.style.display = "block";
    suggestionsEl.innerHTML = "";
    for (const [userId, name] of matches) {
      const item = document.createElement("div");
      item.className = "mention-item";
      item.textContent = name;
      item.addEventListener("click", () => {
        const before = inputEl.value.slice(0, m.at);
        const after = inputEl.value.slice(inputEl.selectionStart);
        inputEl.value = `${before}@${name} ${after}`;
        mentionsList.push({ userId, displayName: name });
        suggestionsEl.style.display = "none";
        inputEl.focus();
      });
      suggestionsEl.appendChild(item);
    }
  });
}

wireMentionAutocomplete(el.composeInput, el.mentionSuggestions, () => state.selectedRoom, state.composeMentions);

function buildMentionHtml(body, mentions) {
  if (mentions.length === 0) return { ids: [], html: null };
  const candidates = mentions.slice().sort((a, b) => b.displayName.length - a.displayName.length);
  const ids = new Set();
  let html = "";
  let rest = body;
  outer: while (rest.length > 0) {
    for (const { userId, displayName } of candidates) {
      const needle = "@" + displayName;
      if (rest.startsWith(needle)) {
        ids.add(userId);
        html += `<a href="https://matrix.to/#/${encodeURIComponent(userId)}">${escapeHtml(needle)}</a>`;
        rest = rest.slice(needle.length);
        continue outer;
      }
    }
    html += escapeHtml(rest[0]);
    rest = rest.slice(1);
  }
  return { ids: [...ids], html: ids.size > 0 ? html : null };
}

// =========================================================================
// Compose editor — markdown shortcuts/toolbar, auto-grow
// =========================================================================
// A small hand-rolled markdown editor (keyboard shortcuts + a toolbar that
// wrap the current selection in the right syntax) rather than a full
// WYSIWYG rich-text widget — consistent with this app's plain-JS,
// no-bundler setup and with `renderMarkdown`'s own "just enough for chat
// formatting" scope. Shared by the main compose box and the thread
// panel's own copy (rebuilt from scratch on every side-panel re-render).

const MD_WRAPPERS = {
  bold: { before: "**", after: "**" },
  italic: { before: "*", after: "*" },
  code: { before: "`", after: "`" },
};

/** Wraps the current selection in `before`/`after` — or, if it's already
 * wrapped in exactly that, unwraps it instead, so e.g. pressing Ctrl+B on
 * already-bold text un-bolds it rather than nesting `****`. */
function toggleWrap(textarea, before, after) {
  const { value, selectionStart: start, selectionEnd: end } = textarea;
  const selected = value.slice(start, end);
  const already =
    value.slice(start - before.length, start) === before &&
    value.slice(end, end + after.length) === after;

  let newValue, newStart, newEnd;
  if (already) {
    newValue = value.slice(0, start - before.length) + selected + value.slice(end + after.length);
    newStart = start - before.length;
    newEnd = newStart + selected.length;
  } else {
    newValue = value.slice(0, start) + before + selected + after + value.slice(end);
    newStart = start + before.length;
    newEnd = newStart + selected.length;
  }
  textarea.value = newValue;
  textarea.setSelectionRange(newStart, newEnd);
  textarea.dispatchEvent(new Event("input", { bubbles: true }));
  textarea.focus();
}

/** Fences the selection off as a code block — with nothing selected,
 * leaves the cursor ready to type inside a fresh empty one. */
function insertCodeBlock(textarea) {
  const { value, selectionStart: start, selectionEnd: end } = textarea;
  const selected = value.slice(start, end);
  const needsLeadingNewline = start > 0 && value[start - 1] !== "\n";
  const needsTrailingNewline = end < value.length && value[end] !== "\n";
  const block = `${needsLeadingNewline ? "\n" : ""}\`\`\`\n${selected}\n\`\`\`${needsTrailingNewline ? "\n" : ""}`;
  textarea.value = value.slice(0, start) + block + value.slice(end);
  const cursor = start + (needsLeadingNewline ? 1 : 0) + 4 + selected.length;
  textarea.setSelectionRange(cursor, cursor);
  textarea.dispatchEvent(new Event("input", { bubbles: true }));
  textarea.focus();
}

/** Wraps the selection as a markdown link with the cursor left right after
 * the `(`, ready to type the URL — same UX GitHub's own comment box uses.
 * With nothing selected, leaves the cursor inside the `[]` instead. */
function insertLink(textarea) {
  const { value, selectionStart: start, selectionEnd: end } = textarea;
  const selected = value.slice(start, end);
  textarea.value = value.slice(0, start) + `[${selected}]()` + value.slice(end);
  const cursor = selected ? start + selected.length + 3 : start + 1;
  textarea.setSelectionRange(cursor, cursor);
  textarea.dispatchEvent(new Event("input", { bubbles: true }));
  textarea.focus();
}

/** Same bare-URL shape `renderMarkdown` itself auto-links — kept in sync
 * with that regex on purpose, so "does pasting this turn into a link"
 * matches "does typing this turn into a link" once sent. */
function isLikelyUrl(text) {
  return /^https?:\/\/\S+$/i.test(text);
}

/** Turns the current selection into a markdown link to `url`, replacing
 * it outright rather than inserting alongside it. */
function wrapSelectionAsLink(textarea, url) {
  const { value, selectionStart: start, selectionEnd: end } = textarea;
  const selected = value.slice(start, end);
  const linked = `[${selected}](${url})`;
  textarea.value = value.slice(0, start) + linked + value.slice(end);
  const cursor = start + linked.length;
  textarea.setSelectionRange(cursor, cursor);
  textarea.dispatchEvent(new Event("input", { bubbles: true }));
}

function applyMarkdownAction(textarea, action) {
  if (action === "codeblock") return insertCodeBlock(textarea);
  if (action === "link") return insertLink(textarea);
  const wrapper = MD_WRAPPERS[action];
  if (wrapper) toggleWrap(textarea, wrapper.before, wrapper.after);
}

/** Grows a compose `<textarea>` to fit its content up to `.compose-textarea`'s
 * `max-height` (see CSS), then scrolls internally past that — a one-line
 * "hey" doesn't reserve space for a paragraph, but a pasted paragraph
 * doesn't swallow the whole timeline either. */
function autoResizeTextarea(textarea) {
  // An empty textarea should always sit at its CSS `min-height` (one
  // line), not grow to fit the placeholder — some WebView builds
  // (confirmed on Android) compute `scrollHeight` against the wrapped
  // placeholder text when there's no value, which ballooned the compose
  // box to several lines tall on phone widths (long placeholder + larger
  // mobile font-size wraps a lot) even before anything was typed.
  if (!textarea.value) {
    textarea.style.height = "";
    return;
  }
  textarea.style.height = "auto";
  textarea.style.height = textarea.scrollHeight + "px";
}

/** Wires the shared compose-editor keyboard behavior onto one `<textarea>`:
 * Enter sends (Shift+Enter inserts a literal newline instead), Ctrl+B/I/E/K
 * apply markdown, Ctrl+Shift+E fences a code block, and the box auto-grows
 * as it's typed into. `isSuggestionsOpen()` lets the mention-autocomplete
 * dropdown claim Enter for itself (pick a suggestion, not send) — same
 * priority the old plain `<input>` gave it. */
function wireComposeEditor(textarea, { onSend, isSuggestionsOpen }) {
  textarea.addEventListener("input", () => autoResizeTextarea(textarea));
  // Pasting a URL onto a text selection turns the selection into a link to
  // that URL — `[selected text](url)` — instead of just overwriting it
  // with the raw address, the same "paste a link onto a selection" UX
  // GitHub/Slack/Notion all share. A paste with nothing selected (or that
  // isn't a bare URL) falls through to the browser's own default paste
  // untouched.
  textarea.addEventListener("paste", (e) => {
    if (textarea.selectionStart === textarea.selectionEnd) return;
    const pasted = (e.clipboardData || window.clipboardData)?.getData("text/plain")?.trim();
    if (!pasted || !isLikelyUrl(pasted)) return;
    e.preventDefault();
    // Also stops the document-level paste handler (image-paste detection)
    // from seeing this event — it has nothing to do here since this is
    // plain text, but without this it still fires its "no image, trying
    // clipboard API..." fallback toast right after the link is inserted.
    e.stopPropagation();
    wrapSelectionAsLink(textarea, pasted);
    textarea.focus();
  });
  textarea.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey) {
      if (isSuggestionsOpen()) return;
      e.preventDefault();
      onSend();
      return;
    }
    if ((e.ctrlKey || e.metaKey) && e.shiftKey && e.key.toLowerCase() === "e") {
      e.preventDefault();
      // Also keeps this from bubbling to the app-wide shortcut handler —
      // it has no binding on Ctrl+Shift+E, but Ctrl+K below collides with
      // that handler's "focus room search" (same key most editors use for
      // "insert link" too); stopping propagation here is what lets the
      // compose box's meaning win while it has focus.
      e.stopPropagation();
      insertCodeBlock(textarea);
      return;
    }
    if ((e.ctrlKey || e.metaKey) && !e.shiftKey) {
      const action = { b: "bold", i: "italic", e: "code", k: "link" }[e.key.toLowerCase()];
      if (action) {
        e.preventDefault();
        e.stopPropagation();
        applyMarkdownAction(textarea, action);
      }
    }
  });
}

/** Wires the B/I/code/link toolbar row above a compose box. */
function wireMarkdownToolbar(toolbarEl, textarea) {
  toolbarEl?.querySelectorAll("[data-md]").forEach((btn) => {
    btn.addEventListener("click", (e) => {
      e.preventDefault();
      applyMarkdownAction(textarea, btn.dataset.md);
    });
  });
}

// =========================================================================
// Compose / send
// =========================================================================

function sendCurrentMessage() {
  const body = el.composeInput.value.trim();
  if (state.pendingImage && !state.pendingImage.threadId) {
    confirmPendingImage();
    return;
  }
  if (!body || !state.selectedRoom) return;
  el.composeInput.value = "";
  autoResizeTextarea(el.composeInput);
  el.mentionSuggestions.style.display = "none";

  if (state.pendingEdit && !state.pendingEdit.threadId) {
    const { ids, html } = buildMentionHtml(body, state.composeMentions);
    send("EditMessage", {
      room_id: state.selectedRoom,
      event_id: state.pendingEdit.eventId,
      body,
      mentions: ids,
      html_body: html,
    });
    cancelEdit();
    state.composeMentions = [];
    el.composeInput.focus();
    return;
  }

  const { ids, html } = buildMentionHtml(body, state.composeMentions);
  const replyTo = state.pendingReply && !state.pendingReply.threadId ? state.pendingReply.eventId : null;
  send("SendMessage", {
    room_id: state.selectedRoom,
    body,
    thread_id: null,
    mentions: ids,
    html_body: html,
    local_id: crypto.randomUUID(),
    reply_to_event_id: replyTo,
  });
  cancelReply();
  state.composeMentions = [];
  // Enter already loses focus (that's what triggers `lost_focus`-style
  // send elsewhere), so put it back — otherwise every message needs a
  // re-click on the input before the next one can be typed.
  el.composeInput.focus();
}
el.composeSend.addEventListener("click", sendCurrentMessage);
wireComposeEditor(el.composeInput, {
  onSend: sendCurrentMessage,
  isSuggestionsOpen: () => el.mentionSuggestions.style.display !== "none",
});
wireMarkdownToolbar(el.composeToolbar, el.composeInput);

// ---- Custom emoji / meme picker (MSC2545 room image packs) ----
// Shared between the main compose row and the thread panel's own copy
// (wired separately in `renderSidePanel`, since that DOM is rebuilt).
function renderMemePicker(pickerEl, roomId, threadId) {
  const images = state.imagePacks[roomId] || [];
  pickerEl.innerHTML = "";
  if (images.length === 0) {
    const empty = document.createElement("div");
    empty.className = "meme-empty";
    empty.textContent = "no custom emoji yet";
    pickerEl.appendChild(empty);
  }
  for (const img of images) {
    const cell = document.createElement("div");
    cell.className = "meme-cell-wrap";

    const btn = document.createElement("button");
    btn.className = "meme-cell";
    btn.title = `:${img.shortcode}: (${img.pack_name})`;
    const cached = state.imageCache[img.url];
    if (cached) {
      const thumb = document.createElement("img");
      thumb.src = cached;
      btn.appendChild(thumb);
    } else {
      if (!state.imageRequested.has(img.url)) requestImage(img.url);
    }
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      showToast(`sending :${img.shortcode}:${threadId ? " into thread" : ""}...`);
      send("SendMeme", { room_id: roomId, thread_id: threadId, url: img.url, shortcode: img.shortcode });
      pickerEl.style.display = "none";
    });
    cell.appendChild(btn);

    const del = document.createElement("button");
    del.className = "meme-cell-remove";
    del.title = `remove :${img.shortcode}:`;
    del.textContent = "×";
    del.addEventListener("click", (e) => {
      e.stopPropagation();
      send("RemoveImagePackEmoji", { room_id: roomId, scope: img.scope, shortcode: img.shortcode });
    });
    cell.appendChild(del);

    pickerEl.appendChild(cell);
  }

  const addCell = document.createElement("button");
  addCell.className = "meme-cell meme-add-cell";
  addCell.title = "add a new custom emoji";
  addCell.textContent = "+";
  addCell.addEventListener("click", (e) => {
    e.stopPropagation();
    openAddEmojiForm(pickerEl, roomId);
  });
  pickerEl.appendChild(addCell);
}

/** Small inline form (file + shortcode + room-vs-personal) replacing the
 * picker's contents — reachable from either picker's "+" cell, so it's
 * one implementation shared by the main compose box and the thread panel. */
function openAddEmojiForm(pickerEl, roomId) {
  pickerEl.innerHTML = `
    <div class="meme-add-form">
      <input type="file" id="meme-add-file" accept="image/*" />
      <input type="text" id="meme-add-shortcode" placeholder="shortcode (no colons)" />
      <label><input type="radio" name="meme-add-scope" value="room" checked /> this room</label>
      <label><input type="radio" name="meme-add-scope" value="personal" /> personal (all rooms)</label>
      <div class="meme-add-actions">
        <button id="meme-add-cancel" class="small-btn">cancel</button>
        <button id="meme-add-submit" class="small-btn">add</button>
      </div>
    </div>`;
  pickerEl.querySelector("#meme-add-cancel").addEventListener("click", (e) => {
    e.stopPropagation();
    renderMemePicker(pickerEl, roomId, pickerEl === el.memePicker ? null : state.rightPanel?.root?.event_id);
  });
  pickerEl.querySelector("#meme-add-submit").addEventListener("click", (e) => {
    e.stopPropagation();
    const fileInput = pickerEl.querySelector("#meme-add-file");
    const shortcode = pickerEl.querySelector("#meme-add-shortcode").value.trim();
    const scope = pickerEl.querySelector('input[name="meme-add-scope"]:checked').value;
    const file = fileInput.files[0];
    if (!file || !shortcode) {
      showToast("pick an image and a shortcode first");
      return;
    }
    const reader = new FileReader();
    reader.onload = () => {
      const bytes = Array.from(new Uint8Array(reader.result));
      send("AddImagePackEmoji", { room_id: roomId, scope, shortcode, bytes, mime: file.type || "image/png" });
      pickerEl.style.display = "none";
    };
    reader.readAsArrayBuffer(file);
  });
}

function closeMemePickers() {
  el.memePicker.style.display = "none";
  const threadPicker = document.getElementById("thread-meme-picker");
  if (threadPicker) threadPicker.style.display = "none";
}

el.btnMeme.addEventListener("click", (e) => {
  e.stopPropagation();
  const show = el.memePicker.style.display === "none";
  closeMemePickers();
  if (show) {
    renderMemePicker(el.memePicker, state.selectedRoom, null);
    el.memePicker.style.display = "grid";
  }
});
document.addEventListener("click", (e) => {
  if (el.memePicker.style.display !== "none" && !el.memePicker.contains(e.target) && e.target !== el.btnMeme) {
    el.memePicker.style.display = "none";
  }
  const threadPicker = document.getElementById("thread-meme-picker");
  const threadBtn = document.getElementById("thread-btn-meme");
  if (threadPicker && threadPicker.style.display !== "none" && !threadPicker.contains(e.target) && e.target !== threadBtn) {
    threadPicker.style.display = "none";
  }
});

// ---- Images ----
el.btnAttach.addEventListener("click", () => el.fileInput.click());
el.fileInput.addEventListener("change", () => {
  const file = el.fileInput.files[0];
  if (!file) return;
  loadPendingImage(file, state.selectedRoom, null);
  el.fileInput.value = "";
});
let pasteHandledByEvent = false;

/** Which room/thread a paste should land in — the open thread panel's
 * compose box if that's what's focused, otherwise the main compose box's
 * room. Shared by both paste paths below. */
function pasteTarget() {
  const inThread = document.activeElement?.id === "thread-compose-input" && state.rightPanel?.kind === "thread";
  if (inThread) {
    return { roomId: state.rightPanel.roomId, threadId: state.rightPanel.root.event_id };
  }
  return { roomId: state.selectedRoom, threadId: null };
}

document.addEventListener("paste", (e) => {
  const { roomId, threadId } = pasteTarget();
  if (!roomId) return;
  const types = [...(e.clipboardData?.items || [])].map((i) => i.type);
  const item = [...(e.clipboardData?.items || [])].find((i) => i.type.startsWith("image/"));
  if (!item) {
    showToast(
      types.length > 0
        ? `paste event has no image (types: ${types.join(", ")}) — trying clipboard API...`
        : "paste event carried no data — trying clipboard API...",
    );
    return;
  }
  pasteHandledByEvent = true;
  const file = item.getAsFile();
  if (file) loadPendingImage(file, roomId, threadId);
});

// Fallback for when the plain `paste` event's `clipboardData` doesn't
// carry image data — WebKitGTK's clipboard integration doesn't always
// populate it for images copied from screenshot tools / other apps, even
// though the image genuinely is on the clipboard. Reads the system
// clipboard from the Rust side instead (`arboard`, same approach already
// proven working in the egui version) — the webview's own
// `navigator.clipboard.read()` refuses with a permission error here with
// no way to grant it, confirmed via the toast this used to show before
// this existed.
document.addEventListener("keydown", async (e) => {
  const isPaste = (e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "v";
  const { roomId, threadId } = pasteTarget();
  if (!isPaste || !roomId) return;
  pasteHandledByEvent = false;
  // Give the plain `paste` event a moment to fire and claim it first.
  await new Promise((r) => setTimeout(r, 50));
  if (pasteHandledByEvent) return;

  try {
    const bytes = await invoke("read_clipboard_image");
    if (!bytes) {
      showToast("clipboard has no image");
      return;
    }
    const dataUrl = bytesToDataUrl(bytes, "image/png");
    state.pendingImage = { roomId, threadId, dataUrl, bytes, filename: "pasted.png", mime: "image/png" };
    renderPendingImage();
  } catch (err) {
    showToast(`clipboard read failed: ${err}`);
  }
});

// =========================================================================
// App-wide keyboard shortcuts
// =========================================================================
// Search-box-specific navigation (arrow keys / Escape) lives with the
// search box itself above; space-tab navigation lives with the space
// picker. This is only the shortcuts that make sense from anywhere:
// jumping *to* the search box, switching rooms without touching the
// mouse, and a cheat sheet to find out these exist at all.
const SHORTCUTS = [
  ["Ctrl+K or /", "focus room search"],
  ["↑ / ↓ (in search)", "move through search results"],
  ["Enter (in search)", "open the highlighted room"],
  ["Esc (in search)", "clear search, then unfocus"],
  ["← / → (in tags)", "switch space/tag"],
  ["Alt+↑ / Alt+↓", "previous / next room in the list"],
  ["Ctrl+Shift+U", "toggle unread-only filter"],
  ["Esc", "close dialog / menu"],
  ["?", "show this list"],
  ["Shift+Enter (compose)", "new line instead of sending"],
  ["Ctrl+B / Ctrl+I", "bold / italic selection"],
  ["Ctrl+E / Ctrl+Shift+E", "inline code / code block"],
  ["Ctrl+K (compose)", "insert link"],
];

function showShortcutsHelp() {
  const rows = SHORTCUTS.map(
    ([keys, desc]) => `<tr><td class="shortcut-keys">${escapeHtml(keys)}</td><td>${escapeHtml(desc)}</td></tr>`,
  ).join("");
  showDialog(`
    <h3>keyboard shortcuts</h3>
    <table class="shortcut-table">${rows}</table>
    <div class="actions"><button id="dlg-cancel">close</button></div>
  `);
  document.getElementById("dlg-cancel").addEventListener("click", closeDialog);
}
el.btnShortcuts.addEventListener("click", showShortcutsHelp);

/** True while the user is typing somewhere else — global shortcuts (`/`,
 * `?`, ...) that reuse plain, easy-to-hit keys must not fire while that's
 * happening, or every message containing "/" or "?" would get hijacked. */
function isTypingContext(target) {
  const tag = target?.tagName;
  return tag === "INPUT" || tag === "TEXTAREA" || target?.isContentEditable;
}

document.addEventListener("keydown", (e) => {
  const typing = isTypingContext(e.target);

  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
    e.preventDefault();
    el.roomFilter.focus();
    el.roomFilter.select();
    return;
  }
  if (e.key === "/" && !typing) {
    e.preventDefault();
    el.roomFilter.focus();
    return;
  }
  if (e.key === "?" && !typing) {
    e.preventDefault();
    showShortcutsHelp();
    return;
  }
  if ((e.ctrlKey || e.metaKey) && e.shiftKey && e.key.toLowerCase() === "u") {
    e.preventDefault();
    el.btnUnreadOnly.click();
    return;
  }
  if (e.altKey && (e.key === "ArrowUp" || e.key === "ArrowDown")) {
    e.preventDefault();
    if (state.visibleRoomIds.length === 0) return;
    const currentIndex = state.visibleRoomIds.indexOf(state.selectedRoom);
    const delta = e.key === "ArrowDown" ? 1 : -1;
    const base = currentIndex === -1 ? (delta === 1 ? -1 : 0) : currentIndex;
    const nextIndex = (base + delta + state.visibleRoomIds.length) % state.visibleRoomIds.length;
    selectRoom(state.visibleRoomIds[nextIndex]);
    return;
  }
  if (e.key === "Escape") {
    if (document.getElementById("active-dialog")) {
      e.preventDefault();
      closeDialog();
    } else if (el.roomMenu.style.display !== "none") {
      e.preventDefault();
      closeRoomMenu();
    }
  }
});

let toastTimer = null;
function showToast(msg) {
  console.log("[paste]", msg);
  let toast = document.getElementById("toast");
  if (!toast) {
    toast = document.createElement("div");
    toast.id = "toast";
    toast.style.cssText =
      "position:fixed;bottom:16px;left:50%;transform:translateX(-50%);background:#222;border:1px solid var(--border);color:var(--text);padding:6px 12px;font-size:12px;z-index:999;max-width:80%;";
    document.body.appendChild(toast);
  }
  toast.textContent = msg;
  toast.style.display = "block";
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (toast.style.display = "none"), 6000);
}

function loadPendingImage(file, roomId, threadId) {
  const reader = new FileReader();
  reader.onload = () => {
    const bytes = Array.from(new Uint8Array(reader.result));
    const dataUrl = bytesToDataUrl(bytes, file.type);
    state.pendingImage = {
      roomId,
      threadId,
      dataUrl,
      bytes,
      filename: file.name || "image.png",
      mime: file.type || "image/png",
    };
    renderPendingImage();
  };
  reader.readAsArrayBuffer(file);
}

function renderPendingImage() {
  const p = state.pendingImage;

  // Main compose box's preview.
  if (p && !p.threadId) {
    fillPendingImagePreview(el.pendingImagePreview, p);
  } else {
    el.pendingImagePreview.style.display = "none";
  }

  // Thread panel's preview — only relevant while that exact thread is
  // open, since its DOM (rebuilt on every `renderSidePanel`) may or may
  // not currently exist at all.
  const threadPreviewEl = document.getElementById("thread-pending-image-preview");
  if (!threadPreviewEl) return;
  if (p && p.threadId && state.rightPanel?.kind === "thread" && state.rightPanel.root.event_id === p.threadId) {
    fillPendingImagePreview(threadPreviewEl, p);
  } else {
    threadPreviewEl.style.display = "none";
  }
}

function fillPendingImagePreview(container, p) {
  container.style.display = "block";
  container.innerHTML = "";
  const img = document.createElement("img");
  img.src = p.dataUrl;
  container.appendChild(img);
  const actions = document.createElement("div");
  actions.className = "actions";
  const sendBtn = document.createElement("button");
  sendBtn.textContent = "send image";
  sendBtn.addEventListener("click", confirmPendingImage);
  const cancelBtn = document.createElement("button");
  cancelBtn.textContent = "cancel";
  cancelBtn.addEventListener("click", () => {
    state.pendingImage = null;
    renderPendingImage();
  });
  actions.appendChild(sendBtn);
  actions.appendChild(cancelBtn);
  container.appendChild(actions);
}

function confirmPendingImage() {
  const p = state.pendingImage;
  if (!p) return;
  send("SendImage", {
    room_id: p.roomId,
    thread_id: p.threadId,
    filename: p.filename,
    bytes: p.bytes,
    mime: p.mime,
    local_id: crypto.randomUUID(),
  });
  state.pendingImage = null;
  renderPendingImage();
}

// =========================================================================
// Threads
// =========================================================================

function openThread(roomId, root) {
  state.unreadThreads.delete(`${roomId}|${root.event_id}`);
  state.rightPanel = { kind: "thread", roomId, root, events: [] };
  renderSidePanel();
  send("LoadThread", { room_id: roomId, thread_root_id: root.event_id });
  // @mention autocomplete in the thread panel needs this — opening a
  // thread from the global threads popup can land here for a room other
  // than the currently selected one, which `selectRoom` never fetched it
  // for.
  if (!state.roomMembers[roomId]) send("ListMembers", { room_id: roomId });
}

el.btnMarkRead.addEventListener("click", () => {
  if (!state.selectedRoom) return;
  send("MarkRoomRead", { room_id: state.selectedRoom });
  showToast("marked as read");
});

el.btnRoomThreads.addEventListener("click", () => {
  if (!state.selectedRoom) return;
  send("ListThreads", { room_id: state.selectedRoom });
  state.rightPanel = { kind: "threads-list", scope: state.selectedRoom };
  renderSidePanel();
});

el.btnGlobalThreads.addEventListener("click", () => {
  // This view only ever shows *unread* threads (see `unreadOnly` in
  // `renderSidePanel`'s `threads-list` branch) — so a room with nothing in
  // `state.unreadThreads` contributes nothing to it no matter what
  // `ListThreads` comes back with. Used to fetch it for every room
  // regardless (3000+ requests on a large account, almost all thrown
  // away by that same filter); this only asks for the rooms that could
  // actually show up, which `state.unreadThreads` already tracks live via
  // `ThreadReply` sync events — no eager whole-account scan needed.
  const roomsWithUnreadThreads = new Set([...state.unreadThreads].map((k) => k.split("|")[0]));
  for (const roomId of roomsWithUnreadThreads) {
    send("ListThreads", { room_id: roomId });
  }
  state.rightPanel = { kind: "threads-list", scope: null };
  renderSidePanel();
});

function updateThreadsButtonBadge() {
  const anyUnread = state.unreadThreads.size > 0;
  el.btnGlobalThreads.textContent = anyUnread ? "[ threads ● ]" : "[ threads ]";
  el.btnGlobalThreads.style.color = anyUnread ? "#5ac878" : "";

  const roomUnread = state.selectedRoom
    ? [...state.unreadThreads].some((k) => k.startsWith(state.selectedRoom + "|"))
    : false;
  el.btnRoomThreads.textContent = roomUnread ? "[ threads ● ]" : "[ threads ]";
  el.btnRoomThreads.style.color = roomUnread ? "#5ac878" : "";
}

/** Appends exactly one reply to the open thread panel instead of the full
 * `renderSidePanel()` teardown-and-rebuild that used to run on every
 * single live reply — that rebuilt the compose input/mention-suggestions/
 * pending-image-preview elements too, which is why it needed the
 * hadFocus/selectionStart dance in `renderSidePanel` just to put focus
 * back where it was. Appending here never touches any of that, and only
 * follows the new reply down to the bottom if the panel was already
 * scrolled there — same "don't yank the view around" idea as the main
 * timeline's `appendMessage`. No-ops if this thread isn't the one open
 * (caller already checked, but stay defensive) or the panel isn't in the
 * DOM for some reason. */
function appendThreadMessage(rootEventId, event) {
  const rp = state.rightPanel;
  if (!rp || rp.kind !== "thread" || rp.root.event_id !== rootEventId) return;
  const msgsEl = document.getElementById("thread-messages");
  const panelBody = document.getElementById("side-panel-body");
  if (!msgsEl || !panelBody) return;

  const events = rp.events; // caller already pushed `event` onto this
  const prevReply = events.length >= 2 ? events[events.length - 2] : null;
  const threadCtx = { roomId: rp.roomId, threadId: rootEventId };
  const wasNearBottom =
    panelBody.scrollHeight - panelBody.scrollTop - panelBody.clientHeight < 120;
  msgsEl.appendChild(renderMessage(event, threadCtx, { grouped: isGrouped(prevReply, event) }));
  if (wasNearBottom) panelBody.scrollTop = panelBody.scrollHeight;
}

/** Thread-panel counterpart to `rerenderMessageInPlace` — patches one
 * message's row (root or reply) inside the open thread panel without the
 * full `renderSidePanel()` rebuild (which, for the "thread" kind, tears
 * down and recreates the compose input too). No-ops if no thread panel is
 * open or the event isn't part of it. */
/** Patches every currently-rendered row (main timeline + open thread
 * panel, if any) whose `media_url` matches `mxcUri` — shared by the
 * `ImageBytes`/`ImageFetchFailed` handlers, since both just need "redraw
 * whatever was showing a loading/failed placeholder for this image" and
 * neither cares which container(s) that turns out to be. */
function patchMessagesWithMedia(mxcUri) {
  if (state.selectedRoom) {
    for (const ev of state.timelines[state.selectedRoom] || []) {
      if (ev.media_url === mxcUri) rerenderMessageInPlace(state.selectedRoom, ev.event_id);
    }
  }
  const rp = state.rightPanel;
  if (rp?.kind === "thread") {
    if (rp.root.media_url === mxcUri) rerenderThreadMessageInPlace(rp.root.event_id);
    for (const ev of rp.events) {
      if (ev.media_url === mxcUri) rerenderThreadMessageInPlace(ev.event_id);
    }
  }
}

function rerenderThreadMessageInPlace(eventId) {
  const rp = state.rightPanel;
  if (!rp || rp.kind !== "thread") return;
  const msgsEl = document.getElementById("thread-messages");
  if (!msgsEl) return;
  const threadCtx = { roomId: rp.roomId, threadId: rp.root.event_id };
  const row = msgsEl.querySelector(`[data-event-id="${CSS.escape(eventId)}"]`);
  if (!row) return;

  if (rp.root.event_id === eventId) {
    row.replaceWith(renderMessage(rp.root, threadCtx));
    return;
  }
  const idx = rp.events.findIndex((e) => e.event_id === eventId);
  if (idx === -1) return;
  const grouped = isGrouped(idx > 0 ? rp.events[idx - 1] : null, rp.events[idx]);
  row.replaceWith(renderMessage(rp.events[idx], threadCtx, { grouped }));
}

function renderSidePanel() {
  const rp = state.rightPanel;
  const resizerRight = document.getElementById("resizer-right");
  if (!rp) {
    el.sidePanel.style.display = "none";
    el.sidePanel.innerHTML = "";
    resizerRight.style.display = "none";
    return;
  }
  el.sidePanel.style.display = "flex";
  resizerRight.style.display = "block";

  if (rp.kind === "threads-list") {
    const unreadOnly = rp.scope === null;
    let rooms = Object.entries(state.threadsByRoom)
      .filter(([roomId]) => rp.scope === null || roomId === rp.scope)
      .map(([roomId, threads]) => {
        const filtered = unreadOnly
          ? threads.filter((t) => state.unreadThreads.has(`${roomId}|${t.event_id}`))
          : threads;
        const roomName = state.rooms.find((r) => r.room_id === roomId)?.name || roomId;
        return { roomId, roomName, threads: filtered };
      })
      .filter((r) => r.threads.length > 0);
    rooms.sort((a, b) => a.roomName.localeCompare(b.roomName));

    let html = `<div id="side-panel-header"><span>${rp.scope === null ? "all threads" : "threads"}</span><button id="side-panel-close" class="small-btn">[x]</button></div><div id="side-panel-body">`;
    if (rooms.length === 0) {
      html += `<div style="color:var(--text-weak)">${unreadOnly ? "no unread threads" : "no threads yet"}</div>`;
    }
    for (const r of rooms) {
      if (rp.scope === null) html += `<div class="thread-room-name">${escapeHtml(r.roomName)}</div>`;
      for (const t of r.threads) {
        const unread = state.unreadThreads.has(`${r.roomId}|${t.event_id}`);
        html += `<div class="thread-row" data-room="${r.roomId}" data-event="${t.event_id}">
          <div class="sender" style="color:${senderColor(t.sender)}">${unread ? '<span class="unread-dot">●</span> ' : ""}${escapeHtml(t.sender_name)}</div>
          <div class="thread-row-body">${escapeHtml(truncate(t.body || "", 80))}</div>
          <div class="thread-row-meta">${t.thread_count || 0} replies →</div>
        </div>`;
      }
    }
    // Pagination only makes sense for one room's own (unfiltered) thread
    // list — the all-rooms view is already narrowed to unread threads
    // only, which `state.unreadThreads` tracks live rather than needing a
    // deeper fetch. `rooms.length === 1` here whenever `rp.scope` is a
    // room id, since the filter above already narrowed to just that room.
    const scopedRoom = rp.scope !== null ? rooms[0] : null;
    if (scopedRoom && !state.threadsListReachedEnd.has(rp.scope)) {
      const loading = state.threadsListPaginationInFlight.has(rp.scope);
      html += `<div id="threads-load-more-row" style="text-align:center;color:var(--text-weak);font-size:12px;padding:4px;">${
        loading ? "loading more threads..." : '<button id="threads-load-more-btn" class="small-btn">load more threads</button>'
      }</div>`;
    }
    html += "</div>";
    el.sidePanel.innerHTML = html;
    document.getElementById("side-panel-close").addEventListener("click", () => {
      state.rightPanel = null;
      renderSidePanel();
    });
    el.sidePanel.querySelectorAll(".thread-row").forEach((row) => {
      row.addEventListener("click", () => {
        const roomId = row.dataset.room;
        const eventId = row.dataset.event;
        const root = state.threadsByRoom[roomId]?.find((t) => t.event_id === eventId);
        if (root) openThread(roomId, root);
      });
    });
    document.getElementById("threads-load-more-btn")?.addEventListener("click", () => {
      state.threadsListPaginationInFlight.add(rp.scope);
      send("LoadMoreThreads", { room_id: rp.scope });
      renderSidePanel();
    });
    return;
  }

  if (rp.kind === "thread") {
    // A live reply/edit echoing back in re-renders this whole panel from
    // scratch (`el.sidePanel.innerHTML = ...` below), which — being a
    // fresh DOM — silently drops focus even though `sendThreadMsg`
    // explicitly put it back on the input right after sending. Remember
    // whether the compose input had focus (and where the cursor was) so
    // it can be restored once the new one exists.
    const hadFocus = document.activeElement?.id === "thread-compose-input";
    const priorSelectionStart = hadFocus ? document.activeElement.selectionStart : null;

    let html = `<div id="side-panel-header"><span>thread</span><button id="side-panel-close" class="small-btn">[x]</button></div>
      <div id="side-panel-body"><div id="thread-messages"></div></div>
      <div id="thread-reply-indicator" style="display:none;"></div>
      <div id="thread-mention-suggestions" style="display:none;"></div>
      <div id="thread-pending-image-preview" style="display:none;padding:6px 12px;"></div>
      <div id="thread-compose-row" style="border-top:1px solid var(--border);padding:8px;display:flex;gap:6px;">
        <button id="thread-btn-attach" class="small-btn">📎</button>
        <div id="thread-meme-picker-wrap">
          <button id="thread-btn-meme" class="small-btn" title="send a custom emoji/meme">🐸</button>
          <div id="thread-meme-picker" style="display:none;"></div>
        </div>
        <div class="compose-editor-wrap">
          <div class="compose-toolbar" id="thread-compose-toolbar">
            <button type="button" class="md-btn" data-md="bold" title="bold (Ctrl+B)"><b>B</b></button>
            <button type="button" class="md-btn" data-md="italic" title="italic (Ctrl+I)"><i>I</i></button>
            <button type="button" class="md-btn" data-md="code" title="inline code (Ctrl+E)">code</button>
            <button type="button" class="md-btn" data-md="codeblock" title="code block (Ctrl+Shift+E)">{ }</button>
            <button type="button" class="md-btn" data-md="link" title="link (Ctrl+K)">link</button>
          </div>
          <textarea id="thread-compose-input" class="compose-textarea" rows="1" placeholder="reply... (@ to mention, **bold**, *italic*, \`code\`)"></textarea>
        </div>
        <button id="thread-compose-send">send</button>
      </div>`;
    el.sidePanel.innerHTML = html;
    document.getElementById("side-panel-close").addEventListener("click", () => {
      state.rightPanel = null;
      renderSidePanel();
    });
    const msgsEl = document.getElementById("thread-messages");
    const threadCtx = { roomId: rp.roomId, threadId: rp.root.event_id };
    msgsEl.appendChild(renderMessage(rp.root, threadCtx));
    const hr = document.createElement("hr");
    hr.style.borderColor = "var(--border)";
    msgsEl.appendChild(hr);
    let prevReply = null;
    for (const ev of rp.events) {
      msgsEl.appendChild(renderMessage(ev, threadCtx, { grouped: isGrouped(prevReply, ev) }));
      prevReply = ev;
    }
    // `#thread-messages` itself doesn't scroll — its parent
    // `#side-panel-body` does (`overflow-y: auto`) — so that's what needs
    // scrolling to the newest reply, same as the main timeline does.
    const panelBody = document.getElementById("side-panel-body");
    panelBody.scrollTop = panelBody.scrollHeight;

    const threadSuggestionsEl = document.getElementById("thread-mention-suggestions");
    const threadInputEl = document.getElementById("thread-compose-input");
    wireMentionAutocomplete(threadInputEl, threadSuggestionsEl, () => rp.roomId, state.threadComposeMentions);
    renderPendingImage();

    const sendThreadMsg = () => {
      if (state.pendingImage && state.pendingImage.threadId === rp.root.event_id) {
        confirmPendingImage();
        return;
      }
      const input = document.getElementById("thread-compose-input");
      const body = input.value.trim();
      if (!body) return;
      input.value = "";
      autoResizeTextarea(input);
      threadSuggestionsEl.style.display = "none";
      const { ids, html } = buildMentionHtml(body, state.threadComposeMentions);
      state.threadComposeMentions = [];

      if (state.pendingEdit && state.pendingEdit.threadId === rp.root.event_id) {
        send("EditMessage", {
          room_id: rp.roomId,
          event_id: state.pendingEdit.eventId,
          body,
          mentions: ids,
          html_body: html,
        });
        cancelEdit();
        input.focus();
        return;
      }
      const replyTo =
        state.pendingReply && state.pendingReply.threadId === rp.root.event_id ? state.pendingReply.eventId : null;
      send("SendMessage", {
        room_id: rp.roomId,
        body,
        thread_id: rp.root.event_id,
        mentions: ids,
        html_body: html,
        local_id: crypto.randomUUID(),
        reply_to_event_id: replyTo,
      });
      cancelReply();
      input.focus();
    };
    document.getElementById("thread-compose-send").addEventListener("click", sendThreadMsg);
    wireComposeEditor(threadInputEl, {
      onSend: sendThreadMsg,
      isSuggestionsOpen: () => threadSuggestionsEl.style.display !== "none",
    });
    wireMarkdownToolbar(document.getElementById("thread-compose-toolbar"), threadInputEl);
    document.getElementById("thread-btn-attach").addEventListener("click", () => {
      const input = document.createElement("input");
      input.type = "file";
      input.accept = "image/*";
      input.addEventListener("change", () => {
        if (input.files[0]) loadPendingImage(input.files[0], rp.roomId, rp.root.event_id);
      });
      input.click();
    });
    const threadMemePicker = document.getElementById("thread-meme-picker");
    document.getElementById("thread-btn-meme").addEventListener("click", (e) => {
      e.stopPropagation();
      const show = threadMemePicker.style.display === "none";
      closeMemePickers();
      if (show) {
        renderMemePicker(threadMemePicker, rp.roomId, rp.root.event_id);
        threadMemePicker.style.display = "grid";
      }
    });

    if (hadFocus) {
      threadInputEl.focus();
      if (priorSelectionStart !== null) {
        threadInputEl.setSelectionRange(priorSelectionStart, priorSelectionStart);
      }
    }
    return;
  }

  if (rp.kind === "security") {
    renderSecurityPanel();
  }
}

// =========================================================================
// Security / verification
// =========================================================================

el.btnSecurity.addEventListener("click", () => {
  state.rightPanel = { kind: "security" };
  renderSidePanel();
});

function renderSecurityPanel() {
  let html = `<div id="side-panel-header"><span>security</span><button id="side-panel-close" class="small-btn">[x]</button></div>
    <div id="side-panel-body">`;
  html += `<button id="sec-verify" style="width:100%">start self-verification</button>`;
  if (state.verificationEmojis) {
    html += `<div style="margin-top:10px;">
      <div style="color:var(--text-weak);font-size:12px;">confirm these emoji match on your other device:</div>
      <div style="font-size:22px;margin:8px 0;">${state.verificationEmojis.map(([e]) => e).join(" ")}</div>
      <div style="font-size:11px;color:var(--text-weak);">${state.verificationEmojis.map(([, name]) => name).join(", ")}</div>
      <div style="display:flex;gap:8px;margin-top:8px;">
        <button id="sec-confirm">match</button>
        <button id="sec-cancel">cancel</button>
      </div>
    </div>`;
  }
  html += `<hr style="border-color:var(--border);margin:14px 0;">
    <label style="font-size:12px;color:var(--text-weak);">recovery key</label>
    <input type="text" id="sec-recovery-key" style="width:100%;margin-top:4px;" placeholder="paste recovery key" />
    <button id="sec-recover" style="width:100%;margin-top:8px;">recover</button>`;
  if (state.recoveryStatus) {
    html += `<div style="margin-top:8px;font-size:12px;color:var(--text-weak);">${escapeHtml(state.recoveryStatus)}</div>`;
  }
  html += `<hr style="border-color:var(--border);margin:14px 0;">
    <label style="font-size:12px;color:var(--text-weak);">room key export</label>
    <div style="font-size:11px;color:var(--text-weak);margin:2px 0 6px;">import a .txt file exported from Element (Settings → Security & Privacy → Export keys)</div>
    <button id="sec-import-keys" style="width:100%;">import room keys from file...</button>`;
  if (state.keyImportStatus) {
    html += `<div style="margin-top:8px;font-size:12px;color:var(--text-weak);">${escapeHtml(state.keyImportStatus)}</div>`;
  }
  html += "</div>";
  el.sidePanel.innerHTML = html;

  document.getElementById("side-panel-close").addEventListener("click", () => {
    state.rightPanel = null;
    renderSidePanel();
  });
  document.getElementById("sec-verify").addEventListener("click", () => send("StartSelfVerification"));
  document.getElementById("sec-confirm")?.addEventListener("click", () => send("ConfirmVerification"));
  document.getElementById("sec-cancel")?.addEventListener("click", () => {
    send("CancelVerification");
    state.verificationEmojis = null;
    renderSidePanel();
  });
  document.getElementById("sec-recover").addEventListener("click", () => {
    const key = document.getElementById("sec-recovery-key").value.trim();
    if (key) send("RecoverWithKey", { recovery_key: key });
  });
  document.getElementById("sec-import-keys").addEventListener("click", () => {
    el.importKeysFileInput.click();
  });
}

el.importKeysFileInput.addEventListener("change", () => {
  const file = el.importKeysFileInput.files[0];
  el.importKeysFileInput.value = "";
  if (!file) return;

  showDialog(`
    <h3>import room keys</h3>
    <div style="font-size:12px;color:var(--text-weak);margin-bottom:4px;">${escapeHtml(file.name)}</div>
    <label>passphrase</label>
    <input type="password" id="dlg-import-passphrase" placeholder="passphrase set when exporting" />
    <div class="actions">
      <button id="dlg-cancel">cancel</button>
      <button id="dlg-import">import</button>
    </div>
  `);
  document.getElementById("dlg-cancel").addEventListener("click", closeDialog);
  const submit = () => {
    const passphrase = document.getElementById("dlg-import-passphrase").value;
    if (!passphrase) return;
    const reader = new FileReader();
    reader.onload = () => {
      const bytes = Array.from(new Uint8Array(reader.result));
      send("ImportRoomKeys", { bytes, passphrase });
      state.keyImportStatus = "importing...";
      if (state.rightPanel?.kind === "security") renderSidePanel();
    };
    reader.readAsArrayBuffer(file);
    closeDialog();
  };
  document.getElementById("dlg-import").addEventListener("click", submit);
  document.getElementById("dlg-import-passphrase").addEventListener("keydown", (e) => {
    if (e.key === "Enter") submit();
  });
  document.getElementById("dlg-import-passphrase").focus();
});

// =========================================================================
// Backend events
// =========================================================================

function handleBackendEvent(evt) {
  const { type, data } = evt;
  switch (type) {
    case "SessionChecked":
      if (data) {
        enterChat();
        send("StartSync");
      }
      break;
    case "LoggedIn":
      enterChat();
      send("StartSync");
      break;
    case "LoginError":
      showLoginError(data);
      break;
    case "Rooms":
      state.rooms = data;
      renderRooms();
      break;
    case "Timeline":
      state.timelines[data.room_id] = data.events;
      state.timelineLoaded.add(data.room_id);
      if (data.room_id === state.selectedRoom) renderTimeline();
      if (state.pendingScrollTarget?.roomId === data.room_id) {
        const eventId = state.pendingScrollTarget.eventId;
        state.pendingScrollTarget = null;
        // `renderTimeline()` just ran synchronously above, but give the
        // browser a tick to actually paint before measuring for scroll.
        setTimeout(() => scrollToMessage("timeline", eventId), 50);
      }
      break;
    case "TimelinePrepend": {
      state.paginationInFlight.delete(data.room_id);
      if (data.reached_start) state.reachedStart.add(data.room_id);
      state.timelines[data.room_id] = data.events;
      if (data.room_id === state.selectedRoom) {
        const prevHeight = el.timeline.scrollHeight;
        renderTimeline();
        el.timeline.scrollTop = el.timeline.scrollHeight - prevHeight;
      }
      break;
    }
    case "NewMessage":
      if (!state.timelines[data.room_id]) state.timelines[data.room_id] = [];
      state.timelines[data.room_id].push(data.event);
      if (data.room_id === state.selectedRoom) {
        appendMessage(data.room_id, data.event);
        // A live message landing in the room you're actively looking at
        // shouldn't make it show up as unread in the sidebar a moment
        // later — keep it marked read as messages arrive, not just once
        // when the room is first opened.
        send("MarkRoomRead", { room_id: data.room_id });
      }
      maybeNotify(data.room_id, data.event);
      break;
    case "ThreadReply": {
      const key = `${data.room_id}|${data.thread_root_id}`;
      const openHere =
        state.rightPanel &&
        state.rightPanel.kind === "thread" &&
        state.rightPanel.root.event_id === data.thread_root_id;
      if (openHere) {
        state.rightPanel.events.push(data.event);
        state.rightPanel.root.thread_count = (state.rightPanel.root.thread_count || 0) + 1;
        appendThreadMessage(data.thread_root_id, data.event);
      } else {
        state.unreadThreads.add(key);
        updateThreadsButtonBadge();
      }
      const bump = (list) => {
        const t = list?.find((e) => e.event_id === data.thread_root_id);
        if (t) t.thread_count = (t.thread_count || 0) + 1;
      };
      bump(state.threadsByRoom[data.room_id]);
      bump(state.timelines[data.room_id]);
      // The "🧵 N replies →" badge on the thread root as shown in the
      // *main* timeline (not the thread panel — that suppresses its own
      // copy of the badge on the root) — patch just that one row instead
      // of a full `renderTimeline()`.
      rerenderMessageInPlace(data.room_id, data.thread_root_id);
      // `openHere` (declared above) covers "thread panel already showing
      // this exact thread" the same way `maybeNotify` covers "room already
      // open and focused" for `NewMessage` — skip the notification in
      // either case, it's already right there on screen.
      if (!openHere) maybeNotify(data.room_id, data.event, data.thread_root_id);
      break;
    }
    case "ThreadEvents":
      if (
        state.rightPanel &&
        state.rightPanel.kind === "thread" &&
        state.rightPanel.root.event_id === data.thread_root_id
      ) {
        state.rightPanel.events = data.events;
        renderSidePanel();
      }
      break;
    case "ThreadsList":
      // A fresh first page — replaces whatever was there before, same as
      // reopening the panel from scratch (e.g. re-clicking "[ threads ]").
      state.threadsByRoom[data.room_id] = data.threads;
      if (data.reached_end) state.threadsListReachedEnd.add(data.room_id);
      else state.threadsListReachedEnd.delete(data.room_id);
      state.threadsListPaginationInFlight.delete(data.room_id);
      if (state.rightPanel && state.rightPanel.kind === "threads-list") renderSidePanel();
      break;
    case "ThreadsListAppend": {
      const existing = state.threadsByRoom[data.room_id] || [];
      // Threads already known (e.g. one that got a live reply and moved
      // itself into `threadsByRoom` via `ThreadReply` before this page
      // loaded) shouldn't show up twice.
      const existingIds = new Set(existing.map((t) => t.event_id));
      const fresh = data.threads.filter((t) => !existingIds.has(t.event_id));
      state.threadsByRoom[data.room_id] = [...existing, ...fresh];
      if (data.reached_end) state.threadsListReachedEnd.add(data.room_id);
      state.threadsListPaginationInFlight.delete(data.room_id);
      if (state.rightPanel && state.rightPanel.kind === "threads-list") renderSidePanel();
      break;
    }
    case "Members":
      state.roomMembers[data.room_id] = data.members;
      break;
    case "Summary":
      state.summaries[data.room_id] = data.text;
      state.summarizing = false;
      el.btnSummarize.textContent = "[ summarize ]";
      el.btnSummarize.disabled = false;
      if (data.room_id === state.selectedRoom) renderSummary();
      break;
    case "MessageDeleted": {
      const ev = findEvent(data.room_id, data.event_id);
      if (ev) {
        ev.msg_type = "deleted";
        ev.body = "";
        rerenderMessageInPlace(data.room_id, data.event_id);
        rerenderThreadMessageInPlace(data.event_id);
      }
      break;
    }
    case "MessageEdited": {
      const ev = findEvent(data.room_id, data.event_id);
      if (ev) {
        ev.body = data.new_body;
        rerenderMessageInPlace(data.room_id, data.event_id);
        rerenderThreadMessageInPlace(data.event_id);
      }
      break;
    }
    case "MessageSent":
    case "MessageSendFailed":
      if (type === "MessageSendFailed") console.error("send failed:", data.error);
      break;
    case "ImageBytes": {
      const declaredMime = state.imageMime[data.key];
      const sniffed = declaredMime || sniffImageMime(new Uint8Array(data.bytes));
      if (sniffed && UNVIEWABLE_IMAGE_MIMES.has(sniffed)) {
        // No point keeping these bytes around or handing them to an
        // `<img>` — sniffed only after the download because the sender
        // didn't set `content.info.mimetype` (declared-mime case never
        // reaches here at all, see the render-time check).
        state.imageUnviewable[data.key] = sniffed;
      } else {
        state.imageCache[data.key] = bytesToDataUrl(data.bytes, declaredMime);
      }
      delete imageFetchAttempts[data.key];
      // One image can back several rendered rows — the main timeline,
      // the thread panel below if the same message is a thread root/
      // reply, or both — patch each in place. A full `renderTimeline()`/
      // `renderSidePanel()` here used to mean every image finishing its
      // download jumped the whole view back to the bottom (and, for the
      // thread panel specifically, dropped focus out of the compose
      // input), which for a room with several images in flight felt like
      // it never stopped jumping.
      patchMessagesWithMedia(data.key);
      // A meme-picker thumbnail finishing its download — re-render
      // whichever picker is currently open (if any) so it swaps from
      // blank to the actual image instead of staying empty until the
      // next unrelated re-render.
      if (el.memePicker.style.display !== "none") {
        renderMemePicker(el.memePicker, state.selectedRoom, null);
      }
      const openThreadMemePicker = document.getElementById("thread-meme-picker");
      if (openThreadMemePicker && openThreadMemePicker.style.display !== "none" && state.rightPanel?.kind === "thread") {
        renderMemePicker(openThreadMemePicker, state.rightPanel.roomId, state.rightPanel.root.event_id);
      }
      break;
    }
    case "ImageFetchFailed": {
      // Without this, a single transient failure (network blip, a
      // moment's auth hiccup) left that image permanently broken —
      // `imageRequested` marks it "already asked for" forever, so no
      // future re-render ever asks again. Retry a few times with a short
      // backoff, then give up and leave `imageRequested` set so it stops
      // hammering a server-side failure (deleted media, real 404, ...).
      console.warn("image fetch failed:", data.key, data.error);
      const attempts = (imageFetchAttempts[data.key] || 0) + 1;
      imageFetchAttempts[data.key] = attempts;
      state.imageRequested.delete(data.key);
      if (attempts <= 3) {
        setTimeout(() => requestImage(data.key), attempts * 1500);
      }
      patchMessagesWithMedia(data.key);
      break;
    }
    case "MediaDownloaded":
      showToast(`saved to ${data.path}`);
      break;
    case "SpaceChildren":
      state.spaceChildren[data.space_room_id] = data.room_ids;
      renderRooms();
      break;
    case "NotificationMode":
      state.notificationModes[data.room_id] = data.mode;
      if (data.room_id === state.selectedRoom) updateNotificationModeUi();
      break;
    case "NotificationClicked":
      selectRoom(data.room_id);
      window.focus?.();
      if (data.thread_id) {
        // The root event may not be loaded locally yet (e.g. the app was
        // fully backgrounded when the reply came in) — a minimal stub is
        // enough to open the panel with, `openThread` itself fetches the
        // real thread content via `LoadThread` right after.
        const root = findEvent(data.room_id, data.thread_id) || {
          event_id: data.thread_id,
          sender_name: "",
          body: "",
          timestamp: Date.now(),
        };
        openThread(data.room_id, root);
      }
      break;
    case "Reactions": {
      const ev = findEvent(data.room_id, data.event_id);
      if (ev) {
        ev.reactions = data.reactions;
        rerenderMessageInPlace(data.room_id, data.event_id);
        rerenderThreadMessageInPlace(data.event_id);
      }
      break;
    }
    case "ImagePacks":
      state.imagePacks[data.room_id] = data.images;
      // Re-render whichever picker is open for this room — e.g. right
      // after `Command::AddImagePackEmoji` answers with the fresh list,
      // so the emoji you just added shows up without reopening anything.
      if (el.memePicker.style.display !== "none" && data.room_id === state.selectedRoom) {
        renderMemePicker(el.memePicker, data.room_id, null);
      }
      {
        const openThreadMemePicker = document.getElementById("thread-meme-picker");
        if (
          openThreadMemePicker &&
          openThreadMemePicker.style.display !== "none" &&
          state.rightPanel?.kind === "thread" &&
          state.rightPanel.roomId === data.room_id
        ) {
          renderMemePicker(openThreadMemePicker, data.room_id, state.rightPanel.root.event_id);
        }
      }
      break;
    case "VerificationEmojis":
      state.verificationEmojis = data;
      if (state.rightPanel?.kind === "security") renderSidePanel();
      break;
    case "VerificationDone":
      state.verificationEmojis = null;
      state.recoveryStatus = "verification complete";
      if (state.rightPanel?.kind === "security") renderSidePanel();
      break;
    case "VerificationCancelled":
      state.verificationEmojis = null;
      state.recoveryStatus = "verification cancelled: " + data;
      if (state.rightPanel?.kind === "security") renderSidePanel();
      break;
    case "RecoveryStatus":
      state.recoveryStatus = data;
      if (state.rightPanel?.kind === "security") renderSidePanel();
      break;
    case "RoomKeysImported":
      state.keyImportStatus = `imported ${data.imported} of ${data.total} session${data.total === 1 ? "" : "s"}`;
      showToast(state.keyImportStatus);
      if (state.rightPanel?.kind === "security") renderSidePanel();
      break;
    case "Error":
      console.error("backend error:", data);
      showToast(data);
      break;
    default:
      break;
  }
}

async function pollEvents() {
  try {
    const events = await invoke("poll_events");
    for (const evt of events) handleBackendEvent(evt);
  } catch (err) {
    console.error("poll_events failed:", err);
  }
}
setInterval(pollEvents, 150);

// =========================================================================
// Resizable panels
// =========================================================================

function setupResizer(handle, panel, { fromRight } = {}) {
  handle.addEventListener("mousedown", (e) => {
    e.preventDefault();
    const startX = e.clientX;
    const startWidth = panel.getBoundingClientRect().width;
    handle.classList.add("dragging");
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";

    function onMove(ev) {
      const delta = fromRight ? startX - ev.clientX : ev.clientX - startX;
      const min = parseInt(getComputedStyle(panel).minWidth, 10) || 120;
      panel.style.width = Math.max(min, startWidth + delta) + "px";
    }
    function onUp() {
      handle.classList.remove("dragging");
      document.body.style.cursor = "";
      document.body.style.userSelect = "";
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    }
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  });
}

setupResizer(document.getElementById("resizer-left"), document.getElementById("room-list"));
setupResizer(document.getElementById("resizer-right"), el.sidePanel, { fromRight: true });

// =========================================================================
// Desktop notifications
// =========================================================================
// Sent via `Command::ShowNotification`, handled in the Rust worker with
// `notify-rust` directly — NOT the web `Notification` API / Tauri's
// notification plugin. That plugin's Linux `show()` panicked ("Cannot
// start a runtime from within a runtime") once the tray icon's `ksni`
// dependency pulled in zbus's "tokio" feature; see the `Command` enum's
// doc comment on `ShowNotification` for the full story. Clicking the
// notification round-trips back here as an `Event::NotificationClicked`
// (handled in `handleBackendEvent`) instead of a DOM `onclick`.

/** Mirrors the egui version's `maybe_notify`: skip your own messages, skip
 * the room you're already looking at (selected *and* the window focused —
 * switching away to another app should still notify), and respect the
 * room's notification mode once known (defaults to "all" for a room whose
 * mode hasn't been fetched yet, same as the room list's own default). */
function maybeNotify(roomId, event, threadId = null) {
  if (event.is_own) return;
  if (roomId === state.selectedRoom && document.hasFocus()) return;

  const mode = state.notificationModes[roomId] || "all";
  if (mode === "mute") return;
  if (mode === "mentions" && !event.mentions_me) return;

  const room = state.rooms.find((r) => r.room_id === roomId);
  const title = room ? `${event.sender_name} (${room.name})` : event.sender_name;
  const body =
    event.msg_type === "image" ? "sent an image" : truncate(event.body || "", 120);
  send("ShowNotification", { room_id: roomId, thread_id: threadId, title, body });
}

/** Jumps to and briefly highlights the message a reply-preview points at.
 * `containerId` is `"timeline"` for the main view or `"thread-messages"`
 * for the thread panel — whichever one this reply-preview was rendered
 * in, since the target can only possibly be loaded there. Silently no-ops
 * (well, a toast) if it isn't currently loaded — e.g. it's further back
 * than "load more messages" has fetched yet. */
function scrollToMessage(containerId, eventId) {
  const container = document.getElementById(containerId);
  const target = container?.querySelector(`[data-event-id="${CSS.escape(eventId)}"]`);
  if (!target) {
    showToast("original message isn't loaded — try loading more history");
    return;
  }
  target.scrollIntoView({ behavior: "smooth", block: "center" });
  target.classList.add("jump-highlight");
  setTimeout(() => target.classList.remove("jump-highlight"), 1600);
}

// =========================================================================
// Share message (matrix.to links)
// =========================================================================

function buildMatrixToLink(roomId, eventId) {
  // Room/event IDs (`!xyz:server`, `$xyz`) don't contain characters that
  // need percent-encoding, and matrix.to links are conventionally shown
  // "clean" (`:` literal, not `%3A`) — encoding them would still work when
  // clicked here (the handler below decodes), but would look broken
  // pasted anywhere that doesn't bother decoding first.
  return `https://matrix.to/#/${roomId}/${eventId}`;
}

async function shareMessage(roomId, eventId) {
  const link = buildMatrixToLink(roomId, eventId);
  try {
    await navigator.clipboard.writeText(link);
    showToast("message link copied — paste it to share");
    return;
  } catch {
    // Fall through to the manual-selection fallback below — plain text
    // clipboard *writes* are usually allowed even where the image *read*
    // API was flatly refused (see `read_clipboard_image`'s doc comment),
    // but cover the case where it isn't either.
  }
  const ta = document.createElement("textarea");
  ta.value = link;
  ta.style.cssText = "position:fixed;opacity:0;";
  document.body.appendChild(ta);
  ta.select();
  const ok = document.execCommand("copy");
  ta.remove();
  showToast(ok ? "message link copied — paste it to share" : `couldn't copy — link: ${link}`);
}

/** Recognizes our own shared links (and anyone else's matrix.to links with
 * the same room-then-event shape) in message bodies and jumps to the
 * target instead of trying to open a browser — switching rooms first if
 * the link points somewhere other than the one currently open. */
document.addEventListener("click", (e) => {
  const a = e.target.closest("a[href]");
  if (!a) return;
  const m = a.getAttribute("href")?.match(/^https:\/\/matrix\.to\/#\/(![^/?]+)\/(\$[^/?]+)/);
  if (!m) return;
  e.preventDefault();
  openMatrixToLink(decodeURIComponent(m[1]), decodeURIComponent(m[2]));
});

function openMatrixToLink(roomId, eventId) {
  if (!state.rooms.some((r) => r.room_id === roomId)) {
    showToast("that room isn't in your room list");
    return;
  }
  // `selectRoom` only fires a fresh `LoadTimeline` (and the `Timeline`
  // event that `pendingScrollTarget` waits on) the *first* time a room is
  // opened — for a room already visited this session, it just re-renders
  // from the cached timeline synchronously, so no `Timeline` event ever
  // arrives to trigger the scroll. Decide up front which case this is:
  // already-loaded scrolls right away, not-yet-loaded queues it for when
  // the load finishes.
  const alreadyLoaded = state.timelineLoaded.has(roomId);
  if (state.selectedRoom !== roomId) {
    selectRoom(roomId);
  }
  if (alreadyLoaded) {
    setTimeout(() => scrollToMessage("timeline", eventId), 50);
  } else {
    state.pendingScrollTarget = { roomId, eventId };
  }
}

// ---- Boot ----
// No `send("CheckSession")` here anymore — the Rust worker now runs that
// check itself the instant it starts (see `matrix/worker.rs::run`), and
// whatever it finds reaches this page through the normal `poll_events`
// loop regardless of whether this script's own boot code just ran. It
// has to work that way: on Android, this script re-running at all isn't
// guaranteed on every app open (confirmed on a real device — the native
// host process restarting doesn't reliably mean the WebView's page came
// back up fresh with it), so a boot-time `send` from here could simply
// never fire, leaving a perfectly valid stored session unchecked.
