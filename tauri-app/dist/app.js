const { invoke } = window.__TAURI__.core;

/** Whether the phone-width layout (style.css's `(max-width: 720px),
 * (max-height: 500px), (hover: none) and (pointer: coarse)` media query)
 * is currently active — kept as one function so every JS call site
 * agrees with each other and with that stylesheet rule on what counts as
 * "narrow," rather than three separately-typed copies of the same query
 * string quietly drifting apart. The touch clause is what actually makes
 * this phone-*shaped* rather than just phone-*sized*: a real touchscreen
 * phone can report a CSS viewport width well outside the plain
 * width/height checks (a ~360–430px assumption a desktop browser's
 * DevTools device emulator trains you to expect doesn't hold for every
 * device), so checking the *input* type too is what actually catches it
 * reliably. See the media query's own comment in style.css for the full
 * reasoning. */
function isNarrowLayout() {
  return window.matchMedia("(max-width: 720px), (max-height: 500px), (hover: none) and (pointer: coarse)").matches;
}

/** Keeps `--app-height` (used by `html, body` in style.css) in sync with
 * `window.visualViewport` — the piece of viewport actually visible right
 * now, as opposed to the *layout* viewport `100%`/`100vh` measure, which
 * on Android's WebView stays full-height even once the on-screen keyboard
 * has covered the bottom of the screen. Without this, opening the
 * keyboard while composing a message pushed the send/attach/markdown
 * buttons (all anchored to the bottom of that never-shrinking 100%-tall
 * column) out from under the visible area — still technically on screen,
 * just physically behind the keyboard. `visualViewport` is undefined on
 * very old WebViews; this is simply a no-op there; on desktop there's no
 * on-screen keyboard to react to and `resize` essentially never fires
 * from this cause. */
if (window.visualViewport) {
  const applyVisualViewportHeight = () => {
    document.documentElement.style.setProperty("--app-height", `${window.visualViewport.height}px`);
  };
  window.visualViewport.addEventListener("resize", applyVisualViewportHeight);
  applyVisualViewportHeight();
}

/** Sends a `Command` — `data` omitted entirely for unit variants (no
 * fields), matching serde's adjacently-tagged representation. */
function send(type, data) {
  const cmd = data === undefined ? { type } : { type, data };
  return invoke("send_command", { cmd }).catch((err) => {
    console.error("send_command failed:", type, err);
  });
}

// key ("room:"+room_id / "invite:"+room_id / "empty-placeholder") -> the
// live DOM node `renderRooms()` last rendered for it. Kept outside `state`
// (not serializable/relevant app data, just a render cache) so
// `renderRooms()` can reuse and update existing rows in place across
// re-renders instead of wiping and rebuilding the whole list every time —
// that full rebuild was the "nhảy nhảy" (jumpy) flicker on every badge
// update, since content-visibility:auto rows getting destroyed/recreated
// forces a reflow of the whole visible list even when a single unread
// count changed and nothing actually needs to move.
const roomRowEls = new Map();

// The backend already paces syncing thousands of rooms in (see
// `scheduleGrowRoomList` below), but once they're in `state.rooms`,
// `renderRooms()` used to still mount a `.room-row` DOM node for every one
// of them — content-visibility:auto skips layout/paint for the offscreen
// ones, but building/reordering/measuring (`animateRoomListChanges`) 3000+
// live nodes on every render is still real, synchronous work, and it's what
// made the list janky on accounts with thousands of rooms (worst on
// mobile). Past this threshold, `renderRooms()` switches to only mounting
// the rows actually scrolled into view (+ overscan), same idea as any
// virtual-scrolling list. Below it, the simpler always-fully-mounted path
// (with its nicer FLIP reorder animation) stays in effect — not worth the
// extra bookkeeping for the common case of a normal-sized account.
const ROOM_LIST_VIRTUALIZE_THRESHOLD = 150;
// Rendered just outside the visible viewport so a fast scroll or a
// PageUp/PageDown jump doesn't flash empty space for a frame before the
// next window recalculation catches up.
const ROOM_LIST_OVERSCAN = 8;
// Fallback until a real row is on screen to measure — kept in sync with
// `.room-row`'s `contain-intrinsic-size` in style.css, itself just an
// estimate, so slight drift here only costs a slightly-off scrollbar for
// one frame, never a layout bug.
const ROOM_LIST_DEFAULT_ROW_HEIGHT = 37;
let roomListMeasuredRowHeight = null;
// Set by `renderRooms()` when it's in the virtualized branch (the full
// ordered list of *matching* rooms, independent of scroll position); read
// by `renderRoomListWindow()` on every scroll tick to remount just the
// visible slice without re-running the filter/sort pass. `null` whenever
// the list isn't currently virtualized (small account, or the invites tab,
// which is never large enough to bother).
let virtualRoomList = null;
let roomListWindowRafPending = false;
// The two flow-layout spacers that stand in for the rows currently
// scrolled out of view, so `#room-list-items`'s natural scroll height
// still matches "all N rows stacked" even though only a slice of them are
// actually mounted. Created lazily on first use.
let roomListTopSpacer = null;
let roomListBottomSpacer = null;

const state = {
  screen: "login",
  loggingIn: false,
  // `rooms` stays a flat array (same shape as before this migration) so
  // every existing `state.rooms.find/.some/.sort` call site keeps working
  // unchanged — it's derived (see `rebuildRoomsFromEntries`) from the two
  // index-mirrored arrays below, which are the actual source of truth for
  // *position* (backend diff ops address a slot by index, within one list).
  // Both are client-side-paginated views over the backend's growing room
  // list (see `Command::GrowRoomList`) — every slot is always a real,
  // already-loaded room; there's no "not synced yet" placeholder concept
  // to represent here (unlike an earlier draft of this protocol).
  roomEntries: [],
  inviteEntries: [],
  rooms: [],
  // Space rooms, from the separate `Event::Spaces` snapshot — `m.space`
  // rooms are excluded from the backend's main sliding-sync lists, so they
  // never appear in `roomEntries`/`rooms` at all.
  spaces: [],
  selectedRoom: null,
  selectedSpace: null,
  spaceChildren: {}, // space_room_id -> [room_id]
  roomFilter: "",
  unreadOnly: false,
  // Mirrors `unreadOnly`, but for `[ @mentions ]` — filters the room list
  // down to rooms with an unread message that pings this account
  // specifically (`room.mention_count > 0`), for "which rooms have
  // something I was actually tagged in and haven't seen yet".
  mentionsOnly: false,
  reloadingRooms: false,
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
  // Every joined member across every joined room, deduped by user id — for
  // the "messages from a user" picker. `null` until `Command::ListAllUsers`
  // has answered once (fetched lazily, the first time that dialog opens,
  // then cached for the rest of the session).
  allUsers: null,
  allUsersLoading: false,
  // Live homeserver user-directory search results for the "invite to room"
  // dialog — `{ query, users }` from the most recent `Event::DirectoryUsers`
  // (`query` lets a stale, slow response be dropped if the input has since
  // changed), or `null` before the first search answers.
  directorySearch: null,
  notificationModes: {}, // room_id -> mode
  // `{ roomId, threadRootId }` for whichever `Command::Summarize` the
  // currently-open summary dialog (if any) is waiting on — `Event::Summary`
  // only updates that dialog's contents when both match, so a stray
  // answer for a request the dialog's since moved past can't clobber it.
  summaryRequest: null,

  pendingReply: null, // { roomId, threadId, eventId, preview }
  pendingEdit: null, // { roomId, threadId, eventId }
  pendingImage: null, // { roomId, threadId, dataUrl, bytes, filename, mime }
  pendingFile: null, // { roomId, threadId, bytes, filename, mime, size } — any file, no dataUrl/thumbnail
  composeMentions: [], // [{userId, displayName}] selected via autocomplete
  threadComposeMentions: [], // same, for the thread panel's compose box
  sending: false,
  // local_id -> { roomId, threadId } for every `Command::SendMessage` this
  // session has fired but not yet resolved — tracks the optimistic
  // "sending…" bubble `sendCurrentMessage`/thread-compose render straight
  // into the timeline the moment Send is pressed, so `MessageSent`/
  // `MessageSendFailed` (and the live echo carrying the same `local_id`
  // back on `event.local_id`, see `TimelineEvent` on the Rust side) know
  // which placeholder row to reconcile without needing the room/thread
  // context re-derived from scratch.
  pendingSends: new Map(),

  rightPanel: null, // {kind:'threads-list', scope: roomId|null} | {kind:'thread', roomId, root, events} | {kind:'security'}
  threadsByRoom: {}, // room_id -> [TimelineEvent] (thread roots)
  // room_id -> true once `Command::ListThreads`/`LoadMoreThreads` has
  // reached that room's oldest thread — stops issuing further
  // `LoadMoreThreads` requests for it, and hides its "load more" row.
  threadsListReachedEnd: new Set(),
  threadsListPaginationInFlight: new Set(),
  unreadThreads: new Set(), // "room_id|thread_root_id"
  // Threads known to have an unread reply that pings this account
  // specifically — same "root_id|thread_root_id" keying and same
  // session-local-heuristic caveat as `unreadThreads` (see its own doc
  // comment): a thread root's own `mentions_me` covers the case where the
  // *first* message tagged you, and this Set catches a live reply doing
  // so later in the session (see the `ThreadReply` handler) — there's no
  // deeper per-reply mention history to fetch beyond what's already
  // loaded, same limitation `unreadThreads` already has.
  mentionThreads: new Set(),
  threadCompose: { sending: false },
  threadPaginationReachedStart: new Set(),
  // Keyboard roving-highlight index into the threads-list panel's
  // currently rendered rows (`state.visibleThreadRows`) — same idea as
  // `roomListActiveIndex`/`visibleRoomIds` for the room list. -1 = none
  // highlighted.
  threadsListActiveIndex: -1,
  // `{roomId, eventId}` pairs in the order `renderSidePanel()`'s
  // "threads-list" branch actually rendered them — what Up/Down/Enter
  // navigate over, kept in sync with the DOM the same way
  // `visibleRoomIds` is for the room list.
  visibleThreadRows: [],
  // Search text for the threads-list panel's own filter box — matched
  // (diacritic/case-insensitive, same as the room filter) against the
  // room name, the thread's first message, and its latest reply.
  // Cleared whenever the panel is (re)opened, not kept across sessions.
  threadsListFilter: "",
  // Mirrors `unreadOnly` for the room list (`[ unread ]` there) — the
  // "all threads" ("+" scope: null) view used to hardcode this to `true`
  // with no way to see anything else, which combined badly with "unread"
  // for a thread meaning, specifically, "a reply arrived over live sync
  // *this session*" (there's no real per-thread read-receipt tracking —
  // see `scanAllRoomThreads`'s comment): right after launch, before
  // anything's arrived live yet, that made the panel look empty even
  // with plenty of genuinely-unread threads sitting in already-loaded
  // room data. Defaults to off, same as the room list's own toggle.
  threadsListUnreadOnly: false,
  // Mirrors `mentionsOnly` for the room list's "[ @mentions ]" — filters
  // the threads-list panel down to unread threads that ping this account
  // (see `isThreadMentioningMe`).
  threadsListMentionsOnly: false,
  // Set once `scanAllRoomThreads` has fired, so it only ever runs once
  // per app launch (see the `Rooms` event handler).
  threadsScanStarted: false,

  verificationEmojis: null,
  recoveryStatus: null,
  recoveryKeyInput: "",
  // Whether an LVX API key is currently set (security panel's own field —
  // never the actual key value, see `Event::LvxApiKeyStatus`). `null`
  // until the panel's asked and gotten an answer.
  lvxApiKeyConfigured: null,
  // The logged-in user's own profile — `{user_id, display_name, avatar_url}`
  // — `null` until the profile panel's asked and gotten an `Event::OwnProfile`
  // answer. Also `"saving"` briefly while a `SetDisplayName`/`SetAvatar`
  // round trip is in flight, so the panel can show a spinner instead of
  // silently doing nothing for however long the upload takes.
  ownProfile: null,
  ownProfileSaving: false,
  // Set right before switching rooms to follow a matrix.to link — once
  // that room's `Timeline` event lands, scroll to this event and clear it.
  pendingScrollTarget: null, // { roomId, eventId }
  // Set by `findAndScrollToMessage` while it's auto-paginating a room
  // backwards looking for a matrix.to link's target message that wasn't
  // in whatever was already loaded — checked again each `TimelinePrepend`
  // for that room until the message turns up or history runs out.
  pendingScrollSearch: null, // { roomId, eventId }
  // Set by a matrix.to link carrying our `?thread=` extension (see
  // `buildMatrixToLink`) while its target room's main timeline is still
  // being paginated back looking for the *thread's root* message — the
  // root is a normal timeline event, and its data (needed to even open
  // the thread panel — see `openThread`) has to be found there before
  // anything thread-specific can happen. Resumed from `TimelinePrepend`
  // the same way `pendingScrollSearch` is.
  pendingThreadLink: null, // { roomId, threadRootId, eventId }
  // Set once the thread panel from a `?thread=` link is open and it's
  // waiting for its (already-in-progress — see `ThreadEvents`) full
  // auto-load of the thread to bring in the specific reply being linked
  // to. Resumed from `ThreadEvents`/`ThreadEventsPrepend`.
  pendingThreadScrollTarget: null, // { roomId, threadRootId, eventId }
  // Set while waiting on `Command::ResolveSharedEvent` for a plain (no
  // `?thread=` hint) matrix.to link, to find out whether its target is
  // actually a thread reply before deciding where to look for it — see
  // `openMatrixToLink`. Cleared by the matching `SharedEventResolved`.
  pendingSharedEventResolve: null, // { roomId, eventId }

  // room_id -> [user_id] currently typing (excludes ourselves — the
  // backend already filters that out). Only ever populated for whichever
  // one room `Command::WatchTyping` last subscribed to (see `selectRoom`).
  typingUsers: {},
  // room_id -> `RoomInfo` (name/topic/avatar_url + can_set_* flags) — see
  // `Command::GetRoomInfo`. `null`/absent until the room settings dialog
  // (or the pinned-banner/room-header code checking `can_set_*`) has asked
  // at least once.
  roomInfo: {},
  // room_id -> [event_id] currently pinned, most-recently-pinned last (the
  // server's own `m.room.pinned_events` order). Absent until
  // `Command::GetPinnedEvents` has answered for that room at least once.
  pinnedEvents: {},
  // user_id -> `PresenceInfo` (see `Command::GetPresence`) — a session-wide
  // cache, not scoped to one room, since the same person can show up in
  // several rooms' member lists.
  presence: {},
  // "roomId|eventId" -> `TimelineEvent` | `null` (fetched, not found/
  // unparseable) — see `getEventPreview`/`Command::GetEventPreview`. Absent
  // entirely means never requested yet.
  eventPreviews: {},
  // poll_event_id -> `PollData` — every poll this session has seen, either
  // from `Command::StartPoll`/`VotePoll`/`EndPoll`'s own answer, a live
  // vote/end update, or `Command::ListPolls`.
  polls: {},
  // Whether the polls-list dialog is currently open for this room, so a
  // live `Event::PollUpdated` while it's open can re-render it in place
  // instead of only updating `state.polls` silently until it's reopened.
  pollsDialogRoomId: null,
  // Same idea as `pollsDialogRoomId`, for the pinned-messages dialog — a
  // pinned image's `Event::ImageBytes` (or a still-loading preview's
  // `Event::EventPreview`) finishing while it's open re-renders it in
  // place instead of leaving a stale placeholder up until it's reopened.
  pinnedEventsDialogRoomId: null,
  // `null` (follow the system/GTK theme, this app's original behavior) |
  // "light" | "dark" — the "[ theme: ... ]" button in the chats menu,
  // persisted to `localStorage` so it survives a restart. See
  // `applyThemeOverride()`.
  themeOverride: null,
  // `null` (default, `--font-family`'s CSS value) | one of `FONT_FAMILY_OPTIONS`'s
  // `value`s — the "[ font ]" dialog's font-family choice, persisted to
  // `localStorage`. See `applyFontSettings()`.
  fontFamily: null,
  // `null` (default, `--font-size`'s CSS value) | a number of px — same
  // dialog's font-size choice, persisted to `localStorage`.
  fontSize: null,
  // `MediaRecorder` instance + captured chunks/analyser while a voice
  // message is being recorded — `null` when not recording. See
  // `toggleVoiceRecording()`.
  voiceRecording: null,
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
  btnMentionsOnly: document.getElementById("btn-mentions-only"),
  roomListItems: document.getElementById("room-list-items"),
  btnProfile: document.getElementById("btn-profile"),
  btnSecurity: document.getElementById("btn-security"),
  btnCreateRoom: document.getElementById("btn-create-room"),
  btnReloadRooms: document.getElementById("btn-reload-rooms"),
  btnGlobalThreads: document.getElementById("btn-global-threads"),
  btnUserSearch: document.getElementById("btn-user-search"),
  btnMessageSearch: document.getElementById("btn-message-search"),
  btnTheme: document.getElementById("btn-theme"),
  btnFontSettings: document.getElementById("btn-font-settings"),
  btnShortcuts: document.getElementById("btn-shortcuts"),
  btnLogout: document.getElementById("btn-logout"),
  btnChatsMenu: document.getElementById("btn-chats-menu"),
  chatsMenu: document.getElementById("chats-menu"),
  btnBackToRooms: document.getElementById("btn-back-to-rooms"),
  timelineTitle: document.getElementById("timeline-title"),
  timelineHeaderActions: document.getElementById("timeline-header-actions"),
  btnMarkRead: document.getElementById("btn-mark-read"),
  btnRoomThreads: document.getElementById("btn-room-threads"),
  btnRoomMenu: document.getElementById("btn-room-menu"),
  roomMenu: document.getElementById("room-menu"),
  btnFavoriteRoom: document.getElementById("btn-favorite-room"),
  btnInvite: document.getElementById("btn-invite"),
  btnLeaveRoom: document.getElementById("btn-leave-room"),
  notificationMode: document.getElementById("notification-mode"),
  btnRoomSettings: document.getElementById("btn-room-settings"),
  btnPins: document.getElementById("btn-pins"),
  btnPolls: document.getElementById("btn-polls"),
  btnSummarize: document.getElementById("btn-summarize"),
  pinnedBanner: document.getElementById("pinned-banner"),
  timeline: document.getElementById("timeline"),
  btnJumpLatest: document.getElementById("btn-jump-latest"),
  typingIndicator: document.getElementById("typing-indicator"),
  replyIndicator: document.getElementById("reply-indicator"),
  editIndicator: document.getElementById("edit-indicator"),
  mentionSuggestions: document.getElementById("mention-suggestions"),
  pendingImagePreview: document.getElementById("pending-image-preview"),
  pendingFilePreview: document.getElementById("pending-file-preview"),
  genericFileInput: document.getElementById("generic-file-input"),
  composeRow: document.getElementById("compose-row"),
  composeInput: document.getElementById("compose-input"),
  composeToolbar: document.getElementById("compose-toolbar"),
  composeSend: document.getElementById("compose-send"),
  btnComposePlus: document.getElementById("btn-compose-plus"),
  memePicker: document.getElementById("meme-picker"),
  fileInput: document.getElementById("file-input"),
  importKeysFileInput: document.getElementById("import-keys-file-input"),
  avatarFileInput: document.getElementById("avatar-file-input"),
  sidePanel: document.getElementById("side-panel"),
  mainPanel: document.getElementById("main-panel"),
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

/** Drops back to the login screen after `Event::SessionExpired` (the
 * server rejected this device's access token — see that event's doc
 * comment in `event.rs`) or `Event::LoggedOut` (`Command::Logout`, the
 * user signing out deliberately). Both wipe the local session/store on
 * the Rust side before sending their event, so there's nothing left to
 * reuse either way.
 *
 * A full page reload rather than manually resetting `state` back to its
 * initial shape: `state` has grown dozens of fields over time (rooms,
 * every open room's timeline/thread/image/member/... caches, dialog
 * state, ...) and hand-picking every one that needs clearing is exactly
 * the kind of thing that quietly rots the next time a field gets added
 * elsewhere and someone forgets this needs to know about it too — a
 * fresh page load can't have that problem, it starts from the same
 * `const state = {...}` literal a real first launch does. The one thing
 * a reload *can't* do on its own is re-ask the backend whether a session
 * exists — the Rust worker only runs `Command::CheckSession` once, right
 * as its own process starts (see the "---- Boot ----" comment below) —
 * but that's fine here: both callers already know for certain there's no
 * valid session left, so the reloaded page landing on its default login
 * screen (nothing yet having called `enterChat()`) is exactly right,
 * no re-check needed. `message`, if given, survives the reload via
 * `sessionStorage` and is shown once the fresh page's login form exists
 * (see the boot-time check for `postReloadLoginMessage` below). */
function reloadToLogin(message) {
  try {
    if (message) sessionStorage.setItem("postReloadLoginMessage", message);
    else sessionStorage.removeItem("postReloadLoginMessage");
  } catch {
    // Private-browsing-style storage blocks, etc. — the reload itself
    // still works fine, the only loss is the explanatory message.
  }
  window.location.reload();
}

function enterChat() {
  state.screen = "chat";
  el.loginScreen.classList.add("hidden");
  el.chatScreen.classList.add("active");
  // The room list otherwise just sits blank until the first
  // "RoomListUpdate" event — which needs a full initial sync to complete
  // first, easily a few seconds (longer on a flaky connection, e.g. the
  // retry-heavy path right after an Android OAuth login) — with nothing to
  // tell the user whether that's still in progress or the app is just
  // stuck. `renderRooms()` only ever `appendChild`s the rows it wants
  // shown — it doesn't clear the container first — so this placeholder
  // has to be registered in `roomRowEls` under a key `renderRooms()` will
  // never re-emit, or it'd sit there forever above the real rows once
  // they start arriving (its own removal-of-anything-not-`keepKeys`
  // cleanup pass is what actually takes it back out).
  const loadingPlaceholder = document.createElement("div");
  loadingPlaceholder.style.cssText = "padding:12px;font-size:12px;text-align:center;";
  loadingPlaceholder.innerHTML = loadingHtml("loading rooms...");
  loadingPlaceholder.dataset.rowKey = "startup-loading-placeholder";
  el.roomListItems.innerHTML = "";
  el.roomListItems.appendChild(loadingPlaceholder);
  roomRowEls.set("startup-loading-placeholder", loadingPlaceholder);
  // Fetched here (not just lazily when the profile panel opens, its only
  // previous caller) so `state.ownProfile` is already populated by the
  // time the user sends their first message — `sendCurrentMessage` needs
  // it to stamp a sender name/avatar onto the optimistic "sending…" bubble
  // it renders immediately, before any server round trip.
  if (!state.ownProfile) send("GetOwnProfile");
}

// =========================================================================
// Room list / spaces / create room
// =========================================================================

// Debounced — `renderRooms()` re-walks every room in `state.rooms` to
// decide what's visible under the new filter, and on an account with
// thousands of rooms that's real work even though existing rows are
// reused rather than rebuilt (see `renderRoomRow`). Running it on every
// single keystroke made fast typing itself feel laggy, one pass behind
// each character. A short debounce lets a burst of keystrokes settle
// before paying for that pass once.
let roomFilterDebounceTimer = null;
// Same debounce idea for the threads-list panel's own search box — see
// `renderSidePanel`'s "threads-list" branch.
let threadsFilterDebounceTimer = null;
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
    scrollRoomListRowIntoView(state.roomListActiveIndex);
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

el.btnReloadRooms.addEventListener("click", () => {
  if (state.reloadingRooms) return;
  state.reloadingRooms = true;
  el.btnReloadRooms.disabled = true;
  el.btnReloadRooms.textContent = "[ reloading… ]";
  send("RefreshRooms");
});

// Keeps asking the backend for more of the room list (see
// `Command::GrowRoomList`) automatically in the background, rather than
// waiting for the user to scroll near the bottom — this app's actual
// homeserver protocol (MSC4186 / "Simplified Sliding Sync") only supports
// a growing-prefix model, not true server-side viewport ranges (see
// `RoomListOp`'s doc comment on the Rust side). Continuously asking for
// more here doesn't reintroduce the original "3000 rooms blocks the UI"
// problem: the server paces the underlying sync itself in small batches
// (see `SlidingSyncMode::Growing` on the Rust side), so this just reveals
// whatever's already been synced so far, a little at a time, without ever
// blocking on one huge fetch.
let growRoomListTimer = null;
let lastGrowRoomListCount = -1;
let growRoomListNoProgressStreak = 0;
const GROW_ROOM_LIST_BASE_DELAY_MS = 150;
const GROW_ROOM_LIST_MAX_DELAY_MS = 5000;

function scheduleGrowRoomList() {
  clearTimeout(growRoomListTimer);
  const grew = state.roomEntries.length !== lastGrowRoomListCount;
  lastGrowRoomListCount = state.roomEntries.length;
  growRoomListNoProgressStreak = grew ? 0 : growRoomListNoProgressStreak + 1;
  // Back off (up to 5s between attempts) once several rounds in a row
  // didn't reveal any new room — likely caught up to whatever the server
  // has synced so far — but never stop entirely, since the server's own
  // background growing sync (or the account joining a new room) can
  // still add more later.
  const delay = Math.min(
    GROW_ROOM_LIST_MAX_DELAY_MS,
    GROW_ROOM_LIST_BASE_DELAY_MS * 2 ** Math.min(growRoomListNoProgressStreak, 8),
  );
  growRoomListTimer = setTimeout(() => send("GrowRoomList"), delay);
}

el.btnUnreadOnly.addEventListener("click", () => {
  state.unreadOnly = !state.unreadOnly;
  el.btnUnreadOnly.classList.toggle("selected", state.unreadOnly);
  renderRooms();
});

el.btnMentionsOnly.addEventListener("click", () => {
  state.mentionsOnly = !state.mentionsOnly;
  el.btnMentionsOnly.classList.toggle("selected", state.mentionsOnly);
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
// A backend room-list diff can race a live `NewMessage`'s own optimistic
// unread-count bump (see that handler below): both are triggered by the
// same incoming event, but if the backend computed its `RoomSummary`
// before the server's own unread count had caught up (observed to be
// *every* time in practice — the notification count isn't available yet
// in the same sliding-sync response that carries the message), applying
// that diff wholesale would silently wipe the bump back down to 0. This
// isn't limited to `Set` diffs: a room jumping to the top of the list (the
// overwhelmingly common case) arrives as `Remove` + `PushFront`/`Insert`
// instead, whose value is a brand new object with no relation to the
// bumped one that just got removed — so the protection has to be keyed by
// `room_id`, independent of *which* op or index carries the value, not
// just diffed against "the old value at this same index" (that only
// covers `Set`). `applyUnreadFloor` keeps the highest unread count seen
// for each room instead of blindly trusting whichever arrived last. Rooms
// just marked read via the "[ mark read ]" button are exempted for a few
// seconds (see `recentlyMarkedRead`) so that action still actually zeroes
// the badge instead of this protection fighting it.
const recentlyMarkedRead = new Map(); // room_id -> Date.now() it was marked
const RECENTLY_MARKED_READ_WINDOW_MS = 5000;
const knownUnreadCounts = new Map(); // room_id -> highest unread_count observed
// Same race, same fix, for the mention-specific count — see
// `RoomSummary::mention_count`.
const knownMentionCounts = new Map(); // room_id -> highest mention_count observed

function applyUnreadFloor(value) {
  if (!value) return value;
  const markedAt = recentlyMarkedRead.get(value.room_id);
  if (markedAt && Date.now() - markedAt < RECENTLY_MARKED_READ_WINDOW_MS) {
    knownUnreadCounts.set(value.room_id, value.unread_count);
    knownMentionCounts.set(value.room_id, value.mention_count);
    return value;
  }
  const unreadFloor = knownUnreadCounts.get(value.room_id) || 0;
  const mentionFloor = knownMentionCounts.get(value.room_id) || 0;
  knownUnreadCounts.set(value.room_id, Math.max(value.unread_count, unreadFloor));
  knownMentionCounts.set(value.room_id, Math.max(value.mention_count, mentionFloor));
  if (value.unread_count < unreadFloor || value.mention_count < mentionFloor) {
    return {
      ...value,
      unread_count: Math.max(value.unread_count, unreadFloor),
      mention_count: Math.max(value.mention_count, mentionFloor),
    };
  }
  return value;
}

/** Applies one backend `RoomListOp` to `entries` (either `state.roomEntries`
 * or `state.inviteEntries`) in place — a straight port of
 * `eyeball_im::VectorDiff`'s semantics (see the Rust `RoomListOp` enum in
 * `event.rs`). */
function applyRoomListOp(entries, op) {
  switch (op.op) {
    case "Append":
      entries.push(...op.values.map(applyUnreadFloor));
      break;
    case "Clear":
      entries.length = 0;
      break;
    case "PushFront":
      entries.unshift(applyUnreadFloor(op.value));
      break;
    case "PushBack":
      entries.push(applyUnreadFloor(op.value));
      break;
    case "PopFront":
      entries.shift();
      break;
    case "PopBack":
      entries.pop();
      break;
    case "Insert":
      entries.splice(op.index, 0, applyUnreadFloor(op.value));
      break;
    case "Set":
      entries[op.index] = applyUnreadFloor(op.value);
      break;
    case "Remove":
      entries.splice(op.index, 1);
      break;
    case "Truncate":
      entries.length = op.length;
      break;
    case "Reset":
      entries.length = 0;
      entries.push(...op.values.map(applyUnreadFloor));
      break;
    default:
      console.warn("unknown RoomListOp", op);
  }
}

/** Shared ordering for `state.rooms`: invites first, then favorited rooms
 * (`Command::SetRoomFavorite` — this app's "pin to top", same `m.favourite`
 * tag Element's own "Favourites" uses), then whatever relative order was
 * already there. A *stable* sort — Array.prototype.sort has guaranteed
 * stability since ES2019 — so this only ever hoists invites/favorites up
 * without reshuffling anything else, whether that existing order is
 * `roomEntries`' own sliding-sync activity order (`rebuildRoomsFromEntries`)
 * or the recency bump `Event::NewMessage` applies itself right before
 * calling this. */
function sortRoomsList() {
  state.rooms.sort((a, b) => (b.is_invite - a.is_invite) || (b.is_favorite - a.is_favorite));
}

/** Rebuilds the flat `state.rooms` array — what every other part of the UI
 * reads — from the two index-mirrored source arrays, invites first (same
 * ordering convention `refresh_rooms` used before this migration), then
 * favorites (see `sortRoomsList`). */
function rebuildRoomsFromEntries() {
  state.rooms = [...state.inviteEntries, ...state.roomEntries];
  sortRoomsList();
}

// `poll_events` drains its whole backlog in one synchronous JS turn (see
// its own comment in lib.rs) — on a freshly reopened app, the initial
// sync burst can deliver dozens of `RoomListUpdate` events in a single
// poll. Each one used to call `renderRooms()` directly, which rebuilds
// the space picker and (below the virtualization threshold) measures
// `getBoundingClientRect()` per row for the FLIP animation — calling
// that once per event back-to-back with no yield is exactly what froze
// the UI (clicks/scroll unresponsive) while the room list was still
// catching up. State (`state.rooms`, via `rebuildRoomsFromEntries`)
// still applies immediately every time so nothing reads stale data;
// only the expensive DOM rebuild is coalesced to once per animation
// frame no matter how many updates land before it fires.
let roomsRenderScheduled = false;
function scheduleRoomsRender() {
  if (roomsRenderScheduled) return;
  roomsRenderScheduled = true;
  requestAnimationFrame(() => {
    roomsRenderScheduled = false;
    renderRooms();
  });
}

// Sentinel `selectedSpace` value for the dedicated "invites" tab — not a
// real space room ID, so every place that treats `selectedSpace` as one
// (looking up `spaceChildren`, sending `ListSpaceChildren`) has to check
// for this first. Room invites used to always show mixed in at the top of
// every tab (including "[ all ]"); pulled out into its own tab instead so
// the regular tabs only ever show rooms already joined, and an invite
// doesn't clutter every other view until it's dealt with.
const INVITES_TAB = "__invites__";

function renderSpacePicker() {
  const spaces = state.spaces.filter((r) => !r.is_invite);
  el.spacePicker.innerHTML = "";
  if (spaces.length === 0 && state.inviteEntries.length === 0) return;

  const entries = [{ id: null, label: "[ all ]", title: "all rooms" }];
  if (state.inviteEntries.length > 0) {
    entries.push({
      id: INVITES_TAB,
      label: `[ invites (${state.inviteEntries.length}) ]`,
      title: "room invites",
    });
  }
  entries.push(...spaces.map((s) => ({
    id: s.room_id,
    label: s.name,
    title: s.name,
  })));

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
  if (spaceId && spaceId !== INVITES_TAB && !state.spaceChildren[spaceId]) {
    send("ListSpaceChildren", { space_room_id: spaceId });
  }
  renderRooms();
}

/** Builds (once) or updates (every render) the row for one invite. The
 * node is reused across renders — see `roomRowEls` — so listeners are
 * attached only when it's first created. */
function renderInviteRow(room) {
  const key = "invite:" + room.room_id;
  let row = roomRowEls.get(key);
  if (!row) {
    row = document.createElement("div");
    row.className = "invite-row";
    row.innerHTML = `<div><b>invite:</b> <span data-name></span></div>
      <div class="actions">
        <button data-accept>accept</button>
        <button data-decline>decline</button>
      </div>`;
    row.querySelector("[data-accept]").addEventListener("click", () => {
      send("AcceptInvite", { room_id: room.room_id });
      // Jump straight into the room being accepted instead of leaving the
      // user on the invites tab looking at a list entry that's about to
      // disappear from it — the room is already in `state.rooms` (as the
      // invite itself) with a real name, so this works immediately, no
      // need to wait for the accept to round-trip first.
      selectRoom(room.room_id);
    });
    row.querySelector("[data-decline]").addEventListener("click", () =>
      send("DeclineInvite", { room_id: room.room_id }),
    );
    row.dataset.rowKey = key;
    roomRowEls.set(key, row);
  }
  row.querySelector("[data-name]").textContent = room.name;
  return { key, row };
}

/** Same idea for a regular room row — created once, then just has its
 * text/classes patched on every subsequent render instead of being torn
 * down and rebuilt. `rowIndex` (this room's position in the currently
 * *visible* filtered list, used by arrow-key navigation and the hover
 * highlight) does change across renders even when the room itself
 * doesn't move, so it's read from a data attribute at event time rather
 * than captured in the listener closure. */
function renderRoomRow(room, rowIndex) {
  const key = "room:" + room.room_id;
  let row = roomRowEls.get(key);
  let avatarEl, nameEl, badgeEl;
  if (!row) {
    row = document.createElement("div");
    row.className = "room-row";
    avatarEl = renderAvatar(room.avatar_url, room.name, room.room_id, 32);
    avatarEl.dataset.avatarSrc = room.avatar_url || "";
    row.appendChild(avatarEl);
    nameEl = document.createElement("div");
    nameEl.className = "room-name";
    row.appendChild(nameEl);
    row.addEventListener("click", () => selectRoom(row.dataset.roomId));
    row.addEventListener("mouseenter", () => {
      state.roomListActiveIndex = Number(row.dataset.rowIndex);
      el.roomListItems.querySelector(".room-row.kbd-active")?.classList.remove("kbd-active");
      row.classList.add("kbd-active");
    });
    roomRowEls.set(key, row);
  } else {
    avatarEl = row.querySelector(".avatar");
    nameEl = row.querySelector(".room-name");
    badgeEl = row.querySelector(".room-badge");
  }

  // Rebuilt only when the room's own avatar actually changed (rare) —
  // `renderAvatar` handles its own image-cache/fallback-initial logic,
  // so re-running it every patch (this function is called on every room-
  // list update, not just when something room-specific changed) would be
  // wasted work for the overwhelmingly common case of nothing changing.
  if (avatarEl.dataset.avatarSrc !== (room.avatar_url || "")) {
    const freshAvatar = renderAvatar(room.avatar_url, room.name, room.room_id, 32);
    freshAvatar.dataset.avatarSrc = room.avatar_url || "";
    avatarEl.replaceWith(freshAvatar);
    avatarEl = freshAvatar;
  }

  row.dataset.roomId = room.room_id;
  row.dataset.rowKey = key;
  row.dataset.rowIndex = String(rowIndex);
  row.className =
    "room-row" +
    (room.room_id === state.selectedRoom ? " selected" : "") +
    (rowIndex === state.roomListActiveIndex ? " kbd-active" : "");

  const name = (room.is_favorite ? "★ " : "") + (room.is_encrypted ? "[e] " : "") + room.name;
  if (nameEl.textContent !== name) nameEl.textContent = name;
  nameEl.className = "room-name" + (room.unread_count > 0 ? " unread" : "");

  if (room.unread_count > 0) {
    const badgeText = room.unread_count > 99 ? "99+" : String(room.unread_count);
    if (!badgeEl) {
      badgeEl = document.createElement("span");
      badgeEl.className = "room-badge";
      row.appendChild(badgeEl);
    }
    if (badgeEl.textContent !== badgeText) badgeEl.textContent = badgeText;
    // Red "you were mentioned" styling instead of the plain grey
    // just-unread badge — same visual distinction Element makes.
    badgeEl.classList.toggle("mention", room.mention_count > 0);
  } else if (badgeEl) {
    badgeEl.remove();
  }

  return { key, row };
}

/** Renders the room list by reusing/patching existing row elements
 * (`roomRowEls`) rather than wiping and rebuilding the whole list on
 * every call — that rebuild used to run on every single unread-count
 * change (a message arriving, `[ mark read ]`, the reload button, ...)
 * and was the source of the list visibly jumping/flickering each time,
 * since content-visibility:auto rows getting destroyed and recreated
 * forces the browser to redo layout for the whole visible viewport even
 * when nothing but one badge changed. Existing rows just get their
 * text/classes patched in place; only actual additions/removals/reorders
 * touch the DOM tree, and those are animated (`animateRoomListChanges`)
 * instead of happening instantly. */
function renderRooms() {
  renderSpacePicker();
  const filter = normalizeForSearch(state.roomFilter.trim());
  // The invites tab itself only shows up in the picker while there's at
  // least one invite (see `renderSpacePicker`) — if the last one just got
  // accepted/declined while this tab was open, fall back to "[ all ]"
  // instead of leaving `selectedSpace` pointed at a tab that no longer
  // exists (which would otherwise render an empty list forever).
  if (state.selectedSpace === INVITES_TAB && state.inviteEntries.length === 0) {
    state.selectedSpace = null;
  }
  const onInvitesTab = state.selectedSpace === INVITES_TAB;
  const spaceFilter =
    state.selectedSpace && !onInvitesTab ? state.spaceChildren[state.selectedSpace] : null;

  // Matching is separated from mounting: this pass only decides *which*
  // rooms belong in the list and in what order, so the (potentially
  // thousands-long) result can be handed to the virtualized window path
  // below without ever building a row for one that won't actually be
  // shown on screen.
  const matchedRooms = [];
  for (const room of state.rooms) {
    if (room.is_invite || onInvitesTab || room.is_space) continue;
    if (filter && !normalizeForSearch(room.name).includes(filter)) continue;
    if (spaceFilter && !spaceFilter.includes(room.room_id)) continue;
    if (state.unreadOnly && !(room.unread_count > 0)) continue;
    if (state.mentionsOnly && !(room.mention_count > 0)) continue;
    matchedRooms.push(room);
  }
  state.visibleRoomIds = matchedRooms.map((room) => room.room_id);
  if (state.roomListActiveIndex >= state.visibleRoomIds.length) {
    state.roomListActiveIndex = state.visibleRoomIds.length - 1;
  }

  const virtualize = !onInvitesTab && matchedRooms.length > ROOM_LIST_VIRTUALIZE_THRESHOLD;
  if (virtualize) {
    virtualRoomList = matchedRooms;
    // Anything cached from a previous render that's no longer in the
    // matched set at all (room left, filtered out) is evicted here —
    // rows merely scrolled out of the current window are *not* touched,
    // they stay cached in `roomRowEls` for `renderRoomListWindow()` to
    // reuse the moment they scroll back into view.
    const keepKeys = new Set(state.visibleRoomIds.map((id) => "room:" + id));
    for (const [key, node] of roomRowEls) {
      if (keepKeys.has(key)) continue;
      roomRowEls.delete(key);
      node.remove();
    }
    renderRoomListWindow();
    return;
  }
  virtualRoomList = null;
  roomListTopSpacer?.remove();
  roomListBottomSpacer?.remove();

  // The FLIP animation below (`animateRoomListChanges`) needs a
  // `getBoundingClientRect()` per already-rendered row — fine at this
  // (below-virtualization-threshold) size, so it stays enabled outside
  // active filtering, where sliding a reordered room into place is worth
  // the extra measuring. Skipped while filtering: the search-as-you-type
  // case gets no benefit from the animation (results are still settling
  // keystroke to keystroke) and re-triggers this function most often.
  const filtering = Boolean(filter) || Boolean(spaceFilter) || state.unreadOnly || state.mentionsOnly;
  const prevRects = new Map();
  if (!filtering) {
    for (const [key, node] of roomRowEls) {
      if (node.isConnected) prevRects.set(key, node.getBoundingClientRect());
    }
  }

  const keepKeys = new Set();
  const orderedRows = [];

  for (let i = 0; i < matchedRooms.length; i++) {
    const { key, row } = renderRoomRow(matchedRooms[i], i);
    keepKeys.add(key);
    orderedRows.push(row);
  }
  if (onInvitesTab) {
    for (const room of state.rooms) {
      if (!room.is_invite) continue;
      const { key, row } = renderInviteRow(room);
      keepKeys.add(key);
      orderedRows.push(row);
    }
  }

  const showEmptyPlaceholder = (state.unreadOnly || state.mentionsOnly) && orderedRows.length === 0;
  if (showEmptyPlaceholder) {
    const key = "empty-placeholder";
    let row = roomRowEls.get(key);
    if (!row) {
      row = document.createElement("div");
      row.style.cssText = "padding:12px;color:var(--text-weak);font-size:12px;text-align:center;";
      row.dataset.rowKey = key;
      roomRowEls.set(key, row);
    }
    row.textContent = state.mentionsOnly ? "no unread mentions" : "no unread rooms";
    keepKeys.add(key);
    orderedRows.push(row);
  }

  // Anything no longer wanted (filtered out, room left, invite resolved)
  // fades out instead of just vanishing.
  const exiting = [];
  for (const [key, node] of roomRowEls) {
    if (keepKeys.has(key)) continue;
    roomRowEls.delete(key);
    if (node.isConnected) exiting.push(node);
  }

  // `appendChild` on a node already in the DOM *moves* it rather than
  // cloning it, so replaying the full desired order here is enough to
  // both reorder existing rows and insert new ones — no manual
  // insertBefore bookkeeping needed.
  for (const row of orderedRows) el.roomListItems.appendChild(row);

  if (filtering) {
    // No FLIP pass — see the comment above `filtering`'s declaration.
    // Exiting rows (filtered out) are just removed outright rather than
    // faded, same reasoning: the fade is nice on a live list update, not
    // worth the extra measuring while search results are still settling
    // keystroke to keystroke.
    for (const row of exiting) row.remove();
  } else {
    animateRoomListChanges(prevRects, orderedRows, exiting);
  }
}

/** FLIP-animates whatever `renderRooms()` just changed: rows that moved
 * slide to their new position instead of jumping there, newly-added rows
 * fade/slide in, and removed rows fade out before actually leaving the
 * DOM. All transform/opacity — never touches layout-affecting properties
 * — so it's cheap even with a long list. */
function animateRoomListChanges(prevRects, orderedRows, exiting) {
  // Pull exiting rows out of flow *first* — otherwise they'd still be
  // occupying space when the "next" rects below are measured, throwing
  // off every delta computed for the rows around them. Pin them to their
  // current spot with explicit top/left/width (relative to the
  // scrollable container, so its `scrollTop` has to be folded in) since
  // switching to `position: absolute` alone would let them shrink to
  // content width and only *happens* to keep their vertical spot via the
  // browser's static-position fallback — not guaranteed once they no
  // longer have layout siblings pushing them there.
  const containerRect = el.roomListItems.getBoundingClientRect();
  for (const row of exiting) {
    const rect = row.getBoundingClientRect();
    row.style.transition = "none";
    row.style.position = "absolute";
    row.style.top = `${rect.top - containerRect.top + el.roomListItems.scrollTop}px`;
    row.style.left = "0";
    row.style.width = `${rect.width}px`;
    row.style.pointerEvents = "none";
  }
  // Forces that reflow to actually happen before the rects below read
  // off it, rather than getting coalesced with the batch of style writes
  // that follows.
  void el.roomListItems.offsetHeight;

  for (const row of orderedRows) {
    const prev = prevRects.get(roomKeyOf(row));
    row.style.transition = "none";
    if (prev) {
      const next = row.getBoundingClientRect();
      const dy = prev.top - next.top;
      if (Math.abs(dy) > 0.5) {
        row.style.transform = `translateY(${dy}px)`;
      }
    } else {
      // Genuinely new row — slide/fade in from just above its final spot.
      row.style.opacity = "0";
      row.style.transform = "translateY(-6px)";
    }
  }

  // Forces the browser to apply the "from" styles above before the "to"
  // styles below get a transition to animate across — without this the
  // two would coalesce into one paint and nothing would visibly move.
  void el.roomListItems.offsetHeight;

  for (const row of orderedRows) {
    row.style.transition = "transform 180ms ease, opacity 180ms ease";
    row.style.transform = "";
    row.style.opacity = "";
  }
  for (const row of exiting) {
    row.style.transition = "opacity 150ms ease";
    row.style.opacity = "0";
    setTimeout(() => row.remove(), 160);
  }
}

function roomKeyOf(row) {
  return row.dataset.rowKey;
}

/** Mounts only the slice of `virtualRoomList` that's actually scrolled
 * into view (+ overscan), flanked by two spacers sized to stand in for
 * the rows on either side that aren't mounted — so `#room-list-items`'s
 * scroll height still reflects "all N rows stacked" even though far
 * fewer than N ever touch the DOM. Reuses the same `renderRoomRow` cache
 * as the non-virtualized path, so a row that scrolls back into view after
 * scrolling away is patched in place rather than rebuilt. Called by
 * `renderRooms()` whenever it decides to virtualize, and again on every
 * scroll/resize of the list while it's virtualized. */
function renderRoomListWindow() {
  if (!virtualRoomList) return;
  const total = virtualRoomList.length;
  if (!roomListTopSpacer) {
    roomListTopSpacer = document.createElement("div");
    roomListTopSpacer.className = "room-list-spacer";
  }
  if (!roomListBottomSpacer) {
    roomListBottomSpacer = document.createElement("div");
    roomListBottomSpacer.className = "room-list-spacer";
  }

  const rowHeight = roomListMeasuredRowHeight || ROOM_LIST_DEFAULT_ROW_HEIGHT;
  const container = el.roomListItems;
  const viewportHeight = container.clientHeight || 400;
  let startIndex = Math.floor(container.scrollTop / rowHeight) - ROOM_LIST_OVERSCAN;
  let endIndex = Math.ceil((container.scrollTop + viewportHeight) / rowHeight) + ROOM_LIST_OVERSCAN;
  startIndex = Math.max(0, Math.min(startIndex, total - 1));
  endIndex = Math.max(startIndex, Math.min(endIndex, total - 1));

  const windowRows = [];
  for (let i = startIndex; i <= endIndex; i++) {
    const { row } = renderRoomRow(virtualRoomList[i], i);
    windowRows.push(row);
  }

  roomListTopSpacer.style.height = startIndex * rowHeight + "px";
  roomListBottomSpacer.style.height = (total - endIndex - 1) * rowHeight + "px";
  // `replaceChildren` both moves the window's rows into place (existing
  // nodes get relocated, not cloned) and detaches whatever fell out of
  // the window last time — no manual bookkeeping needed for that half.
  container.replaceChildren(roomListTopSpacer, ...windowRows, roomListBottomSpacer);

  // Refine the row-height estimate the spacers use once there's a real
  // row on screen to measure, so the scrollbar thumb settles to its true
  // size/position instead of staying pinned to the `contain-intrinsic-size`
  // guess forever.
  if (roomListMeasuredRowHeight == null && windowRows.length > 0) {
    const measured = windowRows[0].getBoundingClientRect().height;
    if (measured > 0) roomListMeasuredRowHeight = measured;
  }
}

el.roomListItems.addEventListener("scroll", () => {
  if (!virtualRoomList) return;
  if (roomListWindowRafPending) return;
  roomListWindowRafPending = true;
  requestAnimationFrame(() => {
    roomListWindowRafPending = false;
    renderRoomListWindow();
  });
});
window.addEventListener("resize", () => {
  if (virtualRoomList) renderRoomListWindow();
});

/** Same job as a plain `.room-row.kbd-active` `scrollIntoView()` (see the
 * room-filter arrow-key handler), but works while the list is virtualized
 * too, where the target row may not be mounted at all yet — scrolls the
 * container to where that row *will* be, then mounts the window there. */
function scrollRoomListRowIntoView(index) {
  if (index < 0) return;
  if (!virtualRoomList) {
    el.roomListItems.querySelector(".room-row.kbd-active")?.scrollIntoView({ block: "nearest" });
    return;
  }
  const rowHeight = roomListMeasuredRowHeight || ROOM_LIST_DEFAULT_ROW_HEIGHT;
  const container = el.roomListItems;
  const top = index * rowHeight;
  const bottom = top + rowHeight;
  if (top < container.scrollTop) container.scrollTop = top;
  else if (bottom > container.scrollTop + container.clientHeight) {
    container.scrollTop = bottom - container.clientHeight;
  }
  renderRoomListWindow();
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

/** Full-size image viewer — a message bubble's `<img>` is deliberately
 * capped small (`.bubble img { max-width: 280px; ... }`, see its CSS) so
 * a photo doesn't dominate the timeline; this is the "actually look at
 * it" affordance that cap implies but never had a way to act on before.
 * Tapping/clicking anywhere on the overlay closes it — same as
 * `showDialog`'s backdrop, just without needing to land exactly on a
 * particular element first. */
const LIGHTBOX_MIN_SCALE = 1;
const LIGHTBOX_MAX_SCALE = 6;
const LIGHTBOX_DOUBLE_TAP_SCALE = 2.5;

function openLightbox(src, alt) {
  closeLightbox();
  const overlay = document.createElement("div");
  overlay.className = "lightbox-backdrop";
  overlay.id = "active-lightbox";
  const img = document.createElement("img");
  img.className = "lightbox-img";
  img.src = src;
  img.alt = alt || "image";
  img.draggable = false;
  overlay.appendChild(img);
  const closeBtn = document.createElement("button");
  closeBtn.className = "lightbox-close small-btn";
  closeBtn.textContent = "[ x ]";
  overlay.appendChild(closeBtn);
  const zoomControl = document.createElement("div");
  zoomControl.className = "lightbox-zoom-controls";
  zoomControl.innerHTML = `
    <button type="button" class="small-btn" data-zoom="out" title="zoom out">−</button>
    <button type="button" class="small-btn" data-zoom="reset" title="reset zoom">${Math.round(LIGHTBOX_MIN_SCALE * 100)}%</button>
    <button type="button" class="small-btn" data-zoom="in" title="zoom in">+</button>
  `;
  overlay.appendChild(zoomControl);

  // Zoom/pan state — `scale`/`tx`/`ty` back a plain CSS
  // `translate(tx, ty) scale(scale)` on the image itself (see
  // `applyTransform` below). Reset fresh on every open.
  const zoom = { scale: 1, tx: 0, ty: 0 };

  const applyTransform = () => {
    img.style.transform = `translate(${zoom.tx}px, ${zoom.ty}px) scale(${zoom.scale})`;
    img.style.cursor = zoom.scale > 1 ? "grab" : "zoom-in";
    zoomControl.querySelector('[data-zoom="reset"]').textContent = `${Math.round(zoom.scale * 100)}%`;
    // The backdrop's own click-to-close only makes sense back at 1x — once
    // zoomed in, a plain click/tap on the image is how you'd expect to pan
    // from, not close the whole viewer out from under you.
    overlay.style.cursor = zoom.scale > 1 ? "default" : "zoom-out";
  };

  // Zooms so that the point under `clientX,clientY` stays fixed on screen —
  // standard "zoom toward cursor" — rather than always zooming toward the
  // image's own center, which feels wrong the moment you're not perfectly
  // centered on whatever you're trying to look closer at.
  const zoomTo = (newScale, clientX, clientY) => {
    const clamped = Math.min(LIGHTBOX_MAX_SCALE, Math.max(LIGHTBOX_MIN_SCALE, newScale));
    // `getBoundingClientRect()` reflects the *current* transform, so its
    // center is already `naturalCenter + (tx, ty)` — `dx`/`dy` below are
    // the cursor's offset from that already-translated center, not from
    // the image's untransformed natural center.
    const rect = img.getBoundingClientRect();
    const cx = rect.left + rect.width / 2;
    const cy = rect.top + rect.height / 2;
    const dx = clientX - cx;
    const dy = clientY - cy;
    const ratio = clamped / zoom.scale;
    // Cursor exactly on the current center (`dx`/`dy` = 0) must leave
    // `tx`/`ty` unchanged — scaling around the point already under the
    // cursor doesn't move that point. `tx * ratio` (scaling the existing
    // offset down too) would violate that; plain `+ zoom.tx` is correct.
    zoom.tx = dx * (1 - ratio) + zoom.tx;
    zoom.ty = dy * (1 - ratio) + zoom.ty;
    zoom.scale = clamped;
    if (zoom.scale === LIGHTBOX_MIN_SCALE) {
      zoom.tx = 0;
      zoom.ty = 0;
    }
    applyTransform();
  };

  img.addEventListener("wheel", (e) => {
    e.preventDefault();
    e.stopPropagation();
    const factor = e.deltaY < 0 ? 1.2 : 1 / 1.2;
    zoomTo(zoom.scale * factor, e.clientX, e.clientY);
  }, { passive: false });

  img.addEventListener("dblclick", (e) => {
    e.stopPropagation();
    if (zoom.scale > LIGHTBOX_MIN_SCALE) {
      zoomTo(LIGHTBOX_MIN_SCALE, e.clientX, e.clientY);
    } else {
      zoomTo(LIGHTBOX_DOUBLE_TAP_SCALE, e.clientX, e.clientY);
    }
  });

  // Drag-to-pan (mouse), only once actually zoomed in. `mousemove`/`mouseup`
  // are only ever attached to `window` for the duration of one actual drag
  // (added in `mousedown`, removed in `onDragUp`) — same pattern
  // `setupResizer` uses for the panel-resize handles, and for the same
  // reason: attaching them once up front and never removing them would
  // leak a pair of window-level listeners (holding onto this whole
  // closure — `zoom`, `img`, ...) every single time an image is opened,
  // for the rest of the session.
  let dragStart = null;
  function onDragMove(e) {
    if (!dragStart) return;
    zoom.tx = dragStart.tx + (e.clientX - dragStart.x);
    zoom.ty = dragStart.ty + (e.clientY - dragStart.y);
    applyTransform();
  }
  function onDragUp() {
    dragStart = null;
    img.style.cursor = zoom.scale > 1 ? "grab" : "zoom-in";
    window.removeEventListener("mousemove", onDragMove);
    window.removeEventListener("mouseup", onDragUp);
  }
  // Closing (Escape, the [x] button, ...) mid-drag must still clean these
  // up — `closeLightbox()` is a plain global function with no idea this
  // particular drag is in progress, so it calls this via the property
  // instead of just removing the overlay outright.
  overlay.lightboxCleanup = () => {
    if (dragStart) onDragUp();
  };
  img.addEventListener("mousedown", (e) => {
    if (zoom.scale <= LIGHTBOX_MIN_SCALE) return;
    e.preventDefault();
    dragStart = { x: e.clientX, y: e.clientY, tx: zoom.tx, ty: zoom.ty };
    img.style.cursor = "grabbing";
    window.addEventListener("mousemove", onDragMove);
    window.addEventListener("mouseup", onDragUp);
  });

  // Touch: pinch-to-zoom with two fingers, drag-to-pan with one (once
  // zoomed), double-tap to toggle zoom — same gestures as every native
  // photo viewer, since this same build also ships on Android.
  let pinchStartDist = null;
  let pinchStartScale = 1;
  let touchPanStart = null;
  let lastTapTime = 0;
  const touchDist = (touches) => Math.hypot(touches[0].clientX - touches[1].clientX, touches[0].clientY - touches[1].clientY);
  const touchMid = (touches) => ({
    x: (touches[0].clientX + touches[1].clientX) / 2,
    y: (touches[0].clientY + touches[1].clientY) / 2,
  });
  img.addEventListener("touchstart", (e) => {
    if (e.touches.length === 2) {
      e.preventDefault();
      pinchStartDist = touchDist(e.touches);
      pinchStartScale = zoom.scale;
      touchPanStart = null;
    } else if (e.touches.length === 1) {
      const now = Date.now();
      if (now - lastTapTime < 300) {
        const t = e.touches[0];
        if (zoom.scale > LIGHTBOX_MIN_SCALE) zoomTo(LIGHTBOX_MIN_SCALE, t.clientX, t.clientY);
        else zoomTo(LIGHTBOX_DOUBLE_TAP_SCALE, t.clientX, t.clientY);
        lastTapTime = 0;
        return;
      }
      lastTapTime = now;
      if (zoom.scale > LIGHTBOX_MIN_SCALE) {
        touchPanStart = { x: e.touches[0].clientX, y: e.touches[0].clientY, tx: zoom.tx, ty: zoom.ty };
      }
    }
  }, { passive: false });
  img.addEventListener("touchmove", (e) => {
    if (e.touches.length === 2 && pinchStartDist) {
      e.preventDefault();
      const mid = touchMid(e.touches);
      const newScale = pinchStartScale * (touchDist(e.touches) / pinchStartDist);
      zoomTo(newScale, mid.x, mid.y);
    } else if (e.touches.length === 1 && touchPanStart) {
      e.preventDefault();
      zoom.tx = touchPanStart.tx + (e.touches[0].clientX - touchPanStart.x);
      zoom.ty = touchPanStart.ty + (e.touches[0].clientY - touchPanStart.y);
      applyTransform();
    }
  }, { passive: false });
  img.addEventListener("touchend", (e) => {
    if (e.touches.length < 2) pinchStartDist = null;
    if (e.touches.length < 1) touchPanStart = null;
  });

  zoomControl.addEventListener("click", (e) => {
    e.stopPropagation();
    const action = e.target.closest("[data-zoom]")?.dataset.zoom;
    if (!action) return;
    const rect = img.getBoundingClientRect();
    const cx = rect.left + rect.width / 2;
    const cy = rect.top + rect.height / 2;
    if (action === "in") zoomTo(zoom.scale * 1.5, cx, cy);
    else if (action === "out") zoomTo(zoom.scale / 1.5, cx, cy);
    else zoomTo(LIGHTBOX_MIN_SCALE, cx, cy);
  });

  // Only the backdrop itself (not the image, not the zoom controls)
  // closes the viewer now that the image has its own click-driven
  // interactions (double-click to zoom, drag to pan) — closing on
  // `e.target === img` too used to also fire on the very first click of
  // a double-click (scale is still 1 and nothing's been dragged yet at
  // that point), destroying the overlay before `dblclick` ever got a
  // chance to fire.
  overlay.addEventListener("click", (e) => {
    if (e.target === overlay) closeLightbox();
  });
  closeBtn.addEventListener("click", (e) => {
    e.stopPropagation();
    closeLightbox();
  });
  document.addEventListener("keydown", lightboxKeyHandler);
  document.body.appendChild(overlay);
  applyTransform();
}
function closeLightbox() {
  const overlay = document.getElementById("active-lightbox");
  overlay?.lightboxCleanup?.();
  overlay?.remove();
  document.removeEventListener("keydown", lightboxKeyHandler);
}
function lightboxKeyHandler(e) {
  if (e.key === "Escape") closeLightbox();
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
  el.roomMenu.style.display = "none";
  cancelReply();
  cancelEdit();
  const room = state.rooms.find((r) => r.room_id === roomId);
  el.timelineTitle.textContent = room ? room.name : roomId;
  updateFavoriteButtonLabel(room);
  el.timelineHeaderActions.style.display = "flex";
  el.composeRow.style.display = "flex";
  el.composeToolbar.style.display = "flex";
  // Opening a room opens its threads list by default (rather than
  // leaving `rightPanel` closed until the user hits Alt+T) — same
  // request/state shape as `openRoomThreadsList()`, inlined instead of
  // calling it since that function also calls `renderSidePanel()` itself,
  // which happens below anyway. Desktop layout only: on a narrow/phone
  // layout (see `isNarrowLayout()`), `#side-panel` is a full-screen
  // overlay, so this would otherwise bury the timeline the user just
  // tapped to see behind the thread list on every single room open.
  if (isNarrowLayout()) {
    state.rightPanel = null;
  } else {
    send("ListThreads", { room_id: roomId });
    state.rightPanel = { kind: "threads-list", scope: roomId };
    state.threadsListActiveIndex = -1;
    state.threadsListFilter = "";
  }
  renderRooms();
  renderSidePanel();
  if (!state.timelineLoaded.has(roomId)) {
    el.timeline.innerHTML = `<div id="timeline-placeholder">${loadingHtml("loading messages...")}</div>`;
    send("LoadTimeline", { room_id: roomId });
  } else {
    renderTimeline();
    maybeAutoLoadMore(roomId);
  }
  // Deliberately not auto-marking read just for opening the room — read
  // state only ever changes via the explicit "[ mark read ]" button (see
  // `el.btnMarkRead`'s handler), so the unread badge stays put until the
  // user actually says they're done with it, rather than clearing itself
  // the instant a room is clicked open.
  if (!state.roomMembers[roomId]) send("ListMembers", { room_id: roomId });
  if (!state.imagePacks[roomId]) send("ListImagePacks", { room_id: roomId });
  updateNotificationModeUi();
  send("GetNotificationMode", { room_id: roomId });
  updateThreadsButtonBadge();

  send("WatchTyping", { room_id: roomId });
  send("GetRoomInfo", { room_id: roomId });
  send("GetPinnedEvents", { room_id: roomId });
  renderTypingIndicator();
  renderPinnedBanner();
}

function closeRoomMenu() {
  el.roomMenu.style.display = "none";
}
function closeChatsMenu() {
  el.chatsMenu.style.display = "none";
}
el.btnBackToRooms.addEventListener("click", () => {
  el.chatScreen.classList.remove("room-open");
});
el.btnRoomMenu.addEventListener("click", (e) => {
  e.stopPropagation();
  el.roomMenu.style.display = el.roomMenu.style.display === "none" ? "flex" : "none";
});
el.btnChatsMenu.addEventListener("click", (e) => {
  e.stopPropagation();
  el.chatsMenu.style.display = el.chatsMenu.style.display === "none" ? "flex" : "none";
});
document.addEventListener("click", (e) => {
  if (el.roomMenu.style.display !== "none" && !el.roomMenu.contains(e.target) && e.target !== el.btnRoomMenu) {
    closeRoomMenu();
  }
  if (el.chatsMenu.style.display !== "none" && !el.chatsMenu.contains(e.target) && e.target !== el.btnChatsMenu) {
    closeChatsMenu();
  }
});

/** Sends the invite and closes the dialog — shared by clicking a search
 * result and by typing a full id and pressing enter. */
function submitInvite(roomId, userId, displayLabel) {
  if (!userId) return;
  send("InviteUser", { room_id: roomId, user_id: userId });
  showToast(`invite sent to ${displayLabel || userId}`);
  closeDialog();
}

/** Renders the result list inside the "invite to room" dialog: known
 * contacts (`state.allUsers`, from every joined room, filtered locally —
 * instant) merged with the homeserver's user directory search
 * (`state.directorySearch`, debounced, so it can also surface people the
 * account has no room in common with yet), deduped by user id. */
function renderInviteDialogList() {
  const listEl = document.getElementById("dlg-invite-list");
  if (!listEl) return;
  const roomId = state.selectedRoom;
  const rawQuery = document.getElementById("dlg-invite-filter").value.trim();
  const query = normalizeForSearch(rawQuery);

  const byId = new Map(); // user_id -> { userId, name, avatarUrl }
  if (query && state.allUsers) {
    for (const [userId, name] of state.allUsers) {
      if (normalizeForSearch(name).includes(query) || normalizeForSearch(userId).includes(query)) {
        byId.set(userId, { userId, name, avatarUrl: null });
      }
    }
  }
  const searching = query.length > 0 && (state.directorySearch === null || state.directorySearch.query !== rawQuery);
  if (state.directorySearch && state.directorySearch.query === rawQuery) {
    for (const u of state.directorySearch.users) {
      byId.set(u.user_id, { userId: u.user_id, name: u.display_name || u.user_id, avatarUrl: u.avatar_url });
    }
  }

  listEl.innerHTML = "";
  if (!query) {
    listEl.innerHTML = `<div style="padding:8px;font-size:12px;color:var(--text-weak)">type a name, or a full @user:server id</div>`;
    return;
  }
  const matches = [...byId.values()].slice(0, 50);
  if (matches.length === 0) {
    listEl.innerHTML = `<div style="padding:8px;font-size:12px;color:var(--text-weak)">${
      searching ? "searching..." : "no matching user"
    }${rawQuery.startsWith("@") && rawQuery.includes(":") ? " — press enter to invite this id anyway" : ""}</div>`;
    return;
  }
  for (const { userId, name, avatarUrl } of matches) {
    const item = document.createElement("div");
    item.className = "mention-item";
    item.style.display = "flex";
    item.style.alignItems = "center";
    item.style.gap = "8px";
    item.appendChild(renderAvatar(avatarUrl, name, userId, 24));
    const label = document.createElement("span");
    label.textContent = userId === name ? name : `${name}  (${userId})`;
    item.appendChild(label);
    item.addEventListener("click", () => submitInvite(roomId, userId, name));
    listEl.appendChild(item);
  }
  if (searching) {
    const loading = document.createElement("div");
    loading.style.padding = "6px 8px";
    loading.style.fontSize = "12px";
    loading.style.color = "var(--text-weak)";
    loading.textContent = "searching directory...";
    listEl.appendChild(loading);
  }
}

/** Keeps the room menu's "[ ☆ favorite ]"/"[ ★ favorited ]" button label
 * in sync with `room.is_favorite` — called on every `selectRoom()` and
 * again once `Event::RoomFavoriteSet` confirms a toggle. `room` may be
 * `undefined` right after switching to a room whose summary hasn't
 * synced in yet; the button just falls back to the unfavorited label
 * until it has. */
function updateFavoriteButtonLabel(room) {
  el.btnFavoriteRoom.textContent = room?.is_favorite ? "[ ★ favorited ]" : "[ ☆ favorite ]";
}

el.btnFavoriteRoom.addEventListener("click", () => {
  closeRoomMenu();
  if (!state.selectedRoom) return;
  const room = state.rooms.find((r) => r.room_id === state.selectedRoom);
  send("SetRoomFavorite", { room_id: state.selectedRoom, favorite: !room?.is_favorite });
});

let inviteSearchDebounceTimer = null;

el.btnInvite.addEventListener("click", () => {
  closeRoomMenu();
  if (!state.selectedRoom) return;
  const roomId = state.selectedRoom;
  state.directorySearch = null;
  showDialog(`
    <h3>invite to room</h3>
    <label>search by name, or type a full @user:server id</label>
    <input type="text" id="dlg-invite-filter" placeholder="name or @user:server" autocomplete="off" />
    <div id="dlg-invite-list" class="user-search-list"></div>
    <div class="actions">
      <button id="dlg-cancel">cancel</button>
    </div>
  `);
  document.getElementById("dlg-cancel").addEventListener("click", closeDialog);
  const filterInput = document.getElementById("dlg-invite-filter");
  filterInput.addEventListener("input", () => {
    renderInviteDialogList();
    const query = filterInput.value.trim();
    clearTimeout(inviteSearchDebounceTimer);
    if (!query) return;
    inviteSearchDebounceTimer = setTimeout(() => send("SearchDirectoryUsers", { query }), 250);
  });
  filterInput.addEventListener("keydown", (e) => {
    if (e.key !== "Enter") return;
    const typed = filterInput.value.trim();
    if (typed.startsWith("@") && typed.includes(":")) submitInvite(roomId, typed, typed);
  });
  if (state.allUsers === null && !state.allUsersLoading) {
    state.allUsersLoading = true;
    send("ListAllUsers");
  }
  renderInviteDialogList();
  filterInput.focus();
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
  el.composeToolbar.style.display = "none";
  renderSidePanel();
});

el.notificationMode.addEventListener("change", () => {
  if (!state.selectedRoom) return;
  send("SetNotificationMode", { room_id: state.selectedRoom, mode: el.notificationMode.value });
});

/** Patches every already-rendered presence dot in place (main timeline and
 * thread panel both — see the `data-presence-user` marker `renderMessageGroup`
 * puts on each one, overlaid on the sender's avatar) rather than a full
 * re-render, since a presence change is exactly the kind of thing that can
 * arrive in a steady trickle while scrolled somewhere unrelated. */
function updateMemberListPresenceDots() {
  document.querySelectorAll("[data-presence-user]").forEach((dot) => {
    const info = state.presence[dot.dataset.presenceUser];
    dot.className = "avatar-presence-dot" + (info ? ` ${info.presence}` : "");
    dot.title = info ? info.presence : "";
  });
}

function updateNotificationModeUi() {
  const mode = state.notificationModes[state.selectedRoom];
  if (mode) el.notificationMode.value = mode;
}

/** Renders the "X is typing..."/"X and Y are typing..." banner for
 * whichever room is currently open — `state.typingUsers[roomId]` is only
 * ever populated for that one room (see `Command::WatchTyping`), so no
 * per-room filtering is needed here beyond just reading it. */
function renderTypingIndicator() {
  const roomId = state.selectedRoom;
  const userIds = (roomId && state.typingUsers[roomId]) || [];
  if (!roomId || userIds.length === 0) {
    el.typingIndicator.style.display = "none";
    return;
  }
  const members = state.roomMembers[roomId] || [];
  const nameOf = (uid) => members.find((m) => m[0] === uid)?.[1] || uid;
  const names = userIds.map(nameOf);
  let text;
  if (names.length === 1) text = `${names[0]} is typing...`;
  else if (names.length === 2) text = `${names[0]} and ${names[1]} are typing...`;
  else text = `${names.length} people are typing...`;
  el.typingIndicator.textContent = text;
  el.typingIndicator.style.display = "block";
}

/** Renders the pinned-message banner just under the room header — shows
 * the most recently pinned message's preview text; clicking it opens the
 * full pins list (same dialog as "[ pins ]"). Hidden entirely when the
 * room has no pinned messages. */
function renderPinnedBanner() {
  const roomId = state.selectedRoom;
  const eventIds = (roomId && state.pinnedEvents[roomId]) || [];
  if (!roomId || eventIds.length === 0) {
    el.pinnedBanner.style.display = "none";
    el.pinnedBanner.innerHTML = "";
    return;
  }
  const latestId = eventIds[eventIds.length - 1];
  const ev = findEvent(roomId, latestId);
  const preview = ev ? `${ev.sender_name}: ${truncate(ev.body || "", 80)}` : "pinned message";
  const countSuffix = eventIds.length > 1 ? ` (+${eventIds.length - 1} more)` : "";
  el.pinnedBanner.innerHTML = `<span class="pinned-icon">📌</span><span class="pinned-text"></span>`;
  el.pinnedBanner.querySelector(".pinned-text").textContent = preview + countSuffix;
  el.pinnedBanner.style.display = "flex";
  el.pinnedBanner.onclick = () => openPinsDialog(roomId);
}

el.btnSummarize.addEventListener("click", () => {
  closeRoomMenu();
  if (!state.selectedRoom) return;
  openSummaryDialog(state.selectedRoom, null);
});

// =========================================================================
// Room settings (name / topic / avatar)
// =========================================================================
el.btnRoomSettings.addEventListener("click", () => {
  closeRoomMenu();
  if (!state.selectedRoom) return;
  const roomId = state.selectedRoom;
  send("GetRoomInfo", { room_id: roomId }); // refresh — the cached copy may be stale/absent
  openRoomSettingsDialog(roomId);
});

function openRoomSettingsDialog(roomId) {
  const info = state.roomInfo[roomId] || {};
  showDialog(`
    <h3>room settings</h3>
    <label>name</label>
    <input type="text" id="dlg-room-settings-name" value="${escapeHtml(info.name || "")}" ${info.can_set_name ? "" : "disabled"} />
    <label>topic</label>
    <input type="text" id="dlg-room-settings-topic" value="${escapeHtml(info.topic || "")}" ${info.can_set_topic ? "" : "disabled"} />
    <label>avatar</label>
    <div class="checkbox-row">
      ${info.avatar_url ? `<span style="color:var(--text-weak);font-size:11px;">avatar set</span>` : `<span style="color:var(--text-weak);font-size:11px;">no avatar</span>`}
      <button id="dlg-room-settings-avatar-btn" ${info.can_set_avatar ? "" : "disabled"}>change...</button>
      <input type="file" id="dlg-room-settings-avatar-input" accept="image/*" style="display:none;" />
    </div>
    ${info.can_set_name || info.can_set_topic || info.can_set_avatar ? "" : `<p style="color:var(--text-weak);font-size:11px;">you don't have permission to change this room's settings</p>`}
    <div class="actions">
      <button id="dlg-cancel">close</button>
      <button id="dlg-room-settings-save">save</button>
    </div>
  `);
  document.getElementById("dlg-cancel").addEventListener("click", closeDialog);
  document.getElementById("dlg-room-settings-avatar-btn")?.addEventListener("click", () => {
    document.getElementById("dlg-room-settings-avatar-input").click();
  });
  document.getElementById("dlg-room-settings-avatar-input")?.addEventListener("change", async (e) => {
    const file = e.target.files[0];
    if (!file) return;
    const bytes = Array.from(new Uint8Array(await file.arrayBuffer()));
    send("SetRoomAvatar", { room_id: roomId, bytes, mime: file.type || "image/png" });
    showToast("uploading room avatar...");
  });
  document.getElementById("dlg-room-settings-save").addEventListener("click", () => {
    if (info.can_set_name) {
      const name = document.getElementById("dlg-room-settings-name").value.trim();
      if (name !== (info.name || "")) send("SetRoomName", { room_id: roomId, name });
    }
    if (info.can_set_topic) {
      const topic = document.getElementById("dlg-room-settings-topic").value.trim();
      if (topic !== (info.topic || "")) send("SetRoomTopic", { room_id: roomId, topic });
    }
    closeDialog();
  });
}

// =========================================================================
// Pinned messages
// =========================================================================
el.btnPins.addEventListener("click", () => {
  closeRoomMenu();
  if (!state.selectedRoom) return;
  openPinsDialog(state.selectedRoom);
});

/** Returns a cached preview for `eventId` (a `TimelineEvent`, `null` if the
 * server fetch came back empty, or `undefined` if never requested yet —
 * which also kicks off `Command::GetEventPreview` as a side effect, same
 * "ask once, cache, re-render on the answer" pattern `requestImage` uses).
 * For events not already in the loaded timeline (see `findEvent`) — a
 * pinned message being the main case so far, but usable anywhere a bare
 * event ID needs a preview. */
function getEventPreview(roomId, eventId) {
  const key = `${roomId}|${eventId}`;
  if (key in state.eventPreviews) return state.eventPreviews[key];
  state.eventPreviews[key] = undefined;
  send("GetEventPreview", { room_id: roomId, event_id: eventId });
  return undefined;
}

function openPinsDialog(roomId) {
  const eventIds = state.pinnedEvents[roomId] || [];
  const rows = eventIds
    .slice()
    .reverse()
    .map((eventId) => {
      // `findEvent` only sees whatever the timeline has actually paginated
      // in this session — a pinned message is very often well outside
      // that (that's the point of pinning something old). Fall back to
      // fetching it straight from the server via `Command::GetEventPreview`
      // rather than showing the raw `$eventId` forever.
      const ev = findEvent(roomId, eventId) || getEventPreview(roomId, eventId);
      let preview;
      if (ev && ev.msg_type === "image" && ev.media_url) {
        // Same fetch/cache pipeline `renderMessage` uses for an inline
        // image (`state.imageCache`/`requestImage`) — a pinned image
        // showing just its filename ("image.png") isn't a useful preview
        // of *which* pinned photo this is.
        if (ev.media_mime) state.imageMime[ev.media_url] = ev.media_mime;
        if (ev.media_encryption) state.imageEncryption[ev.media_url] = ev.media_encryption;
        const cached = state.imageCache[ev.media_url];
        const senderLabel = `${escapeHtml(ev.sender_name)}: ${escapeHtml(ev.body || "image")}`;
        if (cached) {
          preview = `<div>${senderLabel}</div><img src="${cached}" alt="${escapeHtml(ev.body || "image")}" style="max-width:160px;max-height:160px;display:block;margin-top:6px;border-radius:6px;object-fit:cover;" />`;
        } else {
          preview = `<div>${senderLabel}</div>${loadingHtml("loading image...")}`;
          if (!state.imageRequested.has(ev.media_url)) requestImage(ev.media_url);
        }
      } else if (ev) {
        // `renderMarkdown` (not a plain `escapeHtml`) — same reasoning as
        // `case "Summary"`'s own switch to it: a pinned message keeping
        // its **bold**/`code`/lists intact previews meaningfully better
        // than the raw markdown source showing up as literal asterisks
        // and backticks. Still fully escaped first, same as every other
        // `renderMarkdown` call in this app.
        preview = `<span class="shared-link-sender">${escapeHtml(ev.sender_name)}:</span> ${renderMarkdownPreview(ev.body || "", 100)}`;
      } else if (ev === null) {
        preview = `<span style="color:var(--text-weak);">(message unavailable)</span>`;
      } else {
        preview = loadingHtml("loading...");
      }
      return `<div class="poll-list-item" data-event-id="${escapeHtml(eventId)}">
        <div>${preview}</div>
        <div class="actions" style="margin-top:6px;"><button class="small-btn dlg-unpin" data-event-id="${escapeHtml(eventId)}">unpin</button></div>
      </div>`;
    })
    .join("");
  state.pinnedEventsDialogRoomId = roomId;
  showDialog(`
    <h3>pinned messages</h3>
    ${rows || `<p style="color:var(--text-weak);font-size:12px;">no pinned messages</p>`}
    <div class="actions"><button id="dlg-cancel">close</button></div>
  `);
  document.getElementById("dlg-cancel").addEventListener("click", () => {
    state.pinnedEventsDialogRoomId = null;
    closeDialog();
  });
  document.querySelectorAll(".dlg-unpin").forEach((btn) => {
    btn.addEventListener("click", () => {
      send("UnpinMessage", { room_id: roomId, event_id: btn.dataset.eventId });
      btn.closest(".poll-list-item")?.remove();
    });
  });
}

// =========================================================================
// Polls
// =========================================================================
el.btnPolls.addEventListener("click", () => {
  closeRoomMenu();
  if (!state.selectedRoom) return;
  state.pollsDialogRoomId = state.selectedRoom;
  send("ListPolls", { room_id: state.selectedRoom });
  openPollsDialog(state.selectedRoom, true);
});

function openPollsDialog(roomId, loading) {
  const polls = Object.values(state.polls).filter((p) => p.room_id === roomId);
  const body = loading && polls.length === 0
    ? loadingHtml("loading polls...")
    : polls.length === 0
      ? `<p style="color:var(--text-weak);font-size:12px;">no polls in this room yet</p>`
      : polls.map((p) => `<div class="poll-list-item">${renderPollHtml(p)}</div>`).join("");
  showDialog(`
    <h3>polls</h3>
    <div id="polls-dialog-body">${body}</div>
    <div class="actions"><button id="dlg-cancel">close</button></div>
  `);
  document.getElementById("dlg-cancel").addEventListener("click", () => {
    state.pollsDialogRoomId = null;
    closeDialog();
  });
  wirePollVoteHandlers(document.getElementById("polls-dialog-body"), roomId);
}

/** Poll question + one row per option, a filled bar behind the text
 * showing that option's current vote share. Shared by the polls-list
 * dialog and (via `renderMessage`-adjacent code, if a poll's start event
 * happens to already be loaded in the visible timeline) nowhere else
 * currently — polls aren't part of the live timeline stream, see
 * `Command::ListPolls`'s doc comment on the Rust side for why. */
function renderPollHtml(poll) {
  const totalForBars = Math.max(poll.total_votes, 1);
  const options = poll.options
    .map((opt) => {
      const pct = Math.round((opt.votes / totalForBars) * 100);
      const voted = poll.my_vote_ids.includes(opt.id);
      return `<div class="poll-option${voted ? " voted" : ""}" data-poll-id="${escapeHtml(poll.poll_event_id)}" data-answer-id="${escapeHtml(opt.id)}">
        <div class="poll-option-fill" style="width:${pct}%;"></div>
        <div class="poll-option-label"><span>${voted ? "✓ " : ""}${escapeHtml(opt.text)}</span><span>${opt.votes} (${pct}%)</span></div>
      </div>`;
    })
    .join("");
  const endBtn = poll.ended
    ? ""
    : `<button class="small-btn dlg-end-poll" data-poll-id="${escapeHtml(poll.poll_event_id)}">end poll</button>`;
  return `<div class="poll${poll.ended ? " ended" : ""}">
    <div class="poll-question">${escapeHtml(poll.question)}${poll.ended ? " (ended)" : ""}</div>
    ${options}
    <div class="poll-meta"><span>${poll.total_votes} vote${poll.total_votes === 1 ? "" : "s"}</span>${endBtn}</div>
  </div>`;
}

function wirePollVoteHandlers(containerEl, roomId) {
  if (!containerEl) return;
  containerEl.querySelectorAll(".poll-option").forEach((optEl) => {
    optEl.addEventListener("click", () => {
      const poll = state.polls[optEl.dataset.pollId];
      if (poll?.ended) return;
      send("VotePoll", {
        room_id: roomId,
        poll_event_id: optEl.dataset.pollId,
        answer_ids: [optEl.dataset.answerId],
      });
    });
  });
  containerEl.querySelectorAll(".dlg-end-poll").forEach((btn) => {
    btn.addEventListener("click", () => {
      send("EndPoll", { room_id: roomId, poll_event_id: btn.dataset.pollId });
    });
  });
}

function openCreatePollDialog() {
  if (!state.selectedRoom) return;
  const roomId = state.selectedRoom;
  showDialog(`
    <h3>new poll</h3>
    <label>question</label>
    <input type="text" id="dlg-poll-question" />
    <label>options (one per line, 2-20)</label>
    <textarea id="dlg-poll-options" rows="5" style="width:100%;margin-top:2px;"></textarea>
    <div class="checkbox-row"><input type="checkbox" id="dlg-poll-multi" /><label for="dlg-poll-multi">allow selecting more than one option</label></div>
    <div class="actions">
      <button id="dlg-cancel">cancel</button>
      <button id="dlg-poll-create">create</button>
    </div>
  `);
  document.getElementById("dlg-cancel").addEventListener("click", closeDialog);
  document.getElementById("dlg-poll-create").addEventListener("click", () => {
    const question = document.getElementById("dlg-poll-question").value.trim();
    const options = document.getElementById("dlg-poll-options").value
      .split("\n")
      .map((s) => s.trim())
      .filter(Boolean);
    if (!question || options.length < 2) {
      showToast("a poll needs a question and at least 2 options");
      return;
    }
    const maxSelections = document.getElementById("dlg-poll-multi").checked ? options.length : 1;
    send("StartPoll", { room_id: roomId, thread_id: null, question, options, max_selections: maxSelections });
    closeDialog();
  });
}

// =========================================================================
// Full-text message search (across all rooms, by content)
// =========================================================================
el.btnMessageSearch.addEventListener("click", () => {
  closeChatsMenu();
  showDialog(`
    <h3>search messages</h3>
    <label>text to find</label>
    <input type="text" id="dlg-msg-search-query" />
    <div class="actions">
      <button id="dlg-cancel">cancel</button>
      <button id="dlg-msg-search-go">search</button>
    </div>
    <div id="msg-search-results" class="user-search-list"></div>
  `);
  document.getElementById("dlg-cancel").addEventListener("click", closeDialog);
  const runSearch = () => {
    const query = document.getElementById("dlg-msg-search-query").value.trim();
    if (!query) return;
    document.getElementById("msg-search-results").innerHTML = loadingHtml("searching...");
    send("SearchMessages", { query, room_id: null, from_ts: null, to_ts: null });
  };
  document.getElementById("dlg-msg-search-go").addEventListener("click", runSearch);
  document.getElementById("dlg-msg-search-query").addEventListener("keydown", (e) => {
    if (e.key === "Enter") runSearch();
  });
  document.getElementById("dlg-msg-search-query").focus();
});

function renderMessageSearchResults(results, truncated) {
  const container = document.getElementById("msg-search-results");
  if (!container) return;
  if (results.length === 0) {
    container.innerHTML = `<div style="padding:8px;color:var(--text-weak);font-size:12px;">no matches</div>`;
    return;
  }
  container.innerHTML = results
    .map(
      (hit) => `<div class="thread-row" data-room-id="${escapeHtml(hit.room_id)}" data-event-id="${escapeHtml(hit.event.event_id)}">
        ${avatarHtml(hit.event.sender_avatar_url, hit.event.sender_name, hit.event.sender, 28)}
        <div class="thread-row-content">
          <div class="thread-room-name">${escapeHtml(hit.room_name)}</div>
          <div class="sender">${escapeHtml(hit.event.sender_name)}</div>
          <div class="thread-row-body">${renderMarkdownPreview(hit.event.body || "", 160)}</div>
        </div>
      </div>`
    )
    .join("") + (truncated ? `<div style="padding:6px;color:var(--text-weak);font-size:11px;">results truncated — narrow your search</div>` : "");
  container.querySelectorAll(".thread-row").forEach((rowEl) => {
    rowEl.addEventListener("click", () => {
      closeDialog();
      openMatrixToLink(rowEl.dataset.roomId, rowEl.dataset.eventId, null);
    });
  });
}

// =========================================================================
// Voice messages
// =========================================================================
async function toggleVoiceRecording() {
  if (!state.selectedRoom) return;
  if (state.voiceRecording) {
    state.voiceRecording.recorder.stop();
    return;
  }
  let stream;
  try {
    stream = await navigator.mediaDevices.getUserMedia({ audio: true });
  } catch (e) {
    showToast("microphone access denied");
    return;
  }
  const chunks = [];
  const recorder = new MediaRecorder(stream);
  const startedAt = Date.now();
  // A coarse waveform for the MSC3245 "voice message" playback UI —
  // sampled from the live input level once per animation frame while
  // recording, not decoded from the final encoded audio (getting exact
  // per-sample amplitudes back out of a compressed webm/opus blob without
  // pulling in a decoding library isn't worth it just for a preview
  // waveform other clients render, not this one).
  const audioCtx = new (window.AudioContext || window.webkitAudioContext)();
  const source = audioCtx.createMediaStreamSource(stream);
  const analyser = audioCtx.createAnalyser();
  analyser.fftSize = 256;
  source.connect(analyser);
  const levels = [];
  const dataArray = new Uint8Array(analyser.frequencyBinCount);
  let sampling = true;
  const sampleLoop = () => {
    if (!sampling) return;
    analyser.getByteTimeDomainData(dataArray);
    let peak = 0;
    for (let i = 0; i < dataArray.length; i++) peak = Math.max(peak, Math.abs(dataArray[i] - 128));
    levels.push(peak / 128);
    requestAnimationFrame(sampleLoop);
  };
  sampleLoop();

  recorder.addEventListener("dataavailable", (e) => {
    if (e.data.size > 0) chunks.push(e.data);
  });
  recorder.addEventListener("stop", async () => {
    sampling = false;
    stream.getTracks().forEach((t) => t.stop());
    audioCtx.close().catch(() => {});
    el.btnComposePlus.classList.remove("recording");
    const durationMs = Date.now() - startedAt;
    state.voiceRecording = null;
    if (durationMs < 500) return; // accidental tap, not a real recording
    const blob = new Blob(chunks, { type: recorder.mimeType || "audio/ogg" });
    const bytes = Array.from(new Uint8Array(await blob.arrayBuffer()));
    // Downsample the raw per-frame levels to a fixed small count (MSC3245
    // waveforms are meant to be compact, ~100 points regardless of clip
    // length) via simple bucket-averaging.
    const bucketCount = Math.min(100, Math.max(1, levels.length));
    const waveform = [];
    for (let i = 0; i < bucketCount; i++) {
      const start = Math.floor((i * levels.length) / bucketCount);
      const end = Math.max(start + 1, Math.floor(((i + 1) * levels.length) / bucketCount));
      const slice = levels.slice(start, end);
      waveform.push(slice.reduce((a, b) => a + b, 0) / (slice.length || 1));
    }
    send("SendVoiceMessage", {
      room_id: state.selectedRoom,
      // The voice-record button only exists on the main compose row (not
      // the thread panel's own copy) — see its doc comment — so this
      // always targets the main timeline, same as `sendCurrentMessage`'s
      // own `thread_id: null` case.
      thread_id: null,
      bytes,
      mime: recorder.mimeType || "audio/ogg",
      duration_ms: durationMs,
      waveform,
      local_id: crypto.randomUUID(),
    });
  });

  state.voiceRecording = { recorder, audioCtx };
  el.btnComposePlus.classList.add("recording");
  recorder.start();
}

// =========================================================================
// Theme override (system, plus a curated set of named palettes) — see
// style.css's comment on the (deliberately empty) light `@media` block for
// why this lives here as inline custom-property overrides instead.
// =========================================================================
/** One entry per selectable theme, `id: null` reserved for "system" (drops
 * every override below and lets `Event::SystemTheme`/the plain `:root`
 * defaults take back over — see `applyThemeOverride`). `"dark"` has no
 * palette of its own for the same reason: it just *is* this app's own
 * `:root` defaults (the terminal-tool amber-on-near-black look), so
 * there's nothing to override back to. Every other theme is a genuine,
 * named palette (not GTK-driven), so unlike the old light-only override
 * these also set `--accent-strong`/`--border`/etc. explicitly enough to
 * look coherent rather than just inverted. Colors are each theme's own
 * well-known published values, not approximated. */
const THEME_PALETTES = {
  light: {
    bg: "#f7f5f0", bgAlt: "#ffffff", border: "#d9d3c7",
    text: "#2a2620", textWeak: "#6e675c", accent: "#b5791c", accentStrong: "#8f5f12",
    colorScheme: "light",
  },
  dracula: {
    bg: "#282a36", bgAlt: "#343746", border: "#44475a",
    text: "#f8f8f2", textWeak: "#6272a4", accent: "#bd93f9", accentStrong: "#ff79c6",
    colorScheme: "dark",
  },
  nord: {
    bg: "#2e3440", bgAlt: "#3b4252", border: "#4c566a",
    text: "#e5e9f0", textWeak: "#81a1c1", accent: "#88c0d0", accentStrong: "#5e81ac",
    colorScheme: "dark",
  },
  gruvbox: {
    bg: "#282828", bgAlt: "#3c3836", border: "#504945",
    text: "#ebdbb2", textWeak: "#a89984", accent: "#fabd2f", accentStrong: "#d79921",
    colorScheme: "dark",
  },
  "solarized-dark": {
    bg: "#002b36", bgAlt: "#073642", border: "#586e75",
    text: "#93a1a1", textWeak: "#657b83", accent: "#268bd2", accentStrong: "#2aa198",
    colorScheme: "dark",
  },
  "solarized-light": {
    bg: "#fdf6e3", bgAlt: "#eee8d5", border: "#c9c2ab",
    text: "#586e75", textWeak: "#839496", accent: "#268bd2", accentStrong: "#cb4b16",
    colorScheme: "light",
  },
  "one-dark": {
    bg: "#282c34", bgAlt: "#2c313a", border: "#3e4451",
    text: "#abb2bf", textWeak: "#5c6370", accent: "#61afef", accentStrong: "#c678dd",
    colorScheme: "dark",
  },
  monokai: {
    bg: "#272822", bgAlt: "#3e3d32", border: "#49483e",
    text: "#f8f8f2", textWeak: "#8d8a7d", accent: "#a6e22e", accentStrong: "#f92672",
    colorScheme: "dark",
  },
  "tokyo-night": {
    bg: "#1a1b26", bgAlt: "#24283b", border: "#414868",
    text: "#c0caf5", textWeak: "#7982a9", accent: "#7aa2f7", accentStrong: "#bb9af7",
    colorScheme: "dark",
  },
  "catppuccin-mocha": {
    bg: "#1e1e2e", bgAlt: "#313244", border: "#45475a",
    text: "#cdd6f4", textWeak: "#a6adc8", accent: "#89b4fa", accentStrong: "#f5c2e7",
    colorScheme: "dark",
  },
  "high-contrast": {
    bg: "#000000", bgAlt: "#1a1a1a", border: "#ffffff",
    text: "#ffffff", textWeak: "#d8d8d8", accent: "#ffff00", accentStrong: "#00ffff",
    colorScheme: "dark",
  },
};
const THEME_OPTIONS = [
  { id: null, label: "System" },
  { id: "dark", label: "Dark (default)" },
  { id: "light", label: "Light" },
  { id: "dracula", label: "Dracula" },
  { id: "nord", label: "Nord" },
  { id: "gruvbox", label: "Gruvbox Dark" },
  { id: "solarized-dark", label: "Solarized Dark" },
  { id: "solarized-light", label: "Solarized Light" },
  { id: "one-dark", label: "One Dark" },
  { id: "monokai", label: "Monokai" },
  { id: "tokyo-night", label: "Tokyo Night" },
  { id: "catppuccin-mocha", label: "Catppuccin Mocha" },
  { id: "high-contrast", label: "High Contrast" },
];
const THEME_VARS = ["--bg", "--bg-alt", "--border", "--text", "--text-weak", "--accent", "--accent-strong"];
function applyThemeOverride() {
  const root = document.documentElement.style;
  const palette = THEME_PALETTES[state.themeOverride];
  if (palette) {
    root.setProperty("--bg", palette.bg);
    root.setProperty("--bg-alt", palette.bgAlt);
    root.setProperty("--border", palette.border);
    root.setProperty("--text", palette.text);
    root.setProperty("--text-weak", palette.textWeak);
    root.setProperty("--accent", palette.accent);
    root.setProperty("--accent-strong", palette.accentStrong);
    document.documentElement.style.colorScheme = palette.colorScheme;
  } else {
    // "dark" (this app's own `:root` defaults, nothing to override) or
    // `null`/"system" (drop any override and let the next
    // `Event::SystemTheme` — Linux/GTK — or, absent that, those same
    // plain `:root` defaults take over again).
    THEME_VARS.forEach((v) => root.removeProperty(v));
    document.documentElement.style.colorScheme = "dark";
  }
  const current = THEME_OPTIONS.find((t) => t.id === state.themeOverride);
  el.btnTheme.textContent = `[ theme: ${current ? current.label : "system"} ]`;
}
function persistThemeOverride() {
  try {
    if (state.themeOverride) localStorage.setItem("themeOverride", state.themeOverride);
    else localStorage.removeItem("themeOverride");
  } catch (e) {
    // Private-browsing-style storage block — the override just won't
    // survive a restart, nothing else depends on it persisting.
  }
}
el.btnTheme.addEventListener("click", () => {
  showDialog(`
    <h3>theme</h3>
    <label>theme</label>
    <select id="dlg-theme">
      ${THEME_OPTIONS.map(
        (t) =>
          `<option value="${t.id ? escapeHtml(t.id) : ""}"${t.id === state.themeOverride ? " selected" : ""}>${escapeHtml(t.label)}</option>`
      ).join("")}
    </select>
    <div class="actions">
      <button id="dlg-cancel">close</button>
    </div>
  `);
  document.getElementById("dlg-theme").addEventListener("change", (e) => {
    state.themeOverride = e.target.value || null;
    applyThemeOverride();
    persistThemeOverride();
  });
  document.getElementById("dlg-cancel").addEventListener("click", closeDialog);
});
try {
  const saved = localStorage.getItem("themeOverride");
  if (THEME_OPTIONS.some((t) => t.id === saved)) state.themeOverride = saved;
} catch (e) {
  // same as above
}
applyThemeOverride();

/** Curated rather than a free-text field: a font name that doesn't exist
 * on the OS just silently falls through to the browser default with no
 * feedback, so picking from a list of names actually likely to be
 * installed (mirroring what `renderMarkdown`'s `<code>` styling and the
 * rest of this app's CSS already assume) avoids a picker that quietly
 * does nothing for most typed input. Monospace options keep the
 * terminal-tool look this app defaults to; the rest are for anyone who'd
 * rather it read like a normal chat app. Each still carries a generic
 * fallback family so an OS missing that exact face doesn't fall back all
 * the way to serif. */
const FONT_FAMILY_OPTIONS = [
  { label: "Inter (default)", value: null },
  { label: "Noto Sans", value: '"Noto Sans", sans-serif' },
  { label: "DejaVu Sans", value: '"DejaVu Sans", sans-serif' },
  { label: "Liberation Sans", value: '"Liberation Sans", sans-serif' },
  { label: "Roboto", value: '"Roboto", sans-serif' },
  { label: "Segoe UI", value: '"Segoe UI", sans-serif' },
  { label: "Helvetica", value: '"Helvetica", "Arial", sans-serif' },
  { label: "System default", value: "system-ui, sans-serif" },
  { label: "JetBrains Mono (monospace)", value: '"JetBrains Mono", monospace' },
  { label: "DejaVu Sans Mono (monospace)", value: '"DejaVu Sans Mono", monospace' },
  { label: "Noto Sans Mono (monospace)", value: '"Noto Sans Mono", monospace' },
  { label: "Liberation Mono (monospace)", value: '"Liberation Mono", monospace' },
  { label: "Ubuntu Mono (monospace)", value: '"Ubuntu Mono", monospace' },
  { label: "Cascadia Code (monospace)", value: '"Cascadia Code", monospace' },
  { label: "Fira Code (monospace)", value: '"Fira Code", monospace' },
  { label: "Consolas (monospace)", value: '"Consolas", monospace' },
  { label: "Menlo (monospace)", value: '"Menlo", monospace' },
  { label: "SF Mono (monospace)", value: '"SF Mono", monospace' },
  { label: "Roboto Mono (monospace)", value: '"Roboto Mono", monospace' },
  { label: "IBM Plex Mono (monospace)", value: '"IBM Plex Mono", monospace' },
  { label: "Source Code Pro (monospace)", value: '"Source Code Pro", monospace' },
  { label: "Space Mono (monospace)", value: '"Space Mono", monospace' },
  { label: "Inconsolata (monospace)", value: '"Inconsolata", monospace' },
  { label: "Courier New (monospace)", value: '"Courier New", monospace' },
];
const FONT_SIZE_MIN = 12;
const FONT_SIZE_MAX = 22;
const FONT_SIZE_DEFAULT = 15; // must match `--font-size` in style.css

/** Same `root.style.setProperty`/`removeProperty` shape as
 * `applyThemeOverride()` above — a `null` state value drops the override
 * and lets `:root`'s plain CSS default (style.css) take back over. */
function applyFontSettings() {
  const root = document.documentElement.style;
  if (state.fontFamily) root.setProperty("--font-family", state.fontFamily);
  else root.removeProperty("--font-family");
  if (state.fontSize) root.setProperty("--font-size", `${state.fontSize}px`);
  else root.removeProperty("--font-size");
}
try {
  const savedFamily = localStorage.getItem("fontFamily");
  if (savedFamily && FONT_FAMILY_OPTIONS.some((o) => o.value === savedFamily)) {
    state.fontFamily = savedFamily;
  }
  const savedSize = parseInt(localStorage.getItem("fontSize"), 10);
  if (Number.isFinite(savedSize) && savedSize >= FONT_SIZE_MIN && savedSize <= FONT_SIZE_MAX) {
    state.fontSize = savedSize;
  }
} catch (e) {
  // Private-browsing-style storage block, same as the theme override above.
}
applyFontSettings();

el.btnFontSettings.addEventListener("click", () => {
  showDialog(`
    <h3>font</h3>
    <label>font family</label>
    <select id="dlg-font-family">
      ${FONT_FAMILY_OPTIONS.map(
        (o) =>
          `<option value="${o.value ? escapeHtml(o.value) : ""}"${o.value === state.fontFamily ? " selected" : ""}>${escapeHtml(o.label)}</option>`
      ).join("")}
    </select>
    <label>font size (${FONT_SIZE_MIN}-${FONT_SIZE_MAX}px)</label>
    <input type="number" id="dlg-font-size" min="${FONT_SIZE_MIN}" max="${FONT_SIZE_MAX}" value="${state.fontSize || FONT_SIZE_DEFAULT}" />
    <div class="actions">
      <button id="dlg-font-reset">reset to default</button>
      <button id="dlg-font-save">save</button>
      <button id="dlg-cancel">close</button>
    </div>
  `);
  const familySel = document.getElementById("dlg-font-family");
  const sizeInput = document.getElementById("dlg-font-size");
  const persist = () => {
    try {
      if (state.fontFamily) localStorage.setItem("fontFamily", state.fontFamily);
      else localStorage.removeItem("fontFamily");
      if (state.fontSize) localStorage.setItem("fontSize", String(state.fontSize));
      else localStorage.removeItem("fontSize");
    } catch (e) {
      // same as above — the change still applies for this session
    }
  };
  familySel.addEventListener("change", () => {
    state.fontFamily = familySel.value || null;
    applyFontSettings();
    persist();
  });
  sizeInput.addEventListener("input", () => {
    const n = parseInt(sizeInput.value, 10);
    if (!Number.isFinite(n) || n < FONT_SIZE_MIN || n > FONT_SIZE_MAX) return;
    state.fontSize = n;
    applyFontSettings();
    persist();
  });
  document.getElementById("dlg-font-reset").addEventListener("click", () => {
    state.fontFamily = null;
    state.fontSize = null;
    applyFontSettings();
    persist();
    familySel.value = "";
    sizeInput.value = FONT_SIZE_DEFAULT;
  });
  document.getElementById("dlg-font-save").addEventListener("click", () => {
    // Everything above already applies + persists live on every change —
    // this button doesn't do anything the live updates haven't already
    // done, it just gives an explicit "I'm done, this is saved" action to
    // click instead of only "close", so the choice reads as confirmed
    // rather than dismissed.
    persist();
    closeDialog();
  });
  document.getElementById("dlg-cancel").addEventListener("click", closeDialog);
});

/** Opens the "summarizing..." popup and kicks off `Command::Summarize`
 * for the main room timeline (`threadRootId: null`) or one open thread
 * (`threadRootId` = its root event id) — shared by the room menu's
 * `[ summarize ]` button and the thread panel's own. */
function openSummaryDialog(roomId, threadRootId) {
  state.summaryRequest = { roomId, threadRootId };
  // Says which one this is — room or thread — and which room, so the
  // popup itself makes clear what's being summarized instead of a bare
  // "summary" that reads identically either way (easy to lose track of
  // after opening one, then the other, back to back).
  const roomName = state.rooms.find((r) => r.room_id === roomId)?.name || roomId;
  const scopeLabel = threadRootId ? `thread in ${roomName}` : roomName;
  showDialog(`
    <h3>summary — ${escapeHtml(scopeLabel)}</h3>
    <div id="summary-dialog-body" style="max-height:65vh;overflow-y:auto;line-height:1.5;">${loadingHtml("summarizing...")}</div>
    <div class="actions"><button id="dlg-cancel">close</button></div>
  `);
  // `.dialog-box`'s own CSS caps it at 320px — comfortable for the short
  // forms every other dialog in this app uses, but far too narrow for a
  // multi-paragraph summary (a wall of single-word-wide lines). Widened
  // just for this one, `max-width: 100%` from that same CSS still keeps
  // it from overflowing a narrower window.
  const box = document.querySelector("#active-dialog .dialog-box");
  if (box) box.style.width = "640px";
  document.getElementById("dlg-cancel").addEventListener("click", closeDialog);
  send("Summarize", { room_id: roomId, thread_root_id: threadRootId });
}

// =========================================================================
// Timeline rendering
// =========================================================================

/** Scrolls `container` to its bottom, correcting for `.msg-row`'s
 * `content-visibility: auto` (see style.css) — its rows still off-screen
 * at the moment this runs are sized by their `contain-intrinsic-size`
 * *estimate*, not their real rendered height, since the browser hasn't
 * determined which of them are actually in view yet (that only happens
 * during layout/paint, not synchronously in this script). Reading
 * `scrollHeight` and setting `scrollTop` from it right after appending a
 * bunch of rows therefore targets a value that's shorter than the true
 * final height whenever real content is taller than the estimate (a
 * multi-line message, an image, a grouped block, ...) — reliably leaving
 * the last message or two just out of view, needing an extra manual
 * scroll to actually reach them. Setting `scrollTop` again a couple of
 * frames later, once layout has caught up, closes that gap — imperceptible
 * since it's already close, just not exact. */
function scrollToBottom(container) {
  container.scrollTop = container.scrollHeight;
  scrollAnchorFor(container)?.stickToBottom();
  requestAnimationFrame(() => {
    requestAnimationFrame(() => {
      container.scrollTop = container.scrollHeight;
    });
  });
}

/** Manual scroll anchoring for a message list — keeps whichever message
 * the user is reading pinned at the same on-screen position whenever
 * layout *above* it changes: an older page prepended by "load more", an
 * image/avatar/link preview above the viewport finishing its load (which
 * `rerenderMessageInPlace` swaps in with a different height), the "load
 * more" row flipping to "loading...". Or, if the list was scrolled all
 * the way down, keeps it there. Browsers' own CSS scroll anchoring
 * (`overflow-anchor`) would do this, but WebKitGTK — this app's desktop
 * webview — doesn't implement it at all, so every one of those layout
 * changes used to visibly shove the timeline around after the one-off
 * scroll fixup in `prependMessages` had already run. It's turned off in
 * style.css for these containers too, so Android's Chromium WebView
 * doesn't adjust a second time on top of this.
 *
 * `sync()` undoes any layout shift since the last sync, then re-records
 * the anchor. It runs from a `ResizeObserver` on each `.msg-row` (after
 * layout, before paint, so the correction never shows up as a visible
 * jump), on every scroll, and around programmatic inserts. Never just
 * re-recording without correcting first matters: a scroll landing in the
 * same frame as an image swap would otherwise bake that shift in.
 * `getContainer` returns the current scroll container (or `null`) since
 * the thread panel's `#side-panel-body` is recreated on every full
 * `renderSidePanel()`; `rootEl` is a stable ancestor to watch for rows
 * being added/removed. */
const scrollAnchors = [];
function scrollAnchorFor(container) {
  return scrollAnchors.find((a) => a.container() === container) || null;
}
function createScrollAnchor(getContainer, rootEl) {
  const OBSERVED = ".msg-row, #load-more-row";
  let capturedFor = null;
  let capturedScrollTop = 0;
  let atBottom = true;
  let anchorEventId = null;
  let anchorOffset = 0;

  const resolveContainer = () => {
    const c = getContainer();
    return c && c.isConnected ? c : null;
  };

  /** First `.msg-item` whose bottom edge is below the container's top —
   * binary search, since items are in document (= vertical) order and a
   * long timeline can hold thousands of them. */
  const firstVisibleItem = (c, top) => {
    const items = c.getElementsByClassName("msg-item");
    let lo = 0;
    let hi = items.length - 1;
    let found = null;
    while (lo <= hi) {
      const mid = (lo + hi) >> 1;
      if (items[mid].getBoundingClientRect().bottom > top) {
        found = items[mid];
        hi = mid - 1;
      } else {
        lo = mid + 1;
      }
    }
    return found;
  };

  const findAnchorEl = (c) =>
    anchorEventId ? c.querySelector(`.msg-item[data-event-id="${CSS.escape(anchorEventId)}"]`) : null;

  const capture = () => {
    const c = resolveContainer();
    capturedFor = c;
    anchorEventId = null;
    if (!c) return;
    capturedScrollTop = c.scrollTop;
    atBottom = c.scrollHeight - c.scrollTop - c.clientHeight <= 2;
    if (atBottom) return;
    const top = c.getBoundingClientRect().top;
    const item = firstVisibleItem(c, top);
    if (!item) return;
    anchorEventId = item.dataset.eventId;
    anchorOffset = item.getBoundingClientRect().top - top;
  };

  const sync = () => {
    const c = resolveContainer();
    if (!c || c !== capturedFor) {
      capture();
      return;
    }
    if (atBottom) {
      // Something else deliberately scrolled away from the bottom (a
      // jump-to-message `scrollIntoView`, ...) before the scroll
      // listener got to recapture — respect that rather than yanking
      // back down.
      if (Math.abs(c.scrollTop - capturedScrollTop) > 1) {
        capture();
        return;
      }
      c.scrollTop = c.scrollHeight;
    } else {
      const anchorEl = findAnchorEl(c);
      if (anchorEl) {
        // Pure scrolling moves the anchor by exactly the scroll delta;
        // anything beyond that is a layout shift above it to undo.
        const expected = anchorOffset - (c.scrollTop - capturedScrollTop);
        const actual = anchorEl.getBoundingClientRect().top - c.getBoundingClientRect().top;
        const delta = actual - expected;
        if (Math.abs(delta) >= 1) c.scrollTop += delta;
      }
    }
    capture();
  };

  const resizeObserver = new ResizeObserver(sync);
  const forEachObserved = (node, fn) => {
    if (node.nodeType !== Node.ELEMENT_NODE) return;
    if (node.matches(OBSERVED)) fn(node);
    for (const child of node.querySelectorAll(OBSERVED)) fn(child);
  };
  new MutationObserver((mutations) => {
    for (const m of mutations) {
      for (const n of m.removedNodes) forEachObserved(n, (x) => resizeObserver.unobserve(x));
      for (const n of m.addedNodes) forEachObserved(n, (x) => resizeObserver.observe(x));
    }
  }).observe(rootEl, { childList: true, subtree: true });
  forEachObserved(rootEl, (x) => resizeObserver.observe(x));

  // Capture phase, so this also sees scrolls of a descendant container
  // (scroll events don't bubble) that gets recreated under `rootEl`.
  let rafPending = false;
  rootEl.addEventListener(
    "scroll",
    () => {
      if (rafPending) return;
      rafPending = true;
      requestAnimationFrame(() => {
        rafPending = false;
        sync();
      });
    },
    true,
  );

  const anchor = {
    container: resolveContainer,
    sync,
    stickToBottom() {
      const c = resolveContainer();
      capturedFor = c;
      anchorEventId = null;
      atBottom = true;
      if (c) capturedScrollTop = c.scrollTop;
    },
  };
  scrollAnchors.push(anchor);
  return anchor;
}

const timelineScrollAnchor = createScrollAnchor(() => el.timeline, el.timeline);
const threadScrollAnchor = createScrollAnchor(
  () => (state.rightPanel?.kind === "thread" ? document.getElementById("side-panel-body") : null),
  el.sidePanel,
);

function renderTimeline() {
  const events = state.timelines[state.selectedRoom] || [];
  el.timeline.innerHTML = "";

  if (!state.reachedStart.has(state.selectedRoom) && events.length > 0) {
    const row = document.createElement("div");
    row.id = "load-more-row";
    if (state.paginationInFlight.has(state.selectedRoom)) {
      row.innerHTML = loadingHtml("loading older messages...");
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
  const ctx = { roomId: state.selectedRoom, threadId: null };
  for (const run of groupIntoRuns(events)) {
    el.timeline.appendChild(renderMessageGroup(run, ctx));
  }
  scrollToBottom(el.timeline);
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

/** Builds the optimistic placeholder shown the instant Send is pressed —
 * same shape as a real `TimelineEvent` from the backend, so it flows
 * through `renderMessageItem`/`renderMessageGroup`/grouping exactly like
 * one, just tagged `_sendStatus: "sending"` for the "· sending..." meta
 * suffix. Reconciled away once either `MessageSent`/`MessageSendFailed`
 * or (more usually first) the live echo carrying the same `local_id`
 * arrives — see `state.pendingSends` and the `NewMessage`/`ThreadReply`
 * handlers. Returns `null` when `state.ownProfile` hasn't loaded yet
 * (right after login, before `Command::GetOwnProfile` answers) — sending
 * still works, it just falls back to waiting for the real echo like
 * before, rather than showing a bubble with no name/avatar. */
function makeOptimisticEvent(localId, body, replyToEventId, mentionedUserIds) {
  const profile = state.ownProfile;
  if (!profile) return null;
  return {
    event_id: `local:${localId}`,
    local_id: localId,
    sender: profile.user_id,
    sender_name: profile.display_name,
    sender_avatar_url: profile.avatar_url,
    body,
    msg_type: "text",
    media_url: null,
    media_mime: null,
    media_encryption: null,
    thumbnail_url: null,
    timestamp: Date.now(),
    thread_count: null,
    is_own: true,
    reply_to_event_id: replyToEventId,
    reply_to_preview: null,
    mentions_me: false,
    mentioned_user_ids: mentionedUserIds,
    reactions: [],
    read_by: [],
    latest_reply_sender_name: null,
    latest_reply_body: null,
    latest_reply_ts: null,
    latest_reply_event_id: null,
    latest_reply_mentions_me: false,
    latest_reply_msg_type: null,
    latest_reply_media_url: null,
    latest_reply_media_mime: null,
    latest_reply_media_encryption: null,
    is_unread: null,
    _sendStatus: "sending",
  };
}

/** Swaps a still-pending optimistic placeholder (`event_id: "local:<id>"`,
 * see `makeOptimisticEvent`) for the real event that just arrived carrying
 * the same `local_id` back on `unsigned.transaction_id` — in both the
 * backing `events` array and, if it's actually on screen right now, the
 * rendered DOM row, without disturbing anything else in the timeline.
 * Returns `false` (caller falls back to just appending normally) when
 * there's no matching placeholder left to swap — e.g. `MessageSendFailed`
 * already turned it into a "failed to send" row instead. */
function reconcileLocalEcho(events, containerEl, localId, realEvent, ctx) {
  const placeholderId = `local:${localId}`;
  const idx = events.findIndex((e) => e.event_id === placeholderId);
  if (idx === -1) return false;
  events[idx] = realEvent;
  const itemEl = containerEl?.querySelector(`.msg-item[data-event-id="${CSS.escape(placeholderId)}"]`);
  if (itemEl) itemEl.replaceWith(renderMessageItem(realEvent, ctx));
  return true;
}

/** Re-renders exactly the DOM `.msg-item` for one event, in place — no
 * `renderTimeline()` teardown, so the rest of the timeline (and wherever
 * the user has scrolled to) is left completely untouched. Used for
 * edits/deletes and an image finishing its download, both of which used
 * to force a full rebuild-and-jump-to-bottom for a change to a single
 * message. Only the one message's own content needs rebuilding — which
 * run it's grouped into never changes from an edit/delete/image-load, so
 * unlike the old per-row version this doesn't need to recompute grouping
 * at all. Falls back to a full render if the item isn't there to patch
 * (shouldn't normally happen — caller already checked the room matches). */
function rerenderMessageInPlace(roomId, eventId) {
  if (roomId !== state.selectedRoom) return;
  const events = state.timelines[roomId] || [];
  const event = events.find((e) => e.event_id === eventId);
  if (!event) return;
  const itemEl = el.timeline.querySelector(`.msg-item[data-event-id="${CSS.escape(eventId)}"]`);
  if (!itemEl) {
    renderTimeline();
    return;
  }
  itemEl.replaceWith(renderMessageItem(event, { roomId, threadId: null }));
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
  const ctx = { roomId, threadId: null };
  const grouped = isGrouped(events[events.length - 2], event);
  const lastRow = grouped ? el.timeline.lastElementChild : null;
  const lastBubble = lastRow?.classList.contains("msg-row") ? lastRow.querySelector(".bubble") : null;
  if (lastBubble) {
    lastBubble.appendChild(renderMessageItem(event, ctx));
  } else {
    el.timeline.appendChild(renderMessageGroup([event], ctx));
  }
  if (wasNearBottom) scrollToBottom(el.timeline);
  updateJumpLatestVisibility();
}

/** Inserts older messages (from a `TimelinePrepend`) at the top instead of
 * tearing down and rebuilding the whole timeline the way a plain
 * `renderTimeline()` call does. That full rebuild was the actual source of
 * the jarring scroll jump on "load more": it destroys and recreates every
 * already-rendered row too, including ones with images that had already
 * finished loading — those images then have to reload from scratch, and
 * each one popping back in shifts the layout (and the scroll position)
 * again, *after* the scroll-position fixup below already ran. Leaving
 * existing rows untouched avoids all of that. `prevEvents`/`allEvents` are
 * the timeline's event list just before/after this page loaded — the new
 * messages are exactly `allEvents`'s extra prefix over `prevEvents`. */
function prependMessages(roomId, prevEvents, allEvents) {
  const newCount = allEvents.length - prevEvents.length;
  const loadMoreRow = document.getElementById("load-more-row");
  // Falls back to a full render for anything this incremental path isn't
  // built to handle correctly (no new events despite being called, or no
  // rows to insert relative to — e.g. this was the very first page).
  if (newCount <= 0 || prevEvents.length === 0 || !loadMoreRow) {
    renderTimeline();
    return;
  }

  const newEvents = allEvents.slice(0, newCount);
  // Pins the message currently at the top of the view (or the bottom, if
  // scrolled all the way down — which is also the case right after
  // opening a room whose first page doesn't fill the viewport yet, while
  // `maybeAutoLoadMore` keeps calling this to fill it). Restored below,
  // and kept pinned afterwards as the new rows' images etc. finish
  // loading — see `createScrollAnchor`.
  timelineScrollAnchor.sync();

  // The new page's last (most recent) event may group with the room's
  // previous first event (same sender, close in time — common, since
  // that's exactly what pagination boundaries land on mid-conversation).
  // Rather than falling back to a full `renderTimeline()` for that case —
  // which used to force the scroll position back to the bottom, the very
  // "jump to newest message on load more" this function exists to avoid —
  // fold it into the existing first row's bubble below instead, updating
  // that row's header timestamp to the (now older) start of the group.
  const ctx = { roomId, threadId: null };
  let mergeEvent = null;
  let runEvents = newEvents;
  if (isGrouped(newEvents[newEvents.length - 1], prevEvents[0])) {
    mergeEvent = newEvents[newEvents.length - 1];
    runEvents = newEvents.slice(0, -1);
  }
  const firstRow = loadMoreRow.nextSibling;

  const frag = document.createDocumentFragment();
  for (const run of groupIntoRuns(runEvents)) {
    frag.appendChild(renderMessageGroup(run, ctx));
  }

  el.timeline.insertBefore(frag, loadMoreRow.nextSibling);

  if (mergeEvent && firstRow) {
    const bubble = firstRow.querySelector(".bubble");
    const sender = bubble?.querySelector(".sender");
    const headerTime = sender?.querySelector("time");
    if (headerTime) headerTime.textContent = formatMessageTimestamp(mergeEvent.timestamp);
    if (bubble && sender) {
      bubble.insertBefore(renderMessageItem(mergeEvent, ctx), sender.nextSibling);
      firstRow.dataset.eventId = mergeEvent.event_id;
    }
  }

  if (state.reachedStart.has(roomId)) {
    loadMoreRow.remove();
  } else {
    loadMoreRow.innerHTML = "";
    const btn = document.createElement("button");
    btn.textContent = "load more messages";
    btn.addEventListener("click", () => paginateBack(roomId));
    loadMoreRow.appendChild(btn);
  }

  timelineScrollAnchor.sync();
  updateJumpLatestVisibility();
}

/** Keeps pulling older messages automatically for as long as the loaded
 * history doesn't even fill the visible timeline area — without this, a
 * room with only a handful of synced messages left the "load more
 * messages" button sitting there requiring a click before there was even
 * anything to scroll, unlike the room list's own auto-grow
 * (`scheduleGrowRoomList`) which this mirrors. Once there's enough content
 * to scroll, the existing scroll-near-top listener below takes over as
 * usual. A `setTimeout` gives the browser a paint first so
 * `scrollHeight`/`clientHeight` reflect the DOM just inserted, and
 * `paginateBack`'s own `paginationInFlight` guard keeps this from
 * double-firing while a page is already in flight — the next
 * `TimelinePrepend` response re-checks and keeps going until either the
 * viewport is full or `reachedStart`. */
function maybeAutoLoadMore(roomId) {
  if (roomId !== state.selectedRoom) return;
  if (state.reachedStart.has(roomId) || state.paginationInFlight.has(roomId)) return;
  setTimeout(() => {
    if (roomId !== state.selectedRoom) return;
    if (state.reachedStart.has(roomId) || state.paginationInFlight.has(roomId)) return;
    if (el.timeline.scrollHeight <= el.timeline.clientHeight) {
      paginateBack(roomId);
    }
  }, 0);
}

function paginateBack(roomId) {
  if (state.reachedStart.has(roomId) || state.paginationInFlight.has(roomId)) return;
  state.paginationInFlight.add(roomId);
  // Swap the "load more messages" button for "loading..." without a full
  // `renderTimeline()` — that unconditionally scrolls to the bottom, which
  // is exactly wrong here: this only ever fires because the user just
  // scrolled *up* near the top to trigger it.
  const row = document.getElementById("load-more-row");
  if (row) row.innerHTML = loadingHtml("loading older messages...");
  send("PaginateBack", { room_id: roomId });
}

// Throttled to one check per animation frame — same reasoning as the
// room list's own scroll handler (see `el.roomListItems`'s). Without
// this, a fast scroll/trackpad fling fires this on every single scroll
// event (can be dozens per second), and `updateJumpLatestVisibility()`
// reading `scrollHeight`/`scrollTop`/`clientHeight` forces a synchronous
// layout recalculation each time — a real, measurable source of the
// scroll stutter on WebKitGTK specifically (see `#timeline`'s own
// `will-change: scroll-position` comment for the same underlying
// "WebKitGTK doesn't proactively optimize this like Chromium does" gap).
let timelineScrollRafPending = false;
el.timeline.addEventListener("scroll", () => {
  if (timelineScrollRafPending) return;
  timelineScrollRafPending = true;
  requestAnimationFrame(() => {
    timelineScrollRafPending = false;
    if (el.timeline.scrollTop < 80) paginateBack(state.selectedRoom);
    updateJumpLatestVisibility();
  });
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
  scrollToBottom(el.timeline);
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
  // Appended to <body> (like every dialog/lightbox in this app), not
  // `anchorEl` — `.msg-row` has `content-visibility: auto` (paint
  // containment), which clips any descendant that overflows a row's own
  // box. The popup opening *above* a short bubble routinely did exactly
  // that (escaping past the row's top edge into the previous row's
  // territory), rendering completely invisible even though the trigger
  // button that opened it worked fine — same underlying issue
  // `.reactions`/`.reaction-trigger` had before their own fix, just on
  // the other edge. `position: fixed` + coordinates computed from the
  // anchor's real on-screen position sidesteps the whole row hierarchy
  // (and its containment) entirely, so this can't happen again regardless
  // of future spacing/layout changes there.
  // `position: fixed` set *before* measuring `pickerRect` — while still
  // `position: static` (the default right after `appendChild`), a block
  // box with no explicit width stretches to its containing block's full
  // width (`<body>`, i.e. nearly the whole window), so measuring first
  // reported that same huge width back. `left`'s clamp below then read
  // "not enough room to the right of a window-wide box" and pinned the
  // popup to the left edge no matter where the anchor actually was.
  picker.style.position = "fixed";
  document.body.appendChild(picker);
  const anchorRect = anchorEl.getBoundingClientRect();
  const pickerRect = picker.getBoundingClientRect();
  let top = anchorRect.top - pickerRect.height - 4;
  if (top < 4) top = anchorRect.bottom + 4; // not enough room above — open below instead
  let left = anchorRect.left;
  left = Math.min(left, window.innerWidth - pickerRect.width - 4);
  left = Math.max(4, left);
  picker.style.top = `${top}px`;
  picker.style.left = `${left}px`;

  setTimeout(() => {
    document.addEventListener("click", () => picker.remove(), { once: true });
  }, 0);
}

/** A much broader curated set than `QUICK_REACTIONS` above — that one is
 * a tiny fixed set of reaction shortcuts, this backs the compose box's
 * own emoji picker (inserting a character into the message being typed),
 * where "just six" would be far too limiting. Loosely grouped by
 * category but rendered as one flat scrollable grid (see
 * `openEmojiPicker`) — a set this size doesn't need its own tab/category
 * UI to stay browsable. */
const EMOJI_PICKER_SET = [
  "😀", "😃", "😄", "😁", "😆", "😅", "😂", "🤣", "😊", "😇", "🙂", "🙃", "😉", "😌", "😍", "🥰",
  "😘", "😗", "😙", "😚", "😋", "😛", "😝", "😜", "🤪", "🤨", "🧐", "🤓", "😎", "🥸", "🤩", "🥳",
  "😏", "😒", "😞", "😔", "😟", "😕", "🙁", "☹️", "😣", "😖", "😫", "😩", "🥺", "😢", "😭", "😤",
  "😠", "😡", "🤬", "🤯", "😳", "🥵", "🥶", "😱", "😨", "😰", "😥", "😓", "🤗", "🤔", "🤭", "🤫",
  "🤥", "😶", "😐", "😑", "😬", "🙄", "😯", "😦", "😧", "😮", "😲", "🥱", "😴", "🤤", "😪", "😵",
  "🤐", "🥴", "🤢", "🤮", "🤧", "😷", "🤒", "🤕", "🤑", "🤠", "😈", "👿", "🤡", "💩", "👻", "💀",
  "👽", "🤖", "👍", "👎", "👌", "🤌", "🤏", "✌️", "🤞", "🤟", "🤘", "🤙", "👈", "👉", "👆", "🖕",
  "👇", "☝️", "👏", "🙌", "👐", "🤲", "🙏", "✍️", "💪", "🖐️", "✋", "👋", "🤝", "👊", "✊", "❤️",
  "🧡", "💛", "💚", "💙", "💜", "🖤", "🤍", "🤎", "💔", "❣️", "💕", "💞", "💓", "💗", "💖", "💘",
  "💝", "💯", "💢", "💥", "💫", "💦", "💨", "💣", "💬", "👀", "🎉", "🎊", "🎈", "🎁", "🔥", "✨",
  "🐶", "🐱", "🐭", "🐹", "🐰", "🦊", "🐻", "🐼", "🐨", "🐯", "🦁", "🐮", "🐷", "🐸", "🐵", "🙈",
  "🙉", "🙊", "🐔", "🐧", "🐦", "🦆", "🦉", "🐺", "🐴", "🦄", "🐝", "🦋", "🐢", "🐍", "🍏", "🍎",
  "🍊", "🍋", "🍌", "🍉", "🍇", "🍓", "🍒", "🍑", "🥭", "🍍", "🥥", "🥝", "🍅", "🌽", "🍞", "🧀",
  "🍔", "🍟", "🍕", "🌭", "🌮", "🍜", "🍣", "🍤", "🍩", "🍪", "🎂", "🍰", "🍫", "🍬", "☕", "🍵",
  "🍺", "🍻", "🥂", "🍷", "🚀", "✈️", "🚗", "🚲", "⛵", "🌍", "🌙", "⭐", "☀️", "⛅", "🌈", "☂️",
  "❄️", "⚡", "🎄", "🎃", "🎆", "🏆", "⚽", "🏀", "🎮", "🎲", "📱", "💻", "📷", "📞", "⏰", "💡",
  "📚", "✏️", "💰", "✉️", "📦", "🔒", "🔑", "🔨", "⚙️", "🔬", "🔭", "🛒",
];

/** Opens the compose box's own emoji picker, anchored to `emojiBtn` —
 * inserts the pick into `textarea` at the cursor (see `insertEmoji`),
 * unlike `openReactionPicker`'s "toggle a reaction on this message".
 * Portaled to `document.body` and positioned `fixed` from the button's
 * own on-screen rect, same reasoning as `openReactionPicker`/
 * `openActionsMenu` — the compose row sits at the very bottom of the
 * window, so the picker almost always needs to open *upward* to have
 * anywhere to actually render, which a plain absolutely-positioned
 * child of the toolbar can't do without also getting clipped by
 * whatever scrolls above it. */
function openEmojiPicker(emojiBtn, textarea) {
  document.querySelectorAll(".emoji-picker").forEach((p) => p.remove());
  const picker = document.createElement("div");
  picker.className = "emoji-picker";
  for (const emoji of EMOJI_PICKER_SET) {
    const opt = document.createElement("button");
    opt.type = "button";
    opt.textContent = emoji;
    opt.addEventListener("click", (ev) => {
      ev.stopPropagation();
      insertEmoji(textarea, emoji);
    });
    picker.appendChild(opt);
  }

  picker.style.position = "fixed";
  document.body.appendChild(picker);
  const anchorRect = emojiBtn.getBoundingClientRect();
  const pickerRect = picker.getBoundingClientRect();
  const margin = 4;
  let top = anchorRect.top - pickerRect.height - 4;
  if (top < margin) top = anchorRect.bottom + 4;
  top = Math.max(margin, Math.min(top, window.innerHeight - pickerRect.height - margin));
  let left = anchorRect.left;
  left = Math.max(margin, Math.min(left, window.innerWidth - pickerRect.width - margin));
  picker.style.top = `${top}px`;
  picker.style.left = `${left}px`;

  setTimeout(() => {
    document.addEventListener(
      "click",
      (e) => {
        if (!picker.contains(e.target) && e.target !== emojiBtn) picker.remove();
      },
      { once: true },
    );
  }, 0);
}

/** Inserts `emoji` at the compose textarea's cursor (replacing any
 * current selection) — same insertion mechanics as `toggleWrap`/
 * `insertLink` above, just plain text with no wrapping markup. */
function insertEmoji(textarea, emoji) {
  const { value, selectionStart: start, selectionEnd: end } = textarea;
  textarea.value = value.slice(0, start) + emoji + value.slice(end);
  const cursor = start + emoji.length;
  textarea.setSelectionRange(cursor, cursor);
  textarea.dispatchEvent(new Event("input", { bubbles: true }));
  textarea.focus();
}

/** Existing reaction pills only (emoji + count, highlighted if the
 * logged-in user is a reactor) — returns `null` when there are none, so
 * a bubble with no reactions doesn't reserve a row of space for an empty
 * list (that's what the corner "add reaction" trigger in `renderMessageItem`
 * is for instead, which only shows up on hover). Shared by the main
 * timeline and the thread panel — same `renderMessageItem()` call, same
 * `Command::ToggleReaction` either way. */
/** The small overlapping avatar stack under a message showing who's read
 * up to exactly that message — Element's own "seen by" indicator.
 * `event.read_by` (see `TimelineEvent`'s doc comment on the Rust side) is
 * already just the list of user IDs whose receipt points here, so this is
 * purely presentation: resolve each ID to a name/avatar via the room's
 * member list (already loaded for @mention autocomplete —
 * `renderReactions` above resolves reactor names the same way) and stack
 * them. `null` when nobody's receipt is on this message, same "don't
 * reserve empty space" convention `renderReactions` uses. */
function renderReadReceipts(event, ctx) {
  if (!event.read_by || event.read_by.length === 0) return null;
  const roomMembers = state.roomMembers[ctx.roomId] || [];
  const memberFor = (userId) => roomMembers.find(([id]) => id === userId);

  const wrap = document.createElement("div");
  wrap.className = "read-receipts";
  for (const userId of event.read_by) {
    const member = memberFor(userId);
    const name = member ? member[1] : userId;
    const avatarUrl = member ? member[2] : null;
    const avatar = renderAvatar(avatarUrl, name, userId, 16);
    avatar.title = `seen by ${name}`;
    wrap.appendChild(avatar);
  }
  return wrap;
}

function renderReactions(event, ctx) {
  if (!event.reactions || event.reactions.length === 0) return null;
  const wrap = document.createElement("div");
  wrap.className = "reactions";

  const toggle = (emoji) => {
    send("ToggleReaction", { room_id: ctx.roomId, event_id: event.event_id, emoji });
  };

  const roomMembers = state.roomMembers[ctx.roomId] || [];
  const nameFor = (userId) => {
    const member = roomMembers.find(([id]) => id === userId);
    return member ? member[1] : userId;
  };

  for (const r of event.reactions) {
    const pill = document.createElement("button");
    pill.className = "reaction-pill" + (r.by_me ? " mine" : "");
    pill.textContent = `${r.emoji} ${r.count}`;
    const who = (r.senders || []).map(nameFor).join(", ") || "no one yet";
    const action = r.by_me ? "click to remove your reaction" : "click to react";
    pill.title = `${who} — ${action}`;
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

/** A bubble's timestamp — just the time for a message sent today (the
 * common case, where the date would be redundant clutter on every single
 * bubble), and "dd/mm HH:mm" for anything from an earlier day, so
 * scrolling back into history (or a thread that's been going for weeks)
 * still says *which* day each message landed on. */
function formatMessageTimestamp(timestampMs) {
  const d = new Date(timestampMs);
  const now = new Date();
  const isToday =
    d.getFullYear() === now.getFullYear() && d.getMonth() === now.getMonth() && d.getDate() === now.getDate();
  const time = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  if (isToday) return time;
  const date = d.toLocaleDateString([], { day: "2-digit", month: "2-digit" });
  return `${date} ${time}`;
}

/** Shows/positions `actions` (a message's react/reply/thread/more row)
 * on hovering `item` or `actions` itself — `position: fixed`, computed
 * here from `item`'s real on-screen rect rather than plain CSS
 * (`left: 100%` relative to the message's own box), because a *wide*
 * message (one whose text already reaches close to the column's own
 * max-width) left no room for a purely-CSS "float to the right" to land
 * in without pushing the buttons — the "⋯" trigger specifically — off
 * the edge of the visible timeline entirely, inaccessible. Clamped
 * against the nearest scrollable message container instead, falling
 * back toward the message's own right edge if there's truly no room to
 * spare (better than off-screen, if rare in practice). Hovering `actions`
 * itself also keeps it shown (with a short delay on leaving either one)
 * so moving the cursor from the text onto the buttons never drops them
 * mid-transition. */
function wireMsgActionsHover(item, actions) {
  let hideTimer = null;
  const show = () => {
    clearTimeout(hideTimer);
    const itemRect = item.getBoundingClientRect();
    const container = item.closest("#timeline, #thread-messages") || document.body;
    const containerRect = container.getBoundingClientRect();
    const buttonCount = actions.children.length;
    const width = buttonCount * 32 + Math.max(0, buttonCount - 1) * 6;
    const margin = 6;
    let left = itemRect.right + margin;
    if (left + width > containerRect.right - margin) {
      left = containerRect.right - margin - width;
    }
    const top = itemRect.bottom - 32;
    actions.style.left = `${left}px`;
    actions.style.top = `${top}px`;
    actions.classList.add("visible");
  };
  const hide = () => {
    hideTimer = setTimeout(() => actions.classList.remove("visible"), 80);
  };
  item.addEventListener("mouseenter", show);
  item.addEventListener("mouseleave", hide);
  actions.addEventListener("mouseenter", show);
  actions.addEventListener("mouseleave", hide);
}

/** Builds ONE message's own content — reply-preview, body/media,
 * thread-badge, reactions/trigger, "⋯" menu — as a single
 * `.msg-item[data-event-id]` div. Does NOT build the avatar/sender
 * header or the outer row; `renderMessageGroup` below owns those, once
 * per run of consecutive same-sender messages (see `groupIntoRuns`),
 * with each event's own `.msg-item` stacked tightly inside that one
 * shared bubble — a whole run reads as one continuous block (Element's
 * own grouping) instead of each message getting its own avatar-height
 * row regardless of whether it actually needed one. */
function renderMessageItem(event, ctx) {
  const item = document.createElement("div");
  item.className = "msg-item" + (event.mentions_me ? " mentioned" : "");
  item.dataset.eventId = event.event_id;
  // Everything below except `.msg-actions` (appended straight to `item`,
  // right after this) goes into `.msg-content` — `.msg-actions` floats
  // over the corner via `position: absolute` (see its own CSS comment
  // for why: a real flex sibling that only sometimes exists pushes
  // every other message around on each hover in/out). `bubble` is kept
  // as the local alias every `bubble.appendChild(...)` below already uses.
  const content = document.createElement("div");
  content.className = "msg-content";
  const bubble = content;

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
      img.addEventListener("click", () => openLightbox(cached, event.body));
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
      if (failed) {
        placeholder.textContent = "⟳ image failed to load — click to retry";
      } else {
        placeholder.innerHTML = loadingHtml("loading image...");
      }
      if (failed) {
        placeholder.addEventListener("click", () => {
          delete imageFetchAttempts[event.media_url];
          state.imageRequested.delete(event.media_url);
          requestImage(event.media_url);
          placeholder.innerHTML = loadingHtml("loading image...");
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
  } else if (event.msg_type === "audio" && event.media_url) {
    if (event.media_mime) state.imageMime[event.media_url] = event.media_mime;
    if (event.media_encryption) state.imageEncryption[event.media_url] = event.media_encryption;
    const cached = state.imageCache[event.media_url];
    if (cached) {
      const audio = document.createElement("audio");
      audio.controls = true;
      audio.src = cached;
      bubble.appendChild(audio);
    } else {
      const placeholder = document.createElement("div");
      placeholder.className = "image-placeholder";
      placeholder.innerHTML = loadingHtml("loading voice message...");
      bubble.appendChild(placeholder);
      if (!state.imageRequested.has(event.media_url)) requestImage(event.media_url);
    }
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
    applyMentionPills(body, event.mentioned_user_ids, ctx.roomId);
    decorateSharedLinks(body, ctx.roomId);
    bubble.appendChild(body);
  }

  // Every message's own timestamp — hidden until this specific
  // `.msg-item` is hovered (see `.meta` in style.css), since the group's
  // header line already shows the *first* message's time and repeating
  // it, small and permanent, under every single one is what used to make
  // a quick back-to-back run from one person take up more vertical space
  // than the same messages read in Element.
  const meta = document.createElement("div");
  meta.className = "meta";
  meta.textContent = formatMessageTimestamp(event.timestamp);
  bubble.appendChild(meta);

  // Unlike `.meta` above (hidden until hover — fine for a timestamp
  // nobody needs to see immediately), a message's own send state is worth
  // knowing about right away: this is what makes the optimistic bubble
  // `sendCurrentMessage` renders the instant Send is pressed actually
  // read as "sending", not just a message that silently appeared, and
  // what surfaces a failed send at all (which used to only ever reach
  // `console.error`).
  if (event._sendStatus === "sending" || event._sendStatus === "failed") {
    const status = document.createElement("div");
    status.className = `send-status ${event._sendStatus}`;
    status.textContent =
      event._sendStatus === "sending"
        ? "sending…"
        : `failed to send${event._sendError ? ` — ${event._sendError}` : ""}`;
    bubble.appendChild(status);
  }

  // A message with a thread stays flagged all the time, not just on
  // hover — otherwise it's easy to miss that replies exist at all.
  //
  // `event.thread_count` comes from the raw event's bundled
  // `unsigned.m.relations.m.thread` aggregation (see `convert.rs`), which
  // this homeserver only reliably includes for recently-synced events —
  // older messages pulled in via pagination (`PaginateBack`/`LoadTimeline`)
  // routinely came back with it missing even though the message genuinely
  // has a thread, so the badge silently never showed for a room's earlier
  // history. `state.threadsByRoom` (fetched separately via the `/threads`
  // endpoint, see `scanAllRoomThreads`) doesn't have that gap, so fall back
  // to its count whenever the bundled one is absent.
  const threadCount =
    event.thread_count || state.threadsByRoom[ctx.roomId]?.find((t) => t.event_id === event.event_id)?.thread_count;
  if (!ctx.threadId && threadCount) {
    const badge = document.createElement("div");
    badge.className = "thread-badge";
    badge.textContent = `🧵 ${threadCount} ${threadCount === 1 ? "reply" : "replies"} →`;
    badge.addEventListener("click", () => openThread(ctx.roomId, event));
    bubble.appendChild(badge);
  }

  const reactions = renderReactions(event, ctx);
  if (reactions) bubble.appendChild(reactions);

  // React/reply/thread(/more) all share one `.msg-actions` wrapper,
  // floating over the corner via `position: absolute` (see its CSS
  // comment) — hover-only, and outside layout entirely so showing/
  // hiding it never reflows the messages around it.
  const actions = document.createElement("div");
  actions.className = "msg-actions";

  // A message with no reactions yet gets a small "add reaction" icon
  // here instead — hidden until hover, so it costs no space at all
  // rather than reserving an empty pill row like `renderReactions` used
  // to unconditionally do. Just the emoji, no "+" — that second glyph is
  // what didn't fit next to it inside this button's fixed circle at a
  // large chosen message font-size (the icon's own size is fixed, not
  // tied to that setting, precisely so this can't recur — see its CSS).
  if (!reactions) {
    const trigger = document.createElement("button");
    trigger.className = "reaction-trigger";
    trigger.title = "add reaction";
    trigger.textContent = "🙂";
    trigger.addEventListener("click", (e) => {
      e.stopPropagation();
      openReactionPicker(bubble, (emoji) => {
        send("ToggleReaction", { room_id: ctx.roomId, event_id: event.event_id, emoji });
      });
    });
    actions.appendChild(trigger);
  }

  // Reply — its own always-visible icon now (used to live only inside
  // the "⋯" dropdown), same react/reply/more row a normal chat app's
  // message toolbar has.
  const replyTrigger = document.createElement("button");
  replyTrigger.className = "reply-trigger";
  replyTrigger.title = "reply";
  replyTrigger.textContent = "↩";
  replyTrigger.addEventListener("click", (e) => {
    e.stopPropagation();
    startReply(ctx.roomId, ctx.threadId, event);
  });
  actions.appendChild(replyTrigger);

  // Thread — same, its own icon now, but only for a message that
  // doesn't already have one (once it does, the thread-count badge above
  // is itself the "open this thread" entry point — a second one here
  // would be redundant, same condition the old dropdown item used).
  if (!ctx.threadId && !threadCount) {
    const threadTrigger = document.createElement("button");
    threadTrigger.className = "thread-trigger";
    threadTrigger.title = "reply in thread";
    threadTrigger.textContent = "🧵";
    threadTrigger.addEventListener("click", (e) => {
      e.stopPropagation();
      openThread(ctx.roomId, event);
    });
    actions.appendChild(threadTrigger);
  }

  // The remaining, less-frequent actions stay behind "⋯" rather than
  // becoming their own icons too — `.msg-actions` reads as a normal
  // chat app's react/reply/thread/more row, not a wall of buttons.
  // `openActionsMenu` (portaled to `document.body`, same as
  // `openReactionPicker`) positions its dropdown from the trigger's
  // actual on-screen rect, so it's never at the mercy of `.msg-row`'s
  // layout shifting underneath it after it's already open.
  const menuTrigger = document.createElement("button");
  menuTrigger.className = "msg-menu-trigger";
  menuTrigger.title = "more actions";
  menuTrigger.textContent = "⋯";
  menuTrigger.addEventListener("click", (e) => {
    e.stopPropagation();
    const items = [];
    items.push({ label: "share", onClick: () => shareMessage(ctx.roomId, event.event_id, ctx.threadId) });
    items.push({
      label: "from user",
      title: `see every message from ${event.sender_name}, across all rooms`,
      onClick: () => openUserSearch(event.sender),
    });
    const isPinned = (state.pinnedEvents[ctx.roomId] || []).includes(event.event_id);
    items.push({
      label: isPinned ? "unpin" : "pin",
      onClick: () =>
        send(isPinned ? "UnpinMessage" : "PinMessage", { room_id: ctx.roomId, event_id: event.event_id }),
    });
    if (event.is_own && event.msg_type !== "image" && event.msg_type !== "deleted") {
      items.push({ label: "edit", onClick: () => startEdit(ctx.roomId, ctx.threadId, event) });
    }
    if (event.is_own) {
      items.push({
        label: "delete",
        onClick: () => send("DeleteMessage", { room_id: ctx.roomId, event_id: event.event_id }),
      });
    }
    openActionsMenu(menuTrigger, items);
  });
  actions.appendChild(menuTrigger);

  const readBy = renderReadReceipts(event, ctx);
  if (readBy) content.appendChild(readBy);

  item.appendChild(content);
  item.appendChild(actions);
  wireMsgActionsHover(item, actions);

  // Re-applies a still-active "jump to this message" highlight across a
  // rebuild of this exact item — see `lastJumpHighlightedEventId`'s doc
  // comment for why that's tracked by event ID rather than a DOM
  // reference in the first place.
  if (event.event_id === lastJumpHighlightedEventId) {
    item.classList.add("jump-highlight");
  }

  return item;
}

/** Splits a chronological `events` array into runs of consecutive
 * same-sender messages (see `isGrouped`) — one run backs one
 * `renderMessageGroup` call (one avatar/header, many stacked
 * `.msg-item`s), the same grouping decision the old per-message
 * `opts.grouped` flag used to make one row at a time. */
function groupIntoRuns(events) {
  const runs = [];
  let prev = null;
  for (const event of events) {
    if (prev && isGrouped(prev, event)) {
      runs[runs.length - 1].push(event);
    } else {
      runs.push([event]);
    }
    prev = event;
  }
  return runs;
}

/** Builds one `.msg-row` for a run of consecutive same-sender messages
 * (see `groupIntoRuns`) — avatar + sender header from `events[0]`, then
 * each event's own `.msg-item` (see `renderMessageItem`) stacked inside
 * the same bubble, reading as one continuous block instead of one
 * avatar-height row per message regardless of whether it needed one. */
function renderMessageGroup(events, ctx) {
  const first = events[0];
  const side = first.is_own ? "own" : "other";
  const row = document.createElement("div");
  row.className = `msg-row ${side}`;
  row.dataset.eventId = first.event_id;

  const avatarEl = renderAvatar(first.sender_avatar_url, first.sender_name, first.sender, 28);
  // Presence now overlays the avatar's own corner (Slack/Discord-style
  // badge) instead of sitting as a separate dot on the sender-name line —
  // cleaner next to the name, and this is the one place presence is
  // already scoped to a single person, so it needs no dot-plus-label
  // pairing to stay legible the way, say, a member list would.
  const presenceDot = document.createElement("span");
  const presenceInfo = state.presence[first.sender];
  presenceDot.className = "avatar-presence-dot" + (presenceInfo ? ` ${presenceInfo.presence}` : "");
  presenceDot.title = presenceInfo ? presenceInfo.presence : "";
  presenceDot.dataset.presenceUser = first.sender;
  avatarEl.appendChild(presenceDot);
  row.appendChild(avatarEl);

  const col = document.createElement("div");
  col.className = "msg-col";
  const bubble = document.createElement("div");
  bubble.className = `bubble ${side}`;

  const sender = document.createElement("div");
  sender.className = "sender";
  sender.appendChild(document.createTextNode(first.sender_name));
  sender.style.color = senderColor(first.sender);
  const time = document.createElement("time");
  time.textContent = formatMessageTimestamp(first.timestamp);
  sender.appendChild(time);
  bubble.appendChild(sender);

  for (const event of events) {
    bubble.appendChild(renderMessageItem(event, ctx));
  }

  col.appendChild(bubble);
  row.appendChild(col);
  return row;
}

/** Opens a small dropdown menu anchored to `anchorEl` — `items` is
 * `[{label, onClick, title?}]`. Shared by every "⋯" trigger (currently
 * just `renderMessageItem`'s). Appended to `document.body` and positioned
 * `fixed` from `anchorEl`'s real on-screen rect, so it's never at the
 * mercy of `.msg-row`'s own layout (a resize, a scroll, ...) shifting
 * underneath it after it's already open. */
function openActionsMenu(anchorEl, items) {
  document.querySelectorAll(".actions-menu").forEach((m) => m.remove());
  const menu = document.createElement("div");
  menu.className = "actions-menu";
  for (const item of items) {
    const btn = document.createElement("button");
    btn.textContent = item.label;
    if (item.title) btn.title = item.title;
    btn.addEventListener("click", (ev) => {
      ev.stopPropagation();
      menu.remove();
      item.onClick();
    });
    menu.appendChild(btn);
  }

  menu.style.position = "fixed";
  document.body.appendChild(menu);
  const anchorRect = anchorEl.getBoundingClientRect();
  const menuRect = menu.getBoundingClientRect();
  const margin = 4;

  let top = anchorRect.bottom + 4;
  if (top + menuRect.height > window.innerHeight - margin) {
    top = anchorRect.top - menuRect.height - 4;
  }
  top = Math.max(margin, top);
  let left = anchorRect.right - menuRect.width;
  left = Math.max(margin, Math.min(left, window.innerWidth - menuRect.width - margin));
  menu.style.top = `${top}px`;
  menu.style.left = `${left}px`;

  setTimeout(() => {
    document.addEventListener("click", () => menu.remove(), { once: true });
  }, 0);
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

/** Wraps `bytes` in a `Blob` and hands back an `object URL` for it — an
 * `<img>`/`<audio>` `src` works identically either way, but this used to
 * base64-encode the whole thing into a `data:` URL instead (chunked
 * `String.fromCharCode` + `btoa`, to avoid blowing the call stack on a
 * multi-MB photo). That's real synchronous main-thread work scaling with
 * the image's size — a several-MB photo (an ordinary phone camera shot)
 * could visibly stutter the UI for a moment right as it finished
 * downloading, and worse with several arriving close together (opening a
 * room with a handful of images in view, e.g. the mention-picker/threads-
 * list image thumbnails). `URL.createObjectURL` does no such encoding —
 * it just hands the engine a reference to the same bytes already in
 * memory — so this is both faster and simpler. (Never revoked: this
 * app's `state.imageCache` keeps entries for the whole session, same
 * effective lifetime a base64 string sitting in that same map already
 * had — nothing here makes retention worse, an explicit `revokeObjectURL`
 * would only matter if entries were ever evicted before then, which they
 * currently aren't.) */
function bytesToImageUrl(bytes, mimeHint) {
  const u8 = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
  const mime = mimeHint || sniffImageMime(u8) || "application/octet-stream";
  return URL.createObjectURL(new Blob([u8], { type: mime }));
}

// ---- Minimal markdown: **bold**, *italic*, `code`, ```blocks```, links,
// bare URLs, and paragraph breaks on blank lines. Deliberately small — just
// enough for normal chat formatting, not a full CommonMark implementation.
// `marked` (github.com/markedjs/marked) + `DOMPurify` (github.com/cure53/DOMPurify),
// vendored locally in dist/ and loaded before this script (see index.html)
// — real, battle-tested markdown/sanitization instead of this file hand-
// rolling regex substitutions for it (a previous version did exactly
// that; it covered bold/italic/code/links, then headers/lists, and kept
// needing another pass for whatever it missed next — tables, nested
// lists, blockquotes, ... a real parser just doesn't have that class of
// gap). Configured once, at load, rather than passing the same options
// to every `marked.parse()` call.
marked.use({ gfm: true, breaks: true });

/** Markdown → sanitized HTML, for anywhere message bodies/LLM summaries
 * get displayed. Two separate steps, in this order, and both required:
 * `marked.parse` turns the markdown into HTML but — per its own docs —
 * does *not* sanitize it (raw HTML already present in the source passes
 * straight through, by design, since that's valid CommonMark); this app's
 * "markdown" almost always comes from other, untrusted room members (a
 * message body) or an LLM summarizing them (`Command::Summarize`), so
 * `DOMPurify.sanitize` on the result is what actually keeps a
 * `<script>`/`onerror=`/etc. in someone's message from ever executing —
 * skipping it (or reordering the two calls) reopens a straightforward
 * stored-XSS hole. */
function renderMarkdown(text) {
  return DOMPurify.sanitize(marked.parse(text ?? ""));
}

/** A preview-length version of `renderMarkdown`: renders the *full* body
 * first, then trims the already-rendered HTML down to `maxChars` of
 * visible text — rather than every call site's old pattern of
 * `renderMarkdown(truncate(text, maxChars))`, which truncated the raw
 * markdown *source* before parsing it. That corrupted anything the cut
 * landed in the middle of: a `[label](https://very/long/url)` link with
 * the cut partway through its URL left the `[label](` sitting there as
 * literal text with only the broken tail auto-linkified (marked's GFM
 * autolinker doesn't need a closing `)` the way an actual link does), and
 * the same happens to `**bold**`/`` `code` `` spans straddling the cut.
 * Walking the *rendered* HTML's text nodes instead means a link's `href`
 * always stays intact even once its label text is what's cut short. */
function renderMarkdownPreview(text, maxChars) {
  const container = document.createElement("div");
  container.innerHTML = renderMarkdown(text);
  let remaining = maxChars;
  let done = false;
  const walk = (node) => {
    for (const child of Array.from(node.childNodes)) {
      if (done) {
        child.remove();
        continue;
      }
      if (child.nodeType === Node.TEXT_NODE) {
        const t = child.textContent;
        if (t.length > remaining) {
          child.textContent = t.slice(0, remaining) + "…";
          remaining = 0;
          done = true;
        } else {
          remaining -= t.length;
        }
      } else if (child.nodeType === Node.ELEMENT_NODE) {
        walk(child);
        if (!child.textContent) child.remove();
      }
    }
  };
  walk(container);
  return container.innerHTML;
}

function escapeHtml(s) {
  const d = document.createElement("div");
  d.textContent = s;
  return d.innerHTML;
}

/** Turns a plain-text `@DisplayName` occurrence for someone actually named
 * in `mentionedUserIds` (the message's real `m.mentions`, or a matrix.to
 * pill in its formatted body — see `mentioned_user_ids` on the backend)
 * into a colored pill, matching how Element renders a mention — instead
 * of the plain "@Name" text `renderMarkdown` alone leaves behind.
 * Deliberately gated on that real signal rather than just "any `@` some
 * member's current name happens to follow": retyping the exact same
 * "@Name" a second time without picking it from the autocomplete again
 * sends with no mention metadata at all — and Element itself then shows
 * plain text for it too, confirmed against the real app — so pill-ifying
 * it here anyway would be *less* faithful to Element, not more, even
 * though it can look like "the same tag" was typed twice.
 *
 * Deliberately DOM-based (walks `containerEl`'s text nodes and splices in
 * `<span>` elements) rather than string/HTML-based: the message's
 * `formatted_body` HTML is attacker-controlled (any other user in the
 * room can send it) and isn't sanitized anywhere in this app, so building
 * pills by re-parsing *that* and dropping it into `innerHTML` would be a
 * straightforward stored-XSS hole. This only ever touches text nodes
 * already produced by `renderMarkdown`'s own escaping, and only inserts
 * elements built with `textContent`/`createElement` — nothing here can
 * execute attacker HTML. */
function applyMentionPills(containerEl, mentionedUserIds, roomId) {
  if (!mentionedUserIds || mentionedUserIds.length === 0) return;
  const roomMembers = state.roomMembers[roomId] || [];
  // Longest name first, same reasoning as `buildMentionHtml`'s own
  // `candidates` sort — a shorter mentioned name that happens to be a
  // prefix of a longer one shouldn't shadow it.
  const targets = mentionedUserIds
    .map((uid) => roomMembers.find(([id]) => id === uid))
    .filter(Boolean)
    .map(([userId, name]) => ({ userId, name }))
    .sort((a, b) => b.name.length - a.name.length);
  if (targets.length === 0) return;

  const walker = document.createTreeWalker(containerEl, NodeFilter.SHOW_TEXT);
  const textNodes = [];
  let node;
  while ((node = walker.nextNode())) textNodes.push(node);

  // A word-character check (Unicode-aware, so Vietnamese diacritics count)
  // used to keep a bare-name match (see below) from firing mid-word.
  const isWordChar = (ch) => Boolean(ch) && /[\p{L}\p{N}]/u.test(ch);

  for (const textNode of textNodes) {
    const text = textNode.nodeValue;
    // Quick skip: bail unless the text could possibly contain *some*
    // target, either as "@Name" (this app's own composer inserts that
    // literally) or as a bare "Name" — real Element's plain-text mention
    // fallback has no "@" at all, just the display name.
    if (!targets.some((t) => text.includes(t.name))) continue;
    const parent = textNode.parentNode;
    if (!parent) continue;

    const frag = document.createDocumentFragment();
    let rest = text;
    let changed = false;
    while (rest.length > 0) {
      let best = null;
      for (const t of targets) {
        const atIdx = rest.indexOf("@" + t.name);
        if (atIdx !== -1 && (!best || atIdx < best.idx)) {
          best = { idx: atIdx, len: t.name.length + 1, t };
        }
        // Bare-name fallback, gated on word boundaries so e.g. a target
        // named "An" doesn't fire inside "Anh". Only tried where it'd
        // beat (or tie, favoring the "@"-prefixed match already found)
        // the current best, since a real "@Name" hit is always preferred.
        let searchFrom = 0;
        while (searchFrom <= rest.length) {
          const idx = rest.indexOf(t.name, searchFrom);
          if (idx === -1) break;
          if (best && idx >= best.idx) break;
          const before = rest[idx - 1];
          const after = rest[idx + t.name.length];
          if (!isWordChar(before) && !isWordChar(after)) {
            best = { idx, len: t.name.length, t };
            break;
          }
          searchFrom = idx + 1;
        }
      }
      if (!best) {
        frag.appendChild(document.createTextNode(rest));
        break;
      }
      changed = true;
      if (best.idx > 0) frag.appendChild(document.createTextNode(rest.slice(0, best.idx)));
      const pill = document.createElement("span");
      pill.className = "mention-pill";
      pill.style.color = senderColor(best.t.userId);
      pill.style.background = senderColor(best.t.userId) + "26"; // ~15% opacity
      pill.textContent = "@" + best.t.name;
      pill.title = best.t.userId;
      frag.appendChild(pill);
      rest = rest.slice(best.idx + best.len);
    }
    if (changed) parent.replaceChild(frag, textNode);
  }
}

/** Turns a bare `https://matrix.to/#/!room/$event` link left over from
 * `renderMarkdown`'s auto-linkify (i.e. one nobody gave custom link text —
 * `[click here](...)` keeps its own text untouched) into a small preview
 * card showing who sent the shared message and a snippet of it — same
 * idea as Element's own permalink pills, instead of a long raw URL
 * wrapping across the bubble. Keeps the underlying `<a href>` unchanged
 * (only its class/innerHTML change), so the existing global matrix.to
 * click handler (see the "Share message" section below) still opens it
 * exactly as before — this only changes what it looks like.
 *
 * The target event's sender/body come from whatever's already loaded
 * locally (`findEvent` — covers the timeline, an open thread panel, and
 * the thread-roots cache) or, failing that, `getEventPreview`'s server
 * fetch (same one `Command::GetEventPreview` backs for the pinned-messages
 * dialog) — its `Event::EventPreview` answer patches this exact anchor
 * back in once it arrives, see that event's handler. */
function decorateSharedLinks(containerEl, currentRoomId) {
  const anchors = containerEl.querySelectorAll('a[href^="https://matrix.to/#/!"]');
  anchors.forEach((a) => {
    const href = a.getAttribute("href");
    if (a.textContent !== href) return; // custom link text — leave it alone
    const m = href.match(/^https:\/\/matrix\.to\/#\/(![^/?]+)\/(\$[^/?]+)(\?[^#]*)?/);
    if (!m) return;
    const roomId = decodeURIComponent(m[1]);
    const eventId = decodeURIComponent(m[2]);
    a.classList.add("shared-link-preview");
    a.dataset.sharedRoomId = roomId;
    a.dataset.sharedEventId = eventId;
    renderSharedLinkPreviewInto(a, roomId, eventId, currentRoomId);
  });
}

function renderSharedLinkPreviewInto(a, roomId, eventId, currentRoomId) {
  const ev = findEvent(roomId, eventId) || getEventPreview(roomId, eventId);
  let bodyHtml;
  if (ev) {
    const room = roomId !== currentRoomId ? state.rooms.find((r) => r.room_id === roomId) : null;
    const roomLabel = room ? `<span class="shared-link-room">${escapeHtml(room.name)}</span>` : "";
    bodyHtml = `${roomLabel}<span class="shared-link-sender">${escapeHtml(ev.sender_name)}</span>: ${escapeHtml(truncate(ev.body || "", 80))}`;
  } else if (ev === null) {
    bodyHtml = `<span style="color:var(--text-weak);">message unavailable</span>`;
  } else {
    bodyHtml = loadingHtml("loading shared message...");
  }
  a.innerHTML = `<span class="shared-link-icon">🔗</span><span class="shared-link-body">${bodyHtml}</span>`;
}

/** Every "loading X..." placeholder in the app renders through this, so
 * they all get the same bouncing-dog indicator instead of each writing
 * its own plain-text copy. */
function loadingHtml(text) {
  return `<span class="loading-indicator"><img class="loading-icon" src="assets/loading-dog.png" alt="" /> ${escapeHtml(text)}</span>`;
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
  // Which suggestion row Up/Down has highlighted — same roving-highlight
  // idea as the room list's `roomListActiveIndex`. Reset on every fresh
  // filter (a keystroke, or the box losing focus) since the result set
  // underneath it just changed.
  let activeIndex = -1;

  const selectMention = (userId, name, at) => {
    const before = inputEl.value.slice(0, at);
    const after = inputEl.value.slice(inputEl.selectionStart);
    inputEl.value = `${before}@${name} ${after}`;
    inputEl.dispatchEvent(new Event("input", { bubbles: true }));
    mentionsList.push({ userId, displayName: name });
    suggestionsEl.style.display = "none";
    inputEl.focus();
  };

  inputEl.addEventListener("input", () => {
    activeIndex = -1;
    const m = activeMentionQuery(inputEl.value, inputEl.selectionStart);
    const roomId = getRoomId();
    if (!m || !roomId) {
      suggestionsEl.style.display = "none";
      return;
    }
    const members = state.roomMembers[roomId] || [];
    // Diacritic-insensitive, same as the room list's own filter
    // (`normalizeForSearch`) — without this, typing an unaccented "@duy"
    // (the everyday way most Vietnamese keyboards/typists enter text)
    // never matched a name like "Đinh Hà Duy" at all, since a plain
    // `.toLowerCase()` still leaves "à"/"ă"/... as different characters
    // from their unaccented base letter.
    const query = normalizeForSearch(m.query);
    const matches = members
      .filter(([, name]) => normalizeForSearch(name).includes(query))
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
      item.addEventListener("click", () => selectMention(userId, name, m.at));
      suggestionsEl.appendChild(item);
    }
  });

  // Arrow keys move the highlight, Enter/Tab picks the highlighted (or
  // first, if none highlighted yet) suggestion, Escape dismisses — mouse
  // click was previously the *only* way to actually pick a suggestion;
  // Enter just fell through to the textarea's own handler and inserted a
  // newline instead. Registered before `wireComposeEditor`'s own keydown
  // listener on the same element (see call order below/at the thread
  // panel), so `stopImmediatePropagation()` here reliably pre-empts it.
  inputEl.addEventListener("keydown", (e) => {
    if (suggestionsEl.style.display === "none") return;
    const items = suggestionsEl.querySelectorAll(".mention-item");
    if (items.length === 0) return;

    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      const delta = e.key === "ArrowDown" ? 1 : -1;
      activeIndex = (activeIndex + delta + items.length) % items.length;
      items.forEach((it, i) => it.classList.toggle("active", i === activeIndex));
      items[activeIndex].scrollIntoView({ block: "nearest" });
    } else if (e.key === "Enter" || e.key === "Tab") {
      e.preventDefault();
      e.stopImmediatePropagation();
      const m = activeMentionQuery(inputEl.value, inputEl.selectionStart);
      if (!m) return;
      const idx = activeIndex === -1 ? 0 : activeIndex;
      const roomId = getRoomId();
      const members = state.roomMembers[roomId] || [];
      const query = normalizeForSearch(m.query);
      const matches = members.filter(([, name]) => normalizeForSearch(name).includes(query)).slice(0, 6);
      const picked = matches[idx];
      if (picked) selectMention(picked[0], picked[1], m.at);
    } else if (e.key === "Escape") {
      suggestionsEl.style.display = "none";
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
  // Hand-rolled undo/redo — WebKitGTK (and some Android WebViews) don't
  // reliably wire up native Ctrl+Z/Ctrl+Shift+Z undo for a plain
  // `<textarea>`, confirmed broken here even before any of this app's own
  // JS touches the box. It also wouldn't have survived this app's own
  // edits anyway: every programmatic change below (markdown toolbar,
  // mention/emoji insertion, the paste-a-link-onto-a-selection handler)
  // reassigns `.value` directly, which silently invalidates whatever
  // undo history the browser *did* have — there'd be nothing left to
  // undo back through past the most recent one regardless.
  //
  // Snapshots coalesce: plain typing within `COALESCE_MS` of the last
  // keystroke updates the current step in place rather than pushing a
  // new one, so undo steps back a *pause's* worth of typing at a time
  // (like most editors) instead of one character at a time. A
  // programmatic edit (dispatched as a plain `Event`, not a real
  // `InputEvent` — `e.inputType` is `undefined` for those, never for an
  // actual keystroke/IME commit) always starts its own step instead,
  // since those are deliberate, chunky edits a user would expect to undo
  // as one unit no matter how soon after typing they happened.
  const COALESCE_MS = 400;
  let undoStack = [{ value: textarea.value, start: textarea.selectionStart, end: textarea.selectionEnd }];
  let undoIndex = 0;
  let lastEditAt = 0;

  const currentEntry = () => ({ value: textarea.value, start: textarea.selectionStart, end: textarea.selectionEnd });

  const recordEdit = (isCoalescible) => {
    const now = Date.now();
    if (isCoalescible && undoIndex === undoStack.length - 1 && now - lastEditAt < COALESCE_MS) {
      undoStack[undoIndex] = currentEntry();
    } else {
      undoStack = undoStack.slice(0, undoIndex + 1);
      undoStack.push(currentEntry());
      undoIndex = undoStack.length - 1;
      // Cap so an unusually long compose session doesn't grow this
      // unbounded — losing the oldest step once there are 200 is a
      // reasonable trade against holding every keystroke of an essay.
      if (undoStack.length > 200) {
        undoStack.shift();
        undoIndex--;
      }
    }
    lastEditAt = now;
  };

  const restore = (entry) => {
    textarea.value = entry.value;
    textarea.setSelectionRange(entry.start, entry.end);
    autoResizeTextarea(textarea);
  };

  const undo = () => {
    if (undoIndex === 0) return;
    undoIndex--;
    restore(undoStack[undoIndex]);
  };
  const redo = () => {
    if (undoIndex >= undoStack.length - 1) return;
    undoIndex++;
    restore(undoStack[undoIndex]);
  };

  textarea.addEventListener("input", (e) => {
    autoResizeTextarea(textarea);
    recordEdit(e instanceof InputEvent && e.inputType);
  });
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
      // it has no binding on Ctrl+Shift+E, but stopping propagation here
      // is the same pattern the Ctrl+Z/Y undo/redo handlers below use, so
      // the compose box's own meaning always wins over anything the
      // app-wide handler might someday bind on the same combo.
      e.stopPropagation();
      insertCodeBlock(textarea);
      return;
    }
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "z") {
      e.preventDefault();
      e.stopPropagation();
      if (e.shiftKey) redo();
      else undo();
      return;
    }
    // Ctrl+Y as an alternate redo — the Windows-editor convention,
    // alongside Ctrl+Shift+Z above (the Mac/most-web-apps one).
    if ((e.ctrlKey || e.metaKey) && !e.shiftKey && e.key.toLowerCase() === "y") {
      e.preventDefault();
      e.stopPropagation();
      redo();
      return;
    }
    if ((e.ctrlKey || e.metaKey) && !e.shiftKey) {
      // No "k" here (unlike bold/italic/code) for "insert link" — kept
      // unbound rather than reused, now that "focus room search" moved to
      // Alt+K and no longer collides with it (see the `document`-level
      // handler below). The link toolbar button still works exactly the
      // same either way, just without its own keyboard shortcut.
      const action = { b: "bold", i: "italic", e: "code" }[e.key.toLowerCase()];
      if (action) {
        e.preventDefault();
        e.stopPropagation();
        applyMarkdownAction(textarea, action);
      }
    }
  });
}

/** Wires the B/I/code/link/emoji toolbar row above a compose box. */
function wireMarkdownToolbar(toolbarEl, textarea) {
  toolbarEl?.querySelectorAll("[data-md]").forEach((btn) => {
    btn.addEventListener("click", (e) => {
      e.preventDefault();
      applyMarkdownAction(textarea, btn.dataset.md);
    });
  });
  const emojiBtn = toolbarEl?.querySelector(".emoji-btn");
  emojiBtn?.addEventListener("click", (e) => {
    e.preventDefault();
    openEmojiPicker(emojiBtn, textarea);
  });
}

// =========================================================================
// Compose / send
// =========================================================================

function sendCurrentMessage() {
  stopTypingNotice();
  const body = el.composeInput.value.trim();
  if (state.pendingImage && !state.pendingImage.threadId) {
    confirmPendingImage();
    return;
  }
  if (state.pendingFile && !state.pendingFile.threadId) {
    confirmPendingFile();
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
    // In-place clear, not `state.composeMentions = []` — the main
    // compose box's `wireMentionAutocomplete` is only ever wired up once
    // at page load (unlike the thread panel's, rewired on every
    // re-render), so its closure holds onto this exact array object for
    // the whole session. Reassigning the property to a brand-new array
    // here would leave that closure still pushing into the old, now
    // disconnected one — every mention picked after your *first* sent
    // message would silently stop actually attaching (the send path
    // would always see the fresh, still-empty array instead), even
    // though the "@Name" text still looked right in the box.
    state.composeMentions.length = 0;
    el.composeInput.focus();
    return;
  }

  const { ids, html } = buildMentionHtml(body, state.composeMentions);
  const replyTo = state.pendingReply && !state.pendingReply.threadId ? state.pendingReply.eventId : null;
  const roomId = state.selectedRoom;
  const localId = crypto.randomUUID();
  const optimistic = makeOptimisticEvent(localId, body, replyTo, ids);
  if (optimistic) {
    state.pendingSends.set(localId, { roomId, threadId: null });
    if (!state.timelines[roomId]) state.timelines[roomId] = [];
    state.timelines[roomId].push(optimistic);
    appendMessage(roomId, optimistic);
  }
  send("SendMessage", {
    room_id: roomId,
    body,
    thread_id: null,
    mentions: ids,
    html_body: html,
    local_id: localId,
    reply_to_event_id: replyTo,
  });
  cancelReply();
  // In-place clear — see the `EditMessage` branch above for why
  // reassigning to a new array here would break every mention picked
  // after this one for the rest of the session.
  state.composeMentions.length = 0;
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

// ---- Typing notices ----
// `SetTyping` fires at most once per 4s while actively typing (the SDK's
// own `typing_notice` already dedupes/times this out server-side, but
// there's no point re-sending on every keystroke either) and once more
// with `typing: false` after 5s of no input, so a message left half-typed
// doesn't show as "typing..." forever.
let typingActiveUntil = 0;
let typingStopTimer = null;
function notifyTyping() {
  if (!state.selectedRoom) return;
  const now = Date.now();
  if (now > typingActiveUntil) {
    send("SetTyping", { room_id: state.selectedRoom, typing: true });
  }
  typingActiveUntil = now + 4000;
  clearTimeout(typingStopTimer);
  typingStopTimer = setTimeout(() => {
    if (state.selectedRoom) send("SetTyping", { room_id: state.selectedRoom, typing: false });
    typingActiveUntil = 0;
  }, 5000);
}
function stopTypingNotice() {
  clearTimeout(typingStopTimer);
  if (typingActiveUntil && state.selectedRoom) {
    send("SetTyping", { room_id: state.selectedRoom, typing: false });
  }
  typingActiveUntil = 0;
}
el.composeInput.addEventListener("input", notifyTyping);
el.composeInput.addEventListener("blur", stopTypingNotice);

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

function toggleMemePicker() {
  const show = el.memePicker.style.display === "none";
  closeMemePickers();
  if (show) {
    renderMemePicker(el.memePicker, state.selectedRoom, null);
    el.memePicker.style.display = "grid";
  }
}
document.addEventListener("click", (e) => {
  if (el.memePicker.style.display !== "none" && !el.memePicker.contains(e.target) && e.target !== el.btnComposePlus) {
    el.memePicker.style.display = "none";
  }
  const threadPicker = document.getElementById("thread-meme-picker");
  const threadBtn = document.getElementById("thread-btn-meme");
  if (threadPicker && threadPicker.style.display !== "none" && !threadPicker.contains(e.target) && e.target !== threadBtn) {
    threadPicker.style.display = "none";
  }
});

// The main compose row used to have 4 separate icon buttons (attach/meme/
// voice/poll) — on a phone-width screen that left barely any room for the
// actual text input (see git history: this is what "chỗ nhập tin nhắn ...
// quá nhỏ" was about). One "+" button opening this dropdown (reusing
// `openActionsMenu`, same as a message's "⋯" menu) frees that width back
// up for the textarea; while recording, the same button becomes a "stop"
// button instead of opening this menu (see `toggleVoiceRecording`).
el.btnComposePlus.addEventListener("click", (e) => {
  e.stopPropagation();
  if (state.voiceRecording) {
    toggleVoiceRecording();
    return;
  }
  if (!state.selectedRoom) return;
  openActionsMenu(el.btnComposePlus, [
    { label: "📎 image", onClick: () => el.fileInput.click() },
    { label: "📄 file", onClick: () => el.genericFileInput.click() },
    { label: "🐸 emoji / meme", onClick: toggleMemePicker },
    { label: "🎤 voice message", onClick: toggleVoiceRecording },
    { label: "📊 poll", onClick: openCreatePollDialog },
  ]);
});
el.fileInput.addEventListener("change", () => {
  const file = el.fileInput.files[0];
  if (!file) return;
  loadPendingImage(file, state.selectedRoom, null);
  el.fileInput.value = "";
});
el.genericFileInput.addEventListener("change", () => {
  const file = el.genericFileInput.files[0];
  if (!file) return;
  loadPendingFile(file, state.selectedRoom, null);
  el.genericFileInput.value = "";
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
    const dataUrl = bytesToImageUrl(bytes, "image/png");
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
  ["Alt+K or /", "focus room search"],
  ["Alt+M", "focus the main message box"],
  ["Alt+N", "focus the thread's message box"],
  ["Alt+Shift+M", "focus the main timeline"],
  ["Alt+Shift+N", "focus the thread panel"],
  ["Alt+T", "open this room's threads list, and focus its search box"],
  ["↑ / ↓ (in search)", "move through search results"],
  ["Enter (in search)", "open the highlighted room"],
  ["Esc (in search)", "clear search, then unfocus"],
  ["↑ / ↓ (threads list)", "move through the threads list"],
  ["Enter (threads list)", "open the highlighted thread"],
  ["← / → (in tags)", "switch space/tag"],
  ["Alt+↑ / Alt+↓", "previous / next room in the list"],
  ["Alt+L", "toggle unread-only filter"],
  ["Alt+R", "mark current room as read"],
  ["Alt+U", "mark current thread as read (only while a thread is open)"],
  ["Esc", "close dialog / menu"],
  ["?", "show this list"],
  ["Shift+Enter (compose)", "new line instead of sending"],
  ["Ctrl+B / Ctrl+I", "bold / italic selection"],
  ["Ctrl+E / Ctrl+Shift+E", "inline code / code block"],
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
el.btnShortcuts.addEventListener("click", () => {
  closeChatsMenu();
  showShortcutsHelp();
});

el.btnLogout.addEventListener("click", () => {
  closeChatsMenu();
  send("Logout");
});

/** True while the user is typing somewhere else — global shortcuts (`/`,
 * `?`, ...) that reuse plain, easy-to-hit keys must not fire while that's
 * happening, or every message containing "/" or "?" would get hijacked. */
function isTypingContext(target) {
  const tag = target?.tagName;
  return tag === "INPUT" || tag === "TEXTAREA" || target?.isContentEditable;
}

document.addEventListener("keydown", (e) => {
  const typing = isTypingContext(e.target);

  // Alt is this app's one consistent "leader" modifier for app-wide
  // navigation — a plain Alt+letter is never confusable with a typed
  // character (unlike the bare `/`/`?`/single-letter shortcuts below,
  // which do need the `!typing` gate), so none of these check it either.
  // Previously some of these lived on Ctrl instead, each paired with an
  // unrelated Alt+<same letter> shortcut below it (Ctrl+M "focus compose"
  // next to Alt+M "focus timeline", etc.) — collapsed onto Alt throughout
  // so the whole app has one leader key, with the previously-Alt half of
  // each pair moved to Alt+Shift instead of losing its binding.
  if (e.altKey && !e.ctrlKey && !e.metaKey && e.key.toLowerCase() === "k") {
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
  if (e.altKey && !e.ctrlKey && !e.metaKey && !e.shiftKey && e.key.toLowerCase() === "m") {
    e.preventDefault();
    if (state.selectedRoom) el.composeInput.focus();
    return;
  }
  if (e.altKey && !e.ctrlKey && !e.metaKey && !e.shiftKey && e.key.toLowerCase() === "n") {
    e.preventDefault();
    if (state.rightPanel?.kind === "thread") {
      document.getElementById("thread-compose-input")?.focus();
    }
    return;
  }
  // Alt+Shift variants of the two above — focus the *message list* itself
  // (main timeline / thread panel) rather than its compose box, e.g. to
  // scroll it with Page Up/Down or just move focus off the compose input
  // without sending anything. `tabindex="-1"` on both targets (see
  // index.html) is what makes a plain, non-interactive `<div>` a valid
  // `.focus()` target at all. Alt+Shift+letter rather than some unrelated
  // key: Alt and Shift both sit under the same hand (bottom-left), so this
  // stays a one-handed stretch same as plain Alt+letter, just distinct
  // from "focus compose" on the same letter above.
  if (e.altKey && e.shiftKey && !e.ctrlKey && !e.metaKey && e.key.toLowerCase() === "m") {
    e.preventDefault();
    if (state.selectedRoom) el.timeline.focus();
    return;
  }
  if (e.altKey && e.shiftKey && !e.ctrlKey && !e.metaKey && e.key.toLowerCase() === "n") {
    e.preventDefault();
    if (state.rightPanel?.kind === "thread") {
      // `#side-panel-body`, not `#thread-messages` — the latter doesn't
      // scroll itself (see `appendThreadMessage`'s comment on why: its
      // *parent* does), so focusing it wouldn't let Page Up/Down actually
      // scroll anything.
      document.getElementById("side-panel-body")?.focus();
    }
    return;
  }
  if (e.altKey && !e.ctrlKey && !e.metaKey && e.key.toLowerCase() === "t") {
    e.preventDefault();
    // Opens the current room's thread list if nothing's open yet — but
    // leaves it alone if some threads-list panel (this room's, or "all
    // threads") is already open, so this can't clobber a broader search
    // someone's mid-typing into with a room-scoped one. This single
    // binding now covers what used to be Ctrl+T (just open it)
    // separately from Alt+T (open-if-needed, then focus the filter) —
    // the latter was already a strict superset of the former.
    if (state.rightPanel?.kind !== "threads-list") openRoomThreadsList();
    const filterInput = document.getElementById("threads-filter");
    filterInput?.focus();
    filterInput?.select();
    return;
  }
  if (e.key === "?" && !typing) {
    e.preventDefault();
    showShortcutsHelp();
    return;
  }
  if (e.altKey && !e.ctrlKey && !e.metaKey && e.key.toLowerCase() === "l") {
    e.preventDefault();
    el.btnUnreadOnly.click();
    return;
  }
  // Mark the current *room* as read — always the room, regardless of
  // whatever else is open (a thread panel included), so this key does
  // one predictable thing rather than switching targets underneath you.
  if (e.altKey && !e.ctrlKey && !e.metaKey && e.key.toLowerCase() === "r") {
    e.preventDefault();
    el.btnMarkRead.click();
    return;
  }
  // Mark the current *thread* as read — its own separate key (distinct
  // from Alt+R above) rather than one key that meant different things
  // depending on what was open; only does something while a thread panel
  // is actually open.
  if (e.altKey && !e.ctrlKey && !e.metaKey && e.key.toLowerCase() === "u") {
    e.preventDefault();
    if (state.rightPanel?.kind === "thread") markCurrentThreadRead();
    return;
  }
  if (e.altKey && !e.ctrlKey && !e.metaKey && !e.shiftKey && (e.key === "ArrowUp" || e.key === "ArrowDown")) {
    e.preventDefault();
    if (state.visibleRoomIds.length === 0) return;
    const currentIndex = state.visibleRoomIds.indexOf(state.selectedRoom);
    const delta = e.key === "ArrowDown" ? 1 : -1;
    const base = currentIndex === -1 ? (delta === 1 ? -1 : 0) : currentIndex;
    const nextIndex = (base + delta + state.visibleRoomIds.length) % state.visibleRoomIds.length;
    selectRoom(state.visibleRoomIds[nextIndex]);
    return;
  }
  // Plain (no modifier) Up/Down/Enter navigate the threads-list side
  // panel's rows, same roving-highlight shape as the room filter box's
  // own Up/Down/Enter handling — gated on that panel actually being open
  // and not typing anywhere, so this can't hijack arrow keys/Enter
  // anywhere else (a compose box, a dialog, ...). The panel's own search
  // box (`#threads-filter`) handles the same keys itself, unaffected by
  // this `!typing` gate — see its own `keydown` listener in
  // `renderSidePanel`.
  if (
    !typing &&
    !e.ctrlKey &&
    !e.metaKey &&
    !e.altKey &&
    state.rightPanel?.kind === "threads-list" &&
    (e.key === "ArrowUp" || e.key === "ArrowDown" || e.key === "Enter")
  ) {
    if (navigateThreadsList(e.key)) e.preventDefault();
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
  let toast = document.getElementById("toast");
  if (!toast) {
    toast = document.createElement("div");
    toast.id = "toast";
    // Bottom-left, not top-center — top-center sat right on top of the
    // timeline header's own "[ mark read ]"/"[ threads ]"/"[ ... ]"
    // buttons, which a toast firing from pressing one of those (e.g.
    // "marked as read") then immediately covered, right when someone
    // might reach for another one of them. `bottom` itself is set fresh
    // below every time, not here, since it depends on the compose box's
    // current height (see below).
    toast.style.cssText =
      "position:fixed;left:16px;background:#222;border:1px solid var(--border);color:var(--text);padding:6px 12px;font-size:12px;z-index:999;max-width:80%;";
    document.body.appendChild(toast);
  }
  // On both desktop and phone-width layouts, `#compose-row` spans out to
  // (or past) the window's own left edge — desktop's doesn't start until
  // `#room-list` ends, but phone-width collapses to a single full-width
  // pane the instant a room's open, and a toast is just as likely to fire
  // from inside one as from the room list. Anchoring above its actual
  // current height (0 when it's hidden entirely, e.g. no room selected
  // yet) keeps this clear of it either way, rather than the fixed 16px
  // a plain bottom-left placement would need and then silently overlap
  // the compose box on a narrow window.
  const composeHeight = el.composeRow.style.display !== "none" ? el.composeRow.getBoundingClientRect().height : 0;
  // `env(safe-area-inset-bottom)` matches every other fixed-position
  // element in this app (see e.g. `#side-panel`'s own mobile rule in
  // style.css) — without it, this would sit under a phone's gesture-nav
  // bar on Android's edge-to-edge display.
  toast.style.bottom = `calc(${16 + composeHeight}px + env(safe-area-inset-bottom, 0px))`;
  toast.textContent = msg;
  toast.style.display = "block";
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (toast.style.display = "none"), 6000);
}

function loadPendingImage(file, roomId, threadId) {
  const reader = new FileReader();
  reader.onload = () => {
    const bytes = Array.from(new Uint8Array(reader.result));
    const dataUrl = bytesToImageUrl(bytes, file.type);
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

/** Same "pick, preview, confirm/cancel" flow as `loadPendingImage`, for an
 * arbitrary file (no `accept` restriction on the input, and no image
 * thumbnail — just the filename and size) — the "📄 file" attach option,
 * for anything a chat's "📎 image" picker won't take (a PDF, a zip, an
 * APK, ...). */
function loadPendingFile(file, roomId, threadId) {
  const reader = new FileReader();
  reader.onload = () => {
    const bytes = Array.from(new Uint8Array(reader.result));
    state.pendingFile = {
      roomId,
      threadId,
      bytes,
      filename: file.name || "file",
      mime: file.type || "application/octet-stream",
      size: file.size,
    };
    renderPendingFile();
  };
  reader.readAsArrayBuffer(file);
}

function renderPendingFile() {
  const p = state.pendingFile;

  if (p && !p.threadId) {
    fillPendingFilePreview(el.pendingFilePreview, p);
  } else {
    el.pendingFilePreview.style.display = "none";
  }

  const threadPreviewEl = document.getElementById("thread-pending-file-preview");
  if (!threadPreviewEl) return;
  if (p && p.threadId && state.rightPanel?.kind === "thread" && state.rightPanel.root.event_id === p.threadId) {
    fillPendingFilePreview(threadPreviewEl, p);
  } else {
    threadPreviewEl.style.display = "none";
  }
}

function fillPendingFilePreview(container, p) {
  container.style.display = "flex";
  container.style.alignItems = "center";
  container.style.gap = "8px";
  container.innerHTML = "";
  const label = document.createElement("span");
  label.textContent = `📄 ${p.filename} (${formatFileSize(p.size)})`;
  container.appendChild(label);
  const actions = document.createElement("div");
  actions.className = "actions";
  const sendBtn = document.createElement("button");
  sendBtn.textContent = "send file";
  sendBtn.addEventListener("click", confirmPendingFile);
  const cancelBtn = document.createElement("button");
  cancelBtn.textContent = "cancel";
  cancelBtn.addEventListener("click", () => {
    state.pendingFile = null;
    renderPendingFile();
  });
  actions.appendChild(sendBtn);
  actions.appendChild(cancelBtn);
  container.appendChild(actions);
}

function formatFileSize(bytes) {
  if (!Number.isFinite(bytes)) return "";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function confirmPendingFile() {
  const p = state.pendingFile;
  if (!p) return;
  send("SendFile", {
    room_id: p.roomId,
    thread_id: p.threadId,
    filename: p.filename,
    bytes: p.bytes,
    mime: p.mime,
    local_id: crypto.randomUUID(),
  });
  state.pendingFile = null;
  renderPendingFile();
}

// =========================================================================
// Threads
// =========================================================================

function openThread(roomId, root) {
  // Deliberately doesn't clear `unreadThreads`/`mentionThreads` for this
  // thread — opening it no longer marks it read by itself (see
  // `markCurrentThreadRead`'s doc comment); the unread state stays until
  // the panel's own "[ mark read ]" button is actually pressed.
  //
  // Remembers whichever threads-list panel (if any) was open right before
  // this call, so the thread panel's own close button can go back to it
  // instead of leaving the side panel entirely — on mobile (where the side
  // panel is a full-screen overlay over the timeline, see the
  // `max-width: 720px` block in style.css) closing a thread would
  // otherwise dump you straight out to the timeline you were never
  // looking at, one screen further than the "back" gesture should go. A
  // thread opened some other way (a reply badge in the main timeline, a
  // shared-message link, ...) has no threads-list to return to, so its
  // close button falls back to the old "just close" behavior.
  const cameFrom = state.rightPanel?.kind === "threads-list" ? state.rightPanel : null;
  state.rightPanel = { kind: "thread", roomId, root, events: [], cameFrom };
  renderSidePanel();
  send("LoadThread", { room_id: roomId, thread_root_id: root.event_id });
  // @mention autocomplete in the thread panel needs this — opening a
  // thread from the global threads popup can land here for a room other
  // than the currently selected one, which `selectRoom` never fetched it
  // for.
  if (!state.roomMembers[roomId]) send("ListMembers", { room_id: roomId });
}

// =========================================================================
// Search a user's messages across all rooms
// =========================================================================

/** Opens the side panel and kicks off `Command::SearchUserMessages` for
 * `userId` (`@name:server`) — "show me everything this person has said,
 * across every room". `fromTs`/`toTs` are optional millisecond bounds
 * (inclusive) narrowing the search to a date range — `null` on either
 * side means open-ended. Always re-runs the search, even if the panel is
 * already open for the same user, since a different date range needs a
 * fresh fetch anyway. */
function openUserSearch(userId, fromTs = null, toTs = null) {
  state.rightPanel = { kind: "user-search", userId, loading: true, results: [], truncated: false };
  renderSidePanel();
  send("SearchUserMessages", { user_id: userId, from_ts: fromTs, to_ts: toTs });
}

/** Reads the dialog's optional from/to `<input type=date>` values as
 * millisecond bounds — start-of-day for "from", end-of-day for "to" so the
 * "to" date itself is included. `NaN`/empty inputs become `null` (no
 * bound on that side). */
function readUserSearchDateRange() {
  const fromStr = document.getElementById("dlg-user-search-from")?.value;
  const toStr = document.getElementById("dlg-user-search-to")?.value;
  const fromTs = fromStr ? new Date(fromStr + "T00:00:00").getTime() : null;
  const toTs = toStr ? new Date(toStr + "T23:59:59.999").getTime() : null;
  return { fromTs: Number.isFinite(fromTs) ? fromTs : null, toTs: Number.isFinite(toTs) ? toTs : null };
}

/** Renders/filters the picker list inside the "messages from a user"
 * dialog — `state.allUsers` (`[user_id, display_name]` pairs, deduped
 * across every joined room) once `Event::AllUsers` has answered, or a
 * loading placeholder before that. Matching by id too (not just display
 * name), Vietnamese-diacritic-insensitive like the room filter, since not
 * every account has a friendly display name set. */
function renderUserSearchDialogList() {
  const listEl = document.getElementById("dlg-user-search-list");
  if (!listEl) return;
  if (state.allUsers === null) {
    listEl.innerHTML = `<div style="padding:8px;font-size:12px;">${loadingHtml("loading members...")}</div>`;
    return;
  }
  const query = normalizeForSearch(document.getElementById("dlg-user-search-filter").value.trim());
  const matches = state.allUsers.filter(
    ([userId, name]) => !query || normalizeForSearch(name).includes(query) || normalizeForSearch(userId).includes(query),
  );
  listEl.innerHTML = "";
  if (matches.length === 0) {
    listEl.innerHTML = `<div style="padding:8px;font-size:12px;color:var(--text-weak)">no matching member — you can still type a full @user:server id and press enter</div>`;
    return;
  }
  for (const [userId, name] of matches.slice(0, 200)) {
    const item = document.createElement("div");
    item.className = "mention-item";
    item.textContent = userId === name ? name : `${name}  (${userId})`;
    item.addEventListener("click", () => {
      const { fromTs, toTs } = readUserSearchDateRange();
      closeDialog();
      openUserSearch(userId, fromTs, toTs);
    });
    listEl.appendChild(item);
  }
}

el.btnUserSearch.addEventListener("click", () => {
  closeChatsMenu();
  showDialog(`
    <h3>messages from a user</h3>
    <label>pick a member, or type an id and press enter</label>
    <input type="text" id="dlg-user-search-filter" placeholder="filter by name or @user:server" autocomplete="off" />
    <div id="dlg-user-search-list" class="user-search-list"></div>
    <label>from date (optional)</label>
    <input type="date" id="dlg-user-search-from" />
    <label>to date (optional)</label>
    <input type="date" id="dlg-user-search-to" />
    <div class="actions">
      <button id="dlg-cancel">cancel</button>
    </div>
  `);
  document.getElementById("dlg-cancel").addEventListener("click", closeDialog);
  const filterInput = document.getElementById("dlg-user-search-filter");
  filterInput.addEventListener("input", renderUserSearchDialogList);
  filterInput.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      const typed = filterInput.value.trim();
      if (typed.startsWith("@") && typed.includes(":")) {
        const { fromTs, toTs } = readUserSearchDateRange();
        closeDialog();
        openUserSearch(typed, fromTs, toTs);
      }
    }
  });
  if (state.allUsers === null && !state.allUsersLoading) {
    state.allUsersLoading = true;
    send("ListAllUsers");
  }
  renderUserSearchDialogList();
  filterInput.focus();
});

el.btnMarkRead.addEventListener("click", () => {
  if (!state.selectedRoom) return;
  send("MarkRoomRead", { room_id: state.selectedRoom });
  // Optimistic, same reasoning as the live `NewMessage` unread bump below —
  // the backend no longer round-trips a room-list refresh after
  // `MarkRoomRead` itself (sliding sync's own diff stream will eventually
  // echo the server's confirmation), so clear the badge here instead of
  // waiting for that.
  const room = state.rooms.find((r) => r.room_id === state.selectedRoom);
  if (room) {
    room.unread_count = 0;
    room.mention_count = 0;
    knownUnreadCounts.set(room.room_id, 0);
    knownMentionCounts.set(room.room_id, 0);
    // Exempts this room from `applyUnreadFloor`'s stale-count protection
    // for a few seconds — otherwise the very next room-list diff (which
    // may still carry the server's pre-receipt count) would look exactly
    // like the race that protection exists for, and get "corrected" right
    // back up instead of actually zeroing.
    recentlyMarkedRead.set(state.selectedRoom, Date.now());
    renderRooms();
  }
  showToast("marked as read");
});

el.btnRoomThreads.addEventListener("click", openRoomThreadsList);

/** Opens the current room's thread-list panel — shared by the
 * `[ threads ]` button and the `Alt+T` shortcut. */
/** Moves/activates the threads-list panel's roving highlight — shared by
 * the global keydown handler (fires when nothing in particular has focus)
 * and `#threads-filter`'s own listener (fires while the search box has
 * focus, bypassing the global handler's `!typing` gate the same way
 * `el.roomFilter`'s keydown listener does for the room list). Returns
 * whether it actually handled `key`, so callers know whether to
 * `preventDefault()`. */
/** Whether a thread root `t` (from `state.threadsByRoom[roomId]`) counts
 * as unread — prefers the server-derived `t.is_unread` (this account's
 * actual threaded read receipt vs. the thread's latest reply, computed in
 * `thread_is_unread` in `worker.rs`) whenever it's known. Falls back to
 * the older session-local `unreadThreads` heuristic only for the rare
 * case that couldn't be determined (`is_unread` is `null`/`undefined` —
 * e.g. the room lookup failed backend-side), so a thread never silently
 * stops being flagged unread just because the real answer wasn't
 * available for one specific fetch. */
function isThreadUnread(roomId, t) {
  return t.is_unread ?? state.unreadThreads.has(`${roomId}|${t.event_id}`);
}

/** Whether thread `t` has a message tagging this account — anywhere in
 * the thread, not just its root: the root itself (`t.mentions_me`, its
 * real `m.mentions`/highlight), its bundled latest-reply preview
 * (`t.latest_reply_mentions_me` — same check, run server-side against
 * that reply's own content), or a later live reply this session flagged
 * via `state.mentionThreads` (see that Set's doc comment). Combined with
 * `isThreadUnread`, this is what drives the threads-list panel's
 * "[ @mentions ]" filter — a ping from any reply counts, not only one
 * from the first message. */
function isThreadMentioningMe(roomId, t) {
  return (
    Boolean(t.mentions_me) ||
    Boolean(t.latest_reply_mentions_me) ||
    state.mentionThreads.has(`${roomId}|${t.event_id}`)
  );
}

/** Sends the "mark this thread read" receipt and updates local state for
 * thread `t` — the shared half of `markThreadReadFromList` and
 * `markAllThreadsRead`, split out so marking a whole batch doesn't
 * re-render the panel once per thread. Sends the real receipt for the
 * thread's latest reply (`latest_reply_event_id`, from the root's own
 * bundled aggregation — falls back to the root itself for a thread with
 * no replies loaded), same as opening the thread would once it finished
 * loading. Callers are responsible for `updateThreadsButtonBadge()` and
 * re-rendering afterwards. */
function markThreadReadLocal(roomId, t) {
  const eventId = t.latest_reply_event_id || t.event_id;
  send("MarkThreadRead", { room_id: roomId, thread_root_id: t.event_id, event_id: eventId });
  const key = `${roomId}|${t.event_id}`;
  state.unreadThreads.delete(key);
  state.mentionThreads.delete(key);
  const markRead = (list) => {
    const found = list?.find((e) => e.event_id === t.event_id);
    if (found) found.is_unread = false;
  };
  markRead(state.threadsByRoom[roomId]);
  markRead(state.timelines[roomId]);
}

/** Marks thread `t` read straight from the threads-list panel — the "✓
 * read" button on an unread row, without opening it. */
function markThreadReadFromList(roomId, t) {
  markThreadReadLocal(roomId, t);
  updateThreadsButtonBadge();
  renderThreadsListRows();
}

/** The threads-list panel's "[ mark all read ]" button — marks every
 * currently-unread thread within `scope` (a single room id, or `null` for
 * the "all threads" view) read in one go, regardless of whatever the
 * "[ unread ]"/"[ @mentions ]" toggles or search box are currently
 * filtering the *visible* rows down to; a bulk action like this should
 * act on everything loaded, not just what happens to still be on screen. */
function markAllThreadsRead(scope) {
  for (const [roomId, threads] of Object.entries(state.threadsByRoom)) {
    if (scope !== null && roomId !== scope) continue;
    for (const t of threads) {
      if (isThreadUnread(roomId, t)) markThreadReadLocal(roomId, t);
    }
  }
  updateThreadsButtonBadge();
  renderThreadsListRows();
}

/** The open thread panel's own "[ mark read ]" button — opening a thread
 * no longer marks it read by itself (previously it did, the instant its
 * events finished loading; see the `ThreadEvents` handler's history),
 * same as opening a room no longer marks *it* read either — reading the
 * messages on screen isn't the same as deliberately clearing the badge,
 * and auto-marking meant a thread you'd merely glanced at (or that
 * scrolled past while looking for something else) silently lost its
 * unread state. Marks up through whichever reply is currently the newest
 * loaded in the panel (`rp.events`' last entry), falling back to the root
 * itself for a thread with none loaded yet. */
function markCurrentThreadRead() {
  const rp = state.rightPanel;
  if (!rp || rp.kind !== "thread") return;
  const { roomId, root, events } = rp;
  const eventId = events.length > 0 ? events[events.length - 1].event_id : root.event_id;
  send("MarkThreadRead", { room_id: roomId, thread_root_id: root.event_id, event_id: eventId });
  const key = `${roomId}|${root.event_id}`;
  state.unreadThreads.delete(key);
  state.mentionThreads.delete(key);
  root.is_unread = false;
  const markRead = (list) => {
    const found = list?.find((e) => e.event_id === root.event_id);
    if (found) found.is_unread = false;
  };
  markRead(state.threadsByRoom[roomId]);
  markRead(state.timelines[roomId]);
  updateThreadsButtonBadge();
  showToast("thread marked as read");
}

function navigateThreadsList(key) {
  const rows = state.visibleThreadRows;
  if (rows.length === 0) return false;
  if (key === "Enter") {
    const picked = rows[state.threadsListActiveIndex];
    if (!picked) return false;
    const root = state.threadsByRoom[picked.roomId]?.find((t) => t.event_id === picked.eventId);
    if (root) openThread(picked.roomId, root);
    return true;
  }
  if (key !== "ArrowUp" && key !== "ArrowDown") return false;
  const delta = key === "ArrowDown" ? 1 : -1;
  const base = state.threadsListActiveIndex === -1 ? (delta === 1 ? -1 : 0) : state.threadsListActiveIndex;
  state.threadsListActiveIndex = (base + delta + rows.length) % rows.length;
  // Not `renderSidePanel()` — that would rebuild `#threads-filter` too
  // (see `renderThreadsListRows`'s doc comment), which only matters here
  // because this is reachable from that very input's own `keydown`
  // listener: an arrow key while it's focused would otherwise cost it its
  // own focus on every press.
  renderThreadsListRows();
  document
    .getElementById("side-panel-body")
    ?.querySelector(".thread-row.kbd-active")
    ?.scrollIntoView({ block: "nearest" });
  return true;
}

function openRoomThreadsList() {
  if (!state.selectedRoom) return;
  send("ListThreads", { room_id: state.selectedRoom });
  state.rightPanel = { kind: "threads-list", scope: state.selectedRoom };
  state.threadsListActiveIndex = -1;
  state.threadsListFilter = "";
  renderSidePanel();
}

/** Fetches the first page of threads for every joined, non-space room —
 * once, right after the room list first has anything in it (see the
 * `Rooms` event handler). Without this, "all threads" had no data for a
 * room until either the user opened that room's own thread list, or a
 * `ThreadReply` happened to arrive over live sync sometime this session
 * — which, right after launch, is nothing: the panel looked empty no
 * matter how much real thread activity a room actually had. Genuinely
 * "3000+ requests on a large account" in the worst case (the reason this
 * used to be scoped down to a guessed subset of rooms instead), but each
 * one is a single cheap paginated `/threads` call routed through its own
 * dedicated runtime (`Command::ListThreads`, see `worker.rs`) rather than
 * the ambient one — it can't block anything else this app is doing
 * meanwhile, only take a while to finish on a very large account. */
function scanAllRoomThreads() {
  for (const room of state.rooms) {
    if (room.is_space || room.is_invite) continue;
    send("ListThreads", { room_id: room.room_id });
  }
}

el.btnGlobalThreads.addEventListener("click", () => {
  // `scanAllRoomThreads` already covers every room that existed at
  // launch — this just tops up any room it couldn't have known about yet
  // (joined/created after that scan ran), so opening the panel is never
  // missing data for a room this account is in *right now*.
  for (const room of state.rooms) {
    if (room.is_space || room.is_invite) continue;
    if (!(room.room_id in state.threadsByRoom)) {
      send("ListThreads", { room_id: room.room_id });
    }
  }
  state.rightPanel = { kind: "threads-list", scope: null };
  state.threadsListActiveIndex = -1;
  state.threadsListFilter = "";
  renderSidePanel();
});

function updateThreadsButtonBadge() {
  // `isThreadUnread` prefers the server-derived flag over the
  // session-local `unreadThreads` Set (see its own doc comment) — using
  // it here too means these two badges agree with what "all threads"/a
  // room's own thread list actually show, instead of a plain
  // `unreadThreads.size` check that only ever reflects replies that
  // happened to arrive live this session.
  const anyUnread = Object.entries(state.threadsByRoom).some(([roomId, threads]) =>
    threads.some((t) => isThreadUnread(roomId, t)),
  );
  // Red instead of the plain "unread" green whenever at least one of those
  // unread threads actually pings this account — same grey-vs-red
  // distinction as the room list's badge (`.room-badge.mention`).
  const anyMentioned = Object.entries(state.threadsByRoom).some(([roomId, threads]) =>
    threads.some((t) => isThreadUnread(roomId, t) && isThreadMentioningMe(roomId, t)),
  );
  el.btnGlobalThreads.textContent = anyUnread ? "[ threads ● ]" : "[ threads ]";
  el.btnGlobalThreads.style.color = anyMentioned ? "var(--danger)" : anyUnread ? "#5ac878" : "";

  const roomUnread = state.selectedRoom
    ? (state.threadsByRoom[state.selectedRoom] || []).some((t) => isThreadUnread(state.selectedRoom, t))
    : false;
  const roomMentioned = state.selectedRoom
    ? (state.threadsByRoom[state.selectedRoom] || []).some(
        (t) => isThreadUnread(state.selectedRoom, t) && isThreadMentioningMe(state.selectedRoom, t),
      )
    : false;
  el.btnRoomThreads.textContent = roomUnread ? "[ threads ● ]" : "[ threads ]";
  el.btnRoomThreads.style.color = roomMentioned ? "var(--danger)" : roomUnread ? "#5ac878" : "";
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
  const grouped = isGrouped(prevReply, event);
  const lastRow = grouped ? msgsEl.lastElementChild : null;
  const lastBubble = lastRow?.classList.contains("msg-row") ? lastRow.querySelector(".bubble") : null;
  if (lastBubble) {
    lastBubble.appendChild(renderMessageItem(event, threadCtx));
  } else {
    msgsEl.appendChild(renderMessageGroup([event], threadCtx));
  }
  if (wasNearBottom) scrollToBottom(panelBody);
}

/** The thread panel's first page of replies (`Event::ThreadEvents`,
 * right after `openThread()`'s own initial `renderSidePanel()` — that one
 * always renders with `events: []`, so `#thread-messages` is just the
 * root + a `<hr>` at this point) — appended onto that existing shell
 * instead of a second full `renderSidePanel()` teardown-and-rebuild for
 * data that arrives a moment after opening. Returns `false` (caller falls
 * back to a full render) if the shell isn't there to append onto — e.g.
 * this event beat `openThread`'s own render, which shouldn't normally
 * happen but isn't worth a hard assumption. */
function appendInitialThreadReplies(rootEventId, newEvents) {
  const rp = state.rightPanel;
  if (!rp || rp.kind !== "thread" || rp.root.event_id !== rootEventId) return false;
  const msgsEl = document.getElementById("thread-messages");
  const panelBody = document.getElementById("side-panel-body");
  if (!msgsEl || !panelBody) return false;

  const threadCtx = { roomId: rp.roomId, threadId: rootEventId };
  const frag = document.createDocumentFragment();
  for (const run of groupIntoRuns(newEvents)) {
    frag.appendChild(renderMessageGroup(run, threadCtx));
  }
  msgsEl.appendChild(frag);
  scrollToBottom(panelBody);
  return true;
}

/** Older replies (`Event::ThreadEventsPrepend`) inserted above whatever's
 * already rendered, instead of the full `renderSidePanel()` rebuild that
 * used to run once per page — a long thread's initial open auto-pages
 * back to `reached_start` (see the `ThreadEvents`/`ThreadEventsPrepend`
 * handlers below), which meant the *entire* panel — compose textarea,
 * toolbar, mention state — tore down and rebuilt once per page, visibly
 * flickering the whole side panel while it caught up. Same scroll-anchor
 * idea as the main timeline's `prependMessages`: keep the reply the user
 * was reading pinned in place rather than letting inserted-above content
 * push it down the screen. `oldEvents` is `rp.events` from *before* the
 * caller merges `newEvents` into it — needed here to re-derive the old
 * first reply's grouping now that a new predecessor precedes it, and to
 * fall back to a full render if the shell isn't in the expected shape. */
function prependThreadReplies(rootEventId, newEvents, oldEvents) {
  const rp = state.rightPanel;
  if (!rp || rp.kind !== "thread" || rp.root.event_id !== rootEventId) return false;
  const msgsEl = document.getElementById("thread-messages");
  const panelBody = document.getElementById("side-panel-body");
  const hr = msgsEl?.querySelector("hr");
  if (!msgsEl || !panelBody || !hr) return false;

  const threadCtx = { roomId: rp.roomId, threadId: rootEventId };
  threadScrollAnchor.sync();

  // Same "merge across the page boundary" handling as the main timeline's
  // `prependMessages` — see its own comment for why this can't just fall
  // back to a full `renderSidePanel()` (that rebuild also resets scroll,
  // which was the actual source of the "jumps to newest reply on load
  // more" complaint this is fixing).
  let mergeEvent = null;
  let runEvents = newEvents;
  if (oldEvents.length > 0 && newEvents.length > 0 && isGrouped(newEvents[newEvents.length - 1], oldEvents[0])) {
    mergeEvent = newEvents[newEvents.length - 1];
    runEvents = newEvents.slice(0, -1);
  }
  const firstRow = hr.nextSibling;

  const frag = document.createDocumentFragment();
  for (const run of groupIntoRuns(runEvents)) {
    frag.appendChild(renderMessageGroup(run, threadCtx));
  }
  msgsEl.insertBefore(frag, hr.nextSibling);

  if (mergeEvent && firstRow) {
    const bubble = firstRow.querySelector(".bubble");
    const sender = bubble?.querySelector(".sender");
    const headerTime = sender?.querySelector("time");
    if (headerTime) headerTime.textContent = formatMessageTimestamp(mergeEvent.timestamp);
    if (bubble && sender) {
      bubble.insertBefore(renderMessageItem(mergeEvent, threadCtx), sender.nextSibling);
      firstRow.dataset.eventId = mergeEvent.event_id;
    }
  }

  // Also covers the not-yet-overflowing case the old height-delta fixup
  // skipped, which left the panel parked at the very top (the oldest
  // replies) once auto-paging filled it, instead of on the newest one.
  threadScrollAnchor.sync();
  return true;
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

  // The root is always its own single-message group/row (see the "thread"
  // branch of `renderSidePanel`) — replace the whole row, not a `.msg-item`.
  if (rp.root.event_id === eventId) {
    const row = msgsEl.querySelector(`.msg-row[data-event-id="${CSS.escape(eventId)}"]`);
    if (row) row.replaceWith(renderMessageGroup([rp.root], threadCtx));
    return;
  }
  const event = rp.events.find((e) => e.event_id === eventId);
  if (!event) return;
  const itemEl = msgsEl.querySelector(`.msg-item[data-event-id="${CSS.escape(eventId)}"]`);
  if (!itemEl) return;
  itemEl.replaceWith(renderMessageItem(event, threadCtx));
}

/** Renders the threads-list side panel's static shell (header + search
 * box + an empty `#side-panel-body`) — but only when it isn't already
 * showing the right one; a live search-box keystroke, a background
 * thread-data update, and arrow-key navigation all end up calling this
 * (via `renderSidePanel()`) far more often than the shell itself ever
 * actually needs to change. Skipping the rebuild in the common case is
 * what keeps `#threads-filter` from being destroyed and recreated
 * constantly — see `renderThreadsListRows`'s doc comment for why that
 * matters. Always hands off to `renderThreadsListRows()` for the actual
 * row content, whether or not the shell needed rebuilding. */
function renderThreadsListPanel() {
  const rp = state.rightPanel; // caller already checked kind === "threads-list"
  const shellReady =
    el.sidePanel.dataset.threadsListScope === String(rp.scope) &&
    document.getElementById("threads-filter");
  if (!shellReady) {
    el.sidePanel.dataset.threadsListScope = String(rp.scope);
    el.sidePanel.innerHTML = `<div id="side-panel-header"><span>${rp.scope === null ? "all threads" : "threads"}</span><div style="display:flex;gap:6px;"><button id="threads-mark-all-read" class="small-btn" title="mark every unread thread here as read">[ mark all read ]</button><button id="side-panel-close" class="small-btn">[x]</button></div></div>
      <div id="threads-filter-row">
        <input id="threads-filter" placeholder="search threads..." value="${escapeHtml(state.threadsListFilter)}" />
        <button id="threads-unread-only" class="small-btn${state.threadsListUnreadOnly ? " selected" : ""}">[ unread ]</button>
        <button id="threads-mentions-only" class="small-btn${state.threadsListMentionsOnly ? " selected" : ""}" title="show only unread threads that ping you">[ @mentions ]</button>
      </div>
      <div id="side-panel-body"></div>`;
    document.getElementById("side-panel-close").addEventListener("click", () => {
      state.rightPanel = null;
      renderSidePanel();
    });
    document.getElementById("threads-mark-all-read").addEventListener("click", () => {
      markAllThreadsRead(rp.scope);
    });
    document.getElementById("threads-unread-only").addEventListener("click", () => {
      state.threadsListUnreadOnly = !state.threadsListUnreadOnly;
      document.getElementById("threads-unread-only").classList.toggle("selected", state.threadsListUnreadOnly);
      renderThreadsListRows();
    });
    document.getElementById("threads-mentions-only").addEventListener("click", () => {
      state.threadsListMentionsOnly = !state.threadsListMentionsOnly;
      document.getElementById("threads-mentions-only").classList.toggle("selected", state.threadsListMentionsOnly);
      renderThreadsListRows();
    });
    const filterInput = document.getElementById("threads-filter");
    filterInput.addEventListener("input", () => {
      state.threadsListFilter = filterInput.value;
      clearTimeout(threadsFilterDebounceTimer);
      threadsFilterDebounceTimer = setTimeout(() => {
        state.threadsListActiveIndex = -1;
        renderThreadsListRows();
      }, 120);
    });
    filterInput.addEventListener("keydown", (e) => {
      if (e.key === "ArrowUp" || e.key === "ArrowDown" || e.key === "Enter") {
        if (navigateThreadsList(e.key)) e.preventDefault();
      } else if (e.key === "Escape") {
        if (filterInput.value) {
          filterInput.value = "";
          state.threadsListFilter = "";
          state.threadsListActiveIndex = -1;
          renderThreadsListRows();
        } else {
          filterInput.blur();
        }
      }
    });
  }
  renderThreadsListRows();
}

/** Rebuilds just `#side-panel-body`'s contents (the matching-thread rows,
 * "no results" placeholder, and "load more" row) for the threads-list
 * panel — never touches `#threads-filter` or anything else in the shell.
 * This used to all happen inside one full-panel rebuild that also
 * recreated the search box every time, which meant every keystroke typed
 * into it (the filter re-runs this on a 120ms debounce) tore out and
 * replaced the very input being typed into — knocking focus out of it
 * every time and, worse for anyone typing Vietnamese through an IME,
 * breaking whatever composition was in progress. Splitting the row
 * content out from the shell (see `renderThreadsListPanel`) fixes that
 * at the root instead of trying to save/restore focus around a rebuild
 * that didn't need to touch the input at all. */
/** A thread-row preview (the root message, or its bundled latest-reply)
 * as HTML — an actual thumbnail for an image attachment, same fetch/cache
 * pipeline `renderMessage`'s inline images and the pinned-messages dialog
 * (`openPinsDialog`, above) already use, instead of just the filename
 * text `body` alone would show; markdown-rendered text (`renderMarkdown`,
 * for bold/italic/code/links) for anything else. `null` when `msgType`
 * isn't `"image"` — callers fall back to their own plain-text render. */
function threadRowImageHtml(msgType, mediaUrl, mediaMime, mediaEncryption, altText) {
  if (msgType !== "image" || !mediaUrl) return null;
  if (mediaMime) state.imageMime[mediaUrl] = mediaMime;
  if (mediaEncryption) state.imageEncryption[mediaUrl] = mediaEncryption;
  const cached = state.imageCache[mediaUrl];
  if (cached) {
    return `<img src="${cached}" alt="${escapeHtml(altText || "image")}" class="thread-row-thumb" />`;
  }
  if (!state.imageRequested.has(mediaUrl)) requestImage(mediaUrl);
  return loadingHtml("loading image...");
}

function renderThreadsListRows() {
  const rp = state.rightPanel;
  if (!rp || rp.kind !== "threads-list") return;
  const bodyEl = document.getElementById("side-panel-body");
  if (!bodyEl) return;

  const unreadOnly = state.threadsListUnreadOnly;
  const mentionsOnly = state.threadsListMentionsOnly;
  const query = normalizeForSearch(state.threadsListFilter.trim());
  /** A thread matches if the room it's in, its first message, or its
   * latest reply mention the search text — covers "I remember someone
   * said X" without needing to know which of those two messages it was
   * in, or opening the thread to check. */
  const threadMatches = (roomName, t) =>
    !query ||
    [roomName, t.sender_name, t.body, t.latest_reply_sender_name, t.latest_reply_body]
      .filter(Boolean)
      .some((s) => normalizeForSearch(s).includes(query));

  let rooms = Object.entries(state.threadsByRoom)
    .filter(([roomId]) => rp.scope === null || roomId === rp.scope)
    .map(([roomId, threads]) => {
      const roomName = state.rooms.find((r) => r.room_id === roomId)?.name || roomId;
      const filtered = threads.filter(
        (t) =>
          (!unreadOnly || isThreadUnread(roomId, t)) &&
          (!mentionsOnly || (isThreadUnread(roomId, t) && isThreadMentioningMe(roomId, t))) &&
          threadMatches(roomName, t),
      );
      return { roomId, roomName, threads: filtered };
    })
    .filter((r) => r.threads.length > 0);

  // A thread's own activity time (latest reply if it has one, its own
  // send time otherwise) sorts threads *within* a room; a room's most
  // recent such time — across all of its own matching threads — sorts
  // the rooms themselves, so the whole panel still surfaces recent
  // activity near the top even though it's grouped by room rather than
  // one flat interleaved-by-time list.
  const activityOf = (t) => t.latest_reply_ts ?? t.timestamp;
  for (const r of rooms) {
    r.threads.sort((a, b) => activityOf(b) - activityOf(a));
  }
  rooms.sort((a, b) => activityOf(b.threads[0]) - activityOf(a.threads[0]));

  const totalThreads = rooms.reduce((n, r) => n + r.threads.length, 0);
  let html = "";
  if (totalThreads === 0) {
    html += `<div style="color:var(--text-weak)">${
      query
        ? "no threads match your search"
        : mentionsOnly
          ? "no unread mentions in threads"
          : unreadOnly
            ? "no unread threads"
            : "no threads yet"
    }</div>`;
  }
  state.visibleThreadRows = [];
  for (const r of rooms) {
    // Only meaningful once threads from different rooms are mixed
    // together — the room-scoped view (`rp.scope` a single room id) is
    // already unambiguous without repeating that room's own name back at
    // the top of it.
    if (rp.scope === null) html += `<div class="thread-room-name">${escapeHtml(r.roomName)}</div>`;
    for (const t of r.threads) {
      const roomId = r.roomId;
      const rowIndex = state.visibleThreadRows.length;
      state.visibleThreadRows.push({ roomId, eventId: t.event_id });
      const unread = isThreadUnread(roomId, t);
      const mentioned = unread && isThreadMentioningMe(roomId, t);
      // "First message" is the thread root itself (`t`); "last message"
      // is its bundled latest-reply preview (see `latest_reply_*` on
      // `TimelineEvent` — comes straight off the root event's own
      // server-side aggregation, no per-thread fetch needed just to
      // list them). Absent for a thread with 0 replies.
      const lastMsgBody =
        threadRowImageHtml(t.latest_reply_msg_type, t.latest_reply_media_url, t.latest_reply_media_mime, t.latest_reply_media_encryption, t.latest_reply_body) ??
        renderMarkdownPreview(t.latest_reply_body || "", 80);
      const lastMsgHtml = t.latest_reply_body
        ? `<div class="thread-row-last"><span style="color:${senderColor(t.sender)}">${escapeHtml(t.latest_reply_sender_name || "")}:</span> ${lastMsgBody}</div>`
        : "";
      const rootBody = threadRowImageHtml(t.msg_type, t.media_url, t.media_mime, t.media_encryption, t.body) ?? renderMarkdownPreview(t.body || "", 80);
      // Same avatar-left, sender+content-right layout as a message row in
      // the timeline (`renderMessage`) — reads as "a message that happens
      // to have a thread on it" instead of a plain unrelated text-list
      // entry once the rest of the app moved to that flat, avatar-led
      // message style.
      html += `<div class="thread-row${rowIndex === state.threadsListActiveIndex ? " kbd-active" : ""}" data-room="${roomId}" data-event="${t.event_id}">
      ${avatarHtml(t.sender_avatar_url, t.sender_name, t.sender, 28)}
      <div class="thread-row-content">
        <div class="sender" style="color:${senderColor(t.sender)}">${mentioned ? '<span class="mention-dot">●</span> ' : unread ? '<span class="unread-dot">●</span> ' : ""}${escapeHtml(t.sender_name)}</div>
        <div class="thread-row-body">${rootBody}</div>
        ${lastMsgHtml}
        <div class="thread-row-meta">
          <span>${t.thread_count || 0} replies →</span>
          ${unread ? '<button class="small-btn thread-mark-read-btn" data-mark-read title="mark this thread as read">✓ read</button>' : ""}
        </div>
      </div>
    </div>`;
    }
  }
  if (state.threadsListActiveIndex >= state.visibleThreadRows.length) {
    state.threadsListActiveIndex = state.visibleThreadRows.length - 1;
  }
  // Pagination only makes sense for one room's own (unfiltered) thread
  // list — the all-rooms view is already narrowed to unread threads only,
  // which `state.unreadThreads` tracks live rather than needing a deeper
  // fetch. `rooms.length === 1` here whenever `rp.scope` is a room id,
  // since the filter above already narrowed to just that room. Hidden
  // while a search is active — "load more" fetches more threads from the
  // server, which has nothing to do with what's already loaded just not
  // matching the current search text.
  const scopedRoom = rp.scope !== null && !query ? rooms[0] : null;
  if (scopedRoom && !state.threadsListReachedEnd.has(rp.scope)) {
    const loading = state.threadsListPaginationInFlight.has(rp.scope);
    html += `<div id="threads-load-more-row" style="text-align:center;font-size:calc(var(--font-size) - 3px);padding:4px;">${
      loading ? loadingHtml("loading more threads...") : '<button id="threads-load-more-btn" class="small-btn">load more threads</button>'
    }</div>`;
  }
  bodyEl.innerHTML = html;
  bodyEl.querySelectorAll(".thread-row").forEach((row) => {
    row.addEventListener("click", () => {
      const roomId = row.dataset.room;
      const eventId = row.dataset.event;
      const root = state.threadsByRoom[roomId]?.find((t) => t.event_id === eventId);
      if (root) openThread(roomId, root);
    });
  });
  bodyEl.querySelectorAll(".thread-mark-read-btn").forEach((btn) => {
    // Stop the click from bubbling up to the row's own listener above —
    // this button marks the thread read in place, it shouldn't also open
    // it (that would immediately mark it read too, just less directly).
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      const row = btn.closest(".thread-row");
      const roomId = row.dataset.room;
      const eventId = row.dataset.event;
      const root = state.threadsByRoom[roomId]?.find((t) => t.event_id === eventId);
      if (root) markThreadReadFromList(roomId, root);
    });
  });
  document.getElementById("threads-load-more-btn")?.addEventListener("click", () => {
    state.threadsListPaginationInFlight.add(rp.scope);
    send("LoadMoreThreads", { room_id: rp.scope });
    renderThreadsListRows();
  });
}

/** Keeps `#side-panel`'s inline width and `#resizer-right`'s visibility
 * matching whichever of the three layouts (phone full-screen overlay /
 * medium `#main-panel`-covering overlay / desktop 3-pane — see their
 * matching media queries in style.css) the window's current size falls
 * into. Below `720px`/`500px` `#side-panel` is a full-screen overlay
 * (`width: 100%`), and between that and `1100px` it's an overlay
 * anchored to the right, covering the open conversation while
 * `#room-list` stays visible on the left, with its own fixed width —
 * setting an
 * inline pixel width would outrank either unconditionally (inline styles
 * beat any stylesheet rule regardless of media query) and break it, so a
 * custom width is only ever computed for the genuine 3-pane desktop
 * layout below.
 *
 * Called both from `renderSidePanel` (every open/re-render) and, since a
 * plain OS-level window resize while the panel is already open doesn't
 * trigger a re-render on its own, from a debounced `resize` listener too
 * — without that second path, opening the panel at a wide window width
 * and then resizing/tiling the window down (to "half screen", say) left
 * that wide layout's inline pixel width in place fighting the
 * narrow/medium stylesheet rule for the rest of the session, rendering
 * an overlay far wider than either was ever meant to allow. */
function syncSidePanelLayoutForViewport() {
  if (!state.rightPanel) return;
  const resizerRight = document.getElementById("resizer-right");
  const narrow = isNarrowLayout();
  // `!narrow` already excludes every touch device here too (not just
  // ones outside 721–1100px) — `isNarrowLayout()` matches any touch
  // device regardless of width, so this stays correctly desktop-only.
  const isMediumLayout = !narrow && window.matchMedia("(max-width: 1100px)").matches;
  if (narrow || isMediumLayout) {
    el.sidePanel.style.width = "";
    resizerRight.style.display = "none";
    return;
  }
  resizerRight.style.display = "block";
  if (el.sidePanel.style.display !== "flex") {
    // Default to splitting the space evenly with the timeline instead of
    // a fixed width — measured right before `#side-panel` starts taking
    // up any room, so `#main-panel` (`flex: 1`) still reflects the full
    // width available to both of them at this instant. Only on the
    // closed→open transition (this check), not on every subsequent
    // re-render while it's already showing, so it doesn't fight a resize
    // the user just did by hand.
    const availableWidth = el.mainPanel.getBoundingClientRect().width;
    // `resizerRight` may still be `display: none` at this point, so it
    // has no measurable width of its own yet — `.resizer`'s CSS width is
    // a fixed 4px regardless, so just use that directly.
    const resizerWidth = 4;
    const min = parseInt(getComputedStyle(el.sidePanel).minWidth, 10) || 220;
    el.sidePanel.style.width = Math.max(min, (availableWidth - resizerWidth) / 2) + "px";
  }
}

let sidePanelResizeDebounceTimer = null;
window.addEventListener("resize", () => {
  clearTimeout(sidePanelResizeDebounceTimer);
  sidePanelResizeDebounceTimer = setTimeout(syncSidePanelLayoutForViewport, 100);
});

function renderSidePanel() {
  const rp = state.rightPanel;
  const resizerRight = document.getElementById("resizer-right");
  if (!rp) {
    el.sidePanel.style.display = "none";
    el.sidePanel.innerHTML = "";
    resizerRight.style.display = "none";
    return;
  }
  // Before flipping `display` to `"flex"` below — `syncSidePanelLayoutForViewport`'s
  // desktop-width branch uses "is this element not already `flex`" to
  // detect a closed→open transition (only then does it compute a fresh
  // default width), which would never see anything but "already flex"
  // if this ran after.
  syncSidePanelLayoutForViewport();
  el.sidePanel.style.display = "flex";

  if (rp.kind === "threads-list") {
    renderThreadsListPanel();
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

    let html = `<div id="side-panel-header"><span>thread</span><div style="display:flex;gap:6px;"><button id="thread-btn-mark-read" class="small-btn" title="mark this thread as read">[ mark read ]</button><button id="thread-btn-summarize" class="small-btn">[ summarize ]</button><button id="side-panel-close" class="small-btn">[x]</button></div></div>
      <div id="side-panel-body" tabindex="-1"><div id="thread-messages"></div></div>
      <div id="thread-reply-indicator" style="display:none;"></div>
      <div id="thread-mention-suggestions" style="display:none;"></div>
      <div id="thread-pending-image-preview" style="display:none;padding:6px 12px;"></div>
      <div id="thread-pending-file-preview" style="display:none;padding:6px 12px;"></div>
      <div id="thread-compose-toolbar" class="compose-toolbar" style="padding:4px 8px 0 8px;border-top:1px solid var(--border);">
        <button type="button" class="md-btn" data-md="bold" title="bold (Ctrl+B)"><b>B</b></button>
        <button type="button" class="md-btn" data-md="italic" title="italic (Ctrl+I)"><i>I</i></button>
        <button type="button" class="md-btn" data-md="code" title="inline code (Ctrl+E)">code</button>
        <button type="button" class="md-btn" data-md="codeblock" title="code block (Ctrl+Shift+E)">{ }</button>
        <button type="button" class="md-btn" data-md="link" title="link">link</button>
        <button type="button" class="md-btn emoji-btn" title="emoji">🙂</button>
      </div>
      <div id="thread-compose-row" style="padding:8px;display:flex;gap:6px;align-items:flex-end;">
        <button id="thread-btn-attach" class="small-btn">📎</button>
        <div id="thread-meme-picker-wrap">
          <button id="thread-btn-meme" class="small-btn" title="send a custom emoji/meme">🐸</button>
          <div id="thread-meme-picker" style="display:none;"></div>
        </div>
        <div class="compose-editor-wrap">
          <textarea id="thread-compose-input" class="compose-textarea" rows="1" placeholder="reply... (@ to mention, **bold**, *italic*, \`code\`)"></textarea>
        </div>
        <button id="thread-compose-send">send</button>
      </div>`;
    el.sidePanel.innerHTML = html;
    document.getElementById("side-panel-close").addEventListener("click", () => {
      state.rightPanel = rp.cameFrom || null;
      renderSidePanel();
    });
    document.getElementById("thread-btn-summarize").addEventListener("click", () => {
      openSummaryDialog(rp.roomId, rp.root.event_id);
    });
    document.getElementById("thread-btn-mark-read").addEventListener("click", markCurrentThreadRead);
    const msgsEl = document.getElementById("thread-messages");
    const threadCtx = { roomId: rp.roomId, threadId: rp.root.event_id };
    msgsEl.appendChild(renderMessageGroup([rp.root], threadCtx));
    const hr = document.createElement("hr");
    hr.style.borderColor = "var(--border)";
    msgsEl.appendChild(hr);
    for (const run of groupIntoRuns(rp.events)) {
      msgsEl.appendChild(renderMessageGroup(run, threadCtx));
    }
    // `#thread-messages` itself doesn't scroll — its parent
    // `#side-panel-body` does (`overflow-y: auto`) — so that's what needs
    // scrolling to the newest reply, same as the main timeline does.
    const panelBody = document.getElementById("side-panel-body");
    scrollToBottom(panelBody);

    const threadSuggestionsEl = document.getElementById("thread-mention-suggestions");
    const threadInputEl = document.getElementById("thread-compose-input");
    wireMentionAutocomplete(threadInputEl, threadSuggestionsEl, () => rp.roomId, state.threadComposeMentions);
    renderPendingImage();
    renderPendingFile();

    const sendThreadMsg = () => {
      if (state.pendingImage && state.pendingImage.threadId === rp.root.event_id) {
        confirmPendingImage();
        return;
      }
      if (state.pendingFile && state.pendingFile.threadId === rp.root.event_id) {
        confirmPendingFile();
        return;
      }
      const input = document.getElementById("thread-compose-input");
      const body = input.value.trim();
      if (!body) return;
      input.value = "";
      autoResizeTextarea(input);
      threadSuggestionsEl.style.display = "none";
      const { ids, html } = buildMentionHtml(body, state.threadComposeMentions);
      // In-place clear, same reasoning as the main compose box's two spots
      // above — a live reply/edit echo appends in place (`appendThreadMessage`)
      // rather than rebuilding the whole panel every time (see its own doc
      // comment), so this array can easily outlive any one `renderSidePanel()`
      // call's `wireMentionAutocomplete` closure too.
      state.threadComposeMentions.length = 0;

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
      const localId = crypto.randomUUID();
      const optimistic = makeOptimisticEvent(localId, body, replyTo, ids);
      if (optimistic) {
        state.pendingSends.set(localId, { roomId: rp.roomId, threadId: rp.root.event_id });
        rp.events.push(optimistic);
        appendThreadMessage(rp.root.event_id, optimistic);
      }
      send("SendMessage", {
        room_id: rp.roomId,
        body,
        thread_id: rp.root.event_id,
        mentions: ids,
        html_body: html,
        local_id: localId,
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
    document.getElementById("thread-btn-attach").addEventListener("click", (e) => {
      e.stopPropagation();
      const pickImage = () => {
        const input = document.createElement("input");
        input.type = "file";
        input.accept = "image/*";
        input.addEventListener("change", () => {
          if (input.files[0]) loadPendingImage(input.files[0], rp.roomId, rp.root.event_id);
        });
        input.click();
      };
      const pickFile = () => {
        const input = document.createElement("input");
        input.type = "file";
        input.addEventListener("change", () => {
          if (input.files[0]) loadPendingFile(input.files[0], rp.roomId, rp.root.event_id);
        });
        input.click();
      };
      openActionsMenu(e.currentTarget, [
        { label: "📎 image", onClick: pickImage },
        { label: "📄 file", onClick: pickFile },
      ]);
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

  if (rp.kind === "user-search") {
    let html = `<div id="side-panel-header"><span>messages from ${escapeHtml(rp.userId)}</span><button id="side-panel-close" class="small-btn">[x]</button></div><div id="side-panel-body">`;
    if (rp.loading) {
      html += `<div style="padding:12px;text-align:center;">${loadingHtml("scanning every room, this can take a while...")}</div>`;
    } else {
      if (rp.truncated) {
        html += `<div style="color:var(--text-weak);font-size:12px;margin-bottom:8px;">some very active rooms may be missing older messages — narrow the date range for more complete results</div>`;
      }
      if (rp.results.length === 0) {
        html += `<div style="color:var(--text-weak)">no messages found from this user in this date range</div>`;
      }
      // One room-name header per *run* of consecutive same-room hits (not
      // once per message) — same idea as the main timeline grouping
      // consecutive messages from the same sender.
      let lastRoomId = null;
      for (const hit of rp.results) {
        if (hit.room_id !== lastRoomId) {
          html += `<div class="thread-room-name">${escapeHtml(hit.room_name)}</div>`;
          lastRoomId = hit.room_id;
        }
        html += `<div class="thread-row" data-room="${hit.room_id}" data-event="${hit.event.event_id}">
          ${avatarHtml(hit.event.sender_avatar_url, hit.event.sender_name, hit.event.sender, 28)}
          <div class="thread-row-content">
            <div class="sender" style="color:${senderColor(hit.event.sender)}">${escapeHtml(hit.event.sender_name)}</div>
            <div class="thread-row-body">${renderMarkdownPreview(hit.event.body || "", 120)}</div>
            <div class="thread-row-meta">${new Date(hit.event.timestamp).toLocaleString([], { dateStyle: "medium", timeStyle: "short" })}</div>
          </div>
        </div>`;
      }
    }
    html += "</div>";
    el.sidePanel.innerHTML = html;
    document.getElementById("side-panel-close").addEventListener("click", () => {
      state.rightPanel = null;
      renderSidePanel();
    });
    el.sidePanel.querySelectorAll(".thread-row").forEach((row) => {
      row.addEventListener("click", () => {
        openMatrixToLink(row.dataset.room, row.dataset.event);
      });
    });
  }

  if (rp.kind === "security") {
    renderSecurityPanel();
  }
  if (rp.kind === "profile") {
    renderProfilePanel();
  }
}

// =========================================================================
// Own profile — avatar / display name
// =========================================================================

el.btnProfile.addEventListener("click", () => {
  closeChatsMenu();
  state.rightPanel = { kind: "profile" };
  state.ownProfile = null;
  send("GetOwnProfile");
  renderSidePanel();
});

/** Small circular avatar element — `mxcUri` is `TimelineEvent.sender_avatar_url`
 * / `OwnProfile.avatar_url` (or null/undefined for "no avatar set"). Falls
 * back to a colored circle with the name's first letter while the real
 * image is loading (or if there is none), same idea as `senderColor` gives
 * every sender a stable color for their name — reused here so a given
 * person's fallback circle and their sender-name color always match. `size`
 * is in px; defaults to the message-row avatar size. */
function renderAvatar(mxcUri, name, idForColor, size = 28) {
  const wrap = document.createElement("div");
  wrap.className = "avatar";
  wrap.style.width = `${size}px`;
  wrap.style.height = `${size}px`;
  wrap.style.fontSize = `${Math.round(size * 0.45)}px`;

  const cached = mxcUri && state.imageCache[mxcUri];
  if (cached) {
    const img = document.createElement("img");
    img.src = cached;
    img.alt = name || "";
    wrap.appendChild(img);
  } else {
    wrap.style.background = senderColor(idForColor || name || "");
    wrap.textContent = (name || "?").trim().charAt(0).toUpperCase() || "?";
    if (mxcUri) {
      // Lets `patchAvatarsWithImage` find every avatar slot showing this
      // mxc (a sender can have several on screen at once — one per
      // message group) once `Command::FetchImage` answers, without
      // needing a full re-render of whatever contains them.
      wrap.dataset.mxc = mxcUri;
      requestImage(mxcUri);
    }
  }
  return wrap;
}

/** String-HTML counterpart to `renderAvatar` just above — for the
 * threads-list panel's rows, built as one big HTML string
 * (`renderThreadsListRows`) rather than per-row DOM node construction
 * like `renderMessage`. Same caching/fallback-initial logic and the same
 * `data-mxc` hook, so `patchAvatarsWithImage` (which just queries
 * `.avatar[data-mxc="..."]` anywhere in the document) finds and swaps
 * these in exactly the same way once the real image loads. */
function avatarHtml(mxcUri, name, idForColor, size = 28) {
  const style = `width:${size}px;height:${size}px;font-size:${Math.round(size * 0.45)}px;`;
  const cached = mxcUri && state.imageCache[mxcUri];
  if (cached) {
    return `<div class="avatar" style="${style}"><img src="${escapeHtml(cached)}" alt="${escapeHtml(name || "")}" /></div>`;
  }
  if (mxcUri) requestImage(mxcUri);
  const initial = escapeHtml((name || "?").trim().charAt(0).toUpperCase() || "?");
  const mxcAttr = mxcUri ? ` data-mxc="${escapeHtml(mxcUri)}"` : "";
  return `<div class="avatar" style="${style}background:${senderColor(idForColor || name || "")};"${mxcAttr}>${initial}</div>`;
}

/** Swaps the initials-fallback circle for the real image, in place, on
 * every currently-rendered avatar slot for `mxcUri` — same "patch, don't
 * re-render" reasoning as `patchMessagesWithMedia`. */
function patchAvatarsWithImage(mxcUri) {
  const cached = state.imageCache[mxcUri];
  if (!cached) return;
  document.querySelectorAll(`.avatar[data-mxc="${CSS.escape(mxcUri)}"]`).forEach((wrap) => {
    delete wrap.dataset.mxc;
    wrap.style.background = "";
    // Not `wrap.textContent = ""` — that clears every child, not just the
    // initials text node, silently taking the presence badge (a real
    // `<span>`, see `renderMessageGroup`'s `.avatar-presence-dot`) out
    // with it the moment the real avatar image finished loading.
    const presenceDot = wrap.querySelector(".avatar-presence-dot");
    wrap.innerHTML = "";
    if (presenceDot) wrap.appendChild(presenceDot);
    const img = document.createElement("img");
    img.src = cached;
    wrap.appendChild(img);
  });
}

function renderProfilePanel() {
  const p = state.ownProfile;
  let html = `<div id="side-panel-header"><span>profile</span><button id="side-panel-close" class="small-btn">[x]</button></div>
    <div id="side-panel-body">`;
  if (!p) {
    html += `<div style="color:var(--text-weak);font-size:12px;">loading...</div></div>`;
    el.sidePanel.innerHTML = html;
    document.getElementById("side-panel-close").addEventListener("click", () => {
      state.rightPanel = null;
      renderSidePanel();
    });
    return;
  }

  html += `<div id="profile-avatar-row" style="display:flex;align-items:center;gap:12px;">
      <div id="profile-avatar-slot"></div>
      <div>
        <div style="font-size:12px;color:var(--text-weak);">${escapeHtml(p.user_id)}</div>
        <button id="profile-change-avatar" class="small-btn" ${state.ownProfileSaving ? "disabled" : ""}>change avatar...</button>
      </div>
    </div>
    <hr style="border-color:var(--border);margin:14px 0;">
    <label style="font-size:12px;color:var(--text-weak);">display name</label>
    <input type="text" id="profile-display-name" style="width:100%;margin-top:4px;" value="${escapeHtml(p.display_name)}" ${state.ownProfileSaving ? "disabled" : ""} />
    <button id="profile-save-name" style="width:100%;margin-top:8px;" ${state.ownProfileSaving ? "disabled" : ""}>save name</button>
    <div style="margin-top:10px;font-size:11px;color:var(--text-weak);">
      changing your name or avatar here updates it everywhere (Element included) — other members' clients decide on their own how to show that change in a room's timeline, this app has no control over that part.
    </div>`;
  html += "</div>";
  el.sidePanel.innerHTML = html;

  document.getElementById("profile-avatar-slot").appendChild(
    renderAvatar(p.avatar_url, p.display_name, p.user_id, 56),
  );

  document.getElementById("side-panel-close").addEventListener("click", () => {
    state.rightPanel = null;
    renderSidePanel();
  });
  document.getElementById("profile-change-avatar").addEventListener("click", () => {
    el.avatarFileInput.click();
  });
  document.getElementById("profile-save-name").addEventListener("click", () => {
    const name = document.getElementById("profile-display-name").value.trim();
    if (!name || name === p.display_name) return;
    state.ownProfileSaving = true;
    renderSidePanel();
    send("SetDisplayName", { name });
  });
}

el.avatarFileInput.addEventListener("change", () => {
  const file = el.avatarFileInput.files[0];
  el.avatarFileInput.value = "";
  if (!file) return;
  const reader = new FileReader();
  reader.onload = () => {
    const bytes = Array.from(new Uint8Array(reader.result));
    state.ownProfileSaving = true;
    if (state.rightPanel?.kind === "profile") renderSidePanel();
    send("SetAvatar", { bytes, mime: file.type || "image/png" });
  };
  reader.readAsArrayBuffer(file);
});

// =========================================================================
// Security / verification
// =========================================================================

el.btnSecurity.addEventListener("click", () => {
  closeChatsMenu();
  state.rightPanel = { kind: "security" };
  state.lvxApiKeyConfigured = null;
  send("GetLvxApiKeyStatus");
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
  html += `<hr style="border-color:var(--border);margin:14px 0;">
    <label style="font-size:12px;color:var(--text-weak);">LVX API key (for [ summarize ])</label>
    <div style="font-size:11px;color:var(--text-weak);margin:2px 0 6px;">${
      state.lvxApiKeyConfigured === null
        ? "checking..."
        : state.lvxApiKeyConfigured
          ? "currently configured — paste a new one to replace it, or clear below"
          : "not set — summarize will fail until one's set here or via the LVX_API_KEY env var"
    }</div>
    <input type="password" id="sec-lvx-api-key" style="width:100%;" placeholder="sk-..." autocomplete="off" />
    <div style="display:flex;gap:8px;margin-top:8px;">
      <button id="sec-lvx-api-key-save" style="flex:1;">save</button>
      <button id="sec-lvx-api-key-clear" style="flex:1;">clear</button>
    </div>`;
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
  document.getElementById("sec-lvx-api-key-save").addEventListener("click", () => {
    const input = document.getElementById("sec-lvx-api-key");
    const key = input.value.trim();
    if (!key) return;
    send("SetLvxApiKey", { api_key: key });
    input.value = "";
  });
  document.getElementById("sec-lvx-api-key-clear").addEventListener("click", () => {
    send("SetLvxApiKey", { api_key: "" });
    document.getElementById("sec-lvx-api-key").value = "";
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

/** Overrides the built-in `:root` palette (see `style.css`) with the
 * running GTK/Sway theme's own resolved colors — sent once at startup and
 * again on every live theme switch (see `gtk_theme.rs`, Linux desktop
 * only). Inline styles on `documentElement` outrank the stylesheet
 * regardless of specificity, so this is enough on its own; nothing in
 * `style.css` needs to change. `--danger` and `--radius` are deliberately
 * left alone — GTK themes don't reliably name an equivalent for either,
 * and the built-in values for both already read fine against an
 * arbitrary theme's bg/text. */
function applySystemTheme(theme) {
  const root = document.documentElement.style;
  root.setProperty("--bg", theme.bg);
  root.setProperty("--bg-alt", theme.bg_alt);
  root.setProperty("--border", theme.border);
  root.setProperty("--text", theme.text);
  root.setProperty("--text-weak", theme.text_weak);
  root.setProperty("--accent", theme.accent);
  root.setProperty("--accent-strong", theme.accent_strong);
  // Native form-control chrome (scrollbars, checkboxes, ...) needs its
  // own light/dark hint independent of the custom properties above —
  // derived from the theme's actual background rather than assumed,
  // since a GTK theme's "dark" *name* and its resolved bg color don't
  // always agree (a light theme with a dark accent, for instance).
  const [r, g, b] = [1, 3, 5].map((i) => parseInt(theme.bg.slice(i, i + 2), 16));
  const luminance = (0.299 * r + 0.587 * g + 0.114 * b) / 255;
  document.documentElement.style.colorScheme = luminance < 0.5 ? "dark" : "light";
}

function handleBackendEvent(evt) {
  const { type, data } = evt;
  switch (type) {
    case "SystemTheme":
      // An explicit user choice (see `applyThemeOverride`) always wins
      // over the GTK-driven system theme — otherwise every live theme
      // switch on the Linux desktop build would silently undo it.
      if (!state.themeOverride) applySystemTheme(data);
      break;
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
    case "SessionExpired":
      reloadToLogin("your session has expired — please sign in again");
      break;
    case "LoggedOut":
      reloadToLogin(null);
      break;
    case "RoomListUpdate": {
      const entries = data.list === "Invites" ? state.inviteEntries : state.roomEntries;
      for (const op of data.ops) applyRoomListOp(entries, op);
      rebuildRoomsFromEntries();
      scheduleRoomsRender();
      if (data.list !== "Invites") scheduleGrowRoomList();
      // First time the room list actually has rooms in it — kick off a
      // one-time background fetch of every room's threads (see
      // `scanAllRoomThreads`), same reasoning as the room list's own
      // startup refresh: "all threads" should have real data the moment
      // it's opened, not just whatever `ThreadReply` happened to arrive
      // live since launch. Note this only ever sees rooms actually loaded
      // into the current page/growing-batch so far, not necessarily every
      // room on the account yet — same limitation as
      // `Command::ListAllUsers`/`SearchUserMessages` on a very large
      // account.
      if (!state.threadsScanStarted && state.rooms.length > 0) {
        state.threadsScanStarted = true;
        scanAllRoomThreads();
      }
      break;
    }
    case "Spaces":
      state.spaces = data;
      renderSpacePicker();
      if (state.reloadingRooms) {
        state.reloadingRooms = false;
        el.btnReloadRooms.disabled = false;
        el.btnReloadRooms.textContent = "[ reload ]";
        showToast("spaces reloaded");
      }
      break;
    case "Timeline":
      state.timelines[data.room_id] = data.events;
      state.timelineLoaded.add(data.room_id);
      if (data.room_id === state.selectedRoom) {
        renderTimeline();
        maybeAutoLoadMore(data.room_id);
      }
      if (state.pendingScrollTarget?.roomId === data.room_id) {
        const eventId = state.pendingScrollTarget.eventId;
        state.pendingScrollTarget = null;
        // `renderTimeline()` just ran synchronously above, but give the
        // browser a tick to actually paint before measuring for scroll.
        setTimeout(() => findAndScrollToMessage(data.room_id, eventId), 50);
      }
      if (state.pendingThreadLink?.roomId === data.room_id) {
        const { threadRootId, eventId } = state.pendingThreadLink;
        setTimeout(() => resolveThreadLink(data.room_id, threadRootId, eventId), 50);
      }
      break;
    case "TimelinePrepend": {
      state.paginationInFlight.delete(data.room_id);
      if (data.reached_start) state.reachedStart.add(data.room_id);
      const prevEvents = state.timelines[data.room_id] || [];
      state.timelines[data.room_id] = data.events;
      if (data.room_id === state.selectedRoom) {
        prependMessages(data.room_id, prevEvents, data.events);
        maybeAutoLoadMore(data.room_id);
      }
      if (state.pendingScrollSearch?.roomId === data.room_id) {
        const eventId = state.pendingScrollSearch.eventId;
        setTimeout(() => findAndScrollToMessage(data.room_id, eventId), 50);
      }
      if (state.pendingThreadLink?.roomId === data.room_id) {
        const { threadRootId, eventId } = state.pendingThreadLink;
        setTimeout(() => resolveThreadLink(data.room_id, threadRootId, eventId), 50);
      }
      break;
    }
    case "NewMessage": {
      if (!state.timelines[data.room_id]) state.timelines[data.room_id] = [];
      // `data.event.local_id` only round-trips down `/sync` to the exact
      // device that sent it (see `TimelineEvent.local_id`'s doc comment on
      // the Rust side) — its presence in `state.pendingSends` confirms
      // this is the real echo of a message *this session* just sent, so
      // swap it in for the optimistic "sending…" bubble instead of
      // showing the same message twice.
      const reconciled =
        data.event.local_id &&
        state.pendingSends.delete(data.event.local_id) &&
        reconcileLocalEcho(
          state.timelines[data.room_id],
          data.room_id === state.selectedRoom ? el.timeline : null,
          data.event.local_id,
          data.event,
          { roomId: data.room_id, threadId: null },
        );
      if (!reconciled) {
        state.timelines[data.room_id].push(data.event);
        if (data.room_id === state.selectedRoom) {
          appendMessage(data.room_id, data.event);
        }
      }
      // Update the room list's preview text/ordering/unread badge right
      // as the message arrives, instead of waiting for the backend's own
      // debounced `Event::Rooms` refresh (up to ~800ms later) — which,
      // for a room that's never been opened this session, can't compute
      // an up-to-date preview at all (`Room::latest_event()` needs
      // machinery this app doesn't run; see `refresh_rooms`'s comment on
      // the backend side). Without this, a new message in a room you
      // haven't opened just silently didn't move it up or show a preview
      // until you happened to trigger some other room-list refresh.
      const room = state.rooms.find((r) => r.room_id === data.room_id);
      if (room) {
        room.last_message = data.event.body;
        room.last_message_ts = data.event.timestamp;
        // Bumped unconditionally, even for the room you're currently
        // looking at — read state is only ever cleared by explicitly
        // pressing "[ mark read ]" (see `el.btnMarkRead`'s handler), not
        // just by a message happening to arrive while the room's open.
        room.unread_count = (room.unread_count || 0) + 1;
        knownUnreadCounts.set(room.room_id, room.unread_count);
        if (data.event.mentions_me) {
          room.mention_count = (room.mention_count || 0) + 1;
          knownMentionCounts.set(room.room_id, room.mention_count);
        }
        // Same ordering `sortRoomsList` uses: invites first, then
        // favorites, then most recent activity.
        state.rooms.sort(
          (a, b) => (b.is_invite - a.is_invite) || (b.is_favorite - a.is_favorite) || b.last_message_ts - a.last_message_ts,
        );
        renderRooms();
      }
      maybeNotify(data.room_id, data.event);
      break;
    }
    // A read receipt landed for this room — including this account's own
    // receipt echoing back after being sent from a *different* session
    // (marking read on another device/platform), which is exactly what
    // this exists to catch: `applyUnreadFloor`'s protection (see its own
    // comment) would otherwise clamp that drop right back up, since from
    // its point of view a badge suddenly going down looks exactly like
    // the stale-diff race it guards against. Treated the same way
    // `btnMarkRead`'s own click handler exempts itself from that — set
    // `knownUnreadCounts`/`knownMentionCounts` directly rather than
    // going through `applyUnreadFloor`, since this value just came
    // straight from a fresh `num_unread_notifications()` recompute on
    // the Rust side, not a possibly-stale room-list diff.
    case "UnreadCountChanged": {
      const room = state.rooms.find((r) => r.room_id === data.room_id);
      if (room) {
        room.unread_count = data.unread_count;
        room.mention_count = data.mention_count;
        knownUnreadCounts.set(data.room_id, data.unread_count);
        knownMentionCounts.set(data.room_id, data.mention_count);
        renderRooms();
      }
      break;
    }
    case "ThreadReply": {
      const key = `${data.room_id}|${data.thread_root_id}`;
      const openHere =
        state.rightPanel &&
        state.rightPanel.kind === "thread" &&
        state.rightPanel.root.event_id === data.thread_root_id;
      if (openHere) {
        // Same `local_id` reconciliation as `NewMessage` above, just
        // against the thread panel's own `events` array/DOM instead of
        // the main timeline's.
        const reconciled =
          data.event.local_id &&
          state.pendingSends.delete(data.event.local_id) &&
          reconcileLocalEcho(
            state.rightPanel.events,
            document.getElementById("thread-messages"),
            data.event.local_id,
            data.event,
            { roomId: data.room_id, threadId: data.thread_root_id },
          );
        if (!reconciled) {
          state.rightPanel.events.push(data.event);
          appendThreadMessage(data.thread_root_id, data.event);
        }
        state.rightPanel.root.thread_count = (state.rightPanel.root.thread_count || 0) + 1;
      } else if (data.event.local_id) {
        // The thread panel isn't open (any more) to reconcile into, but
        // the pending-send entry would otherwise just sit in the map
        // forever — nothing else ever clears it in that case.
        state.pendingSends.delete(data.event.local_id);
      }
      // Same manual-only read-tracking as the room list and this panel's
      // own "[ mark read ]" button (`markCurrentThreadRead`) — a live
      // reply counts as unread even if the thread happens to be open on
      // screen right now; only actually pressing "mark read" clears it,
      // same as a room's own unread badge keeps climbing while it's the
      // selected room until "[ mark read ]" is pressed.
      state.unreadThreads.add(key);
      if (data.event.mentions_me) state.mentionThreads.add(key);
      updateThreadsButtonBadge();
      const bump = (list) => {
        const t = list?.find((e) => e.event_id === data.thread_root_id);
        if (t) {
          t.thread_count = (t.thread_count || 0) + 1;
          t.is_unread = true;
        }
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
        state.rightPanel.roomId === data.room_id &&
        state.rightPanel.root.event_id === data.thread_root_id
      ) {
        state.rightPanel.events = data.events;
        state.threadPaginationReachedStart.delete(`${data.room_id}|${data.thread_root_id}`);
        // Appends onto the shell `openThread()` already rendered rather
        // than a second full `renderSidePanel()` teardown — see
        // `appendInitialThreadReplies`'s own comment for why that used to
        // flicker the whole panel.
        if (!appendInitialThreadReplies(data.thread_root_id, data.events)) renderSidePanel();
        // Default to loading the whole thread rather than just the latest
        // page — keep requesting older pages until the server says there's
        // nothing left. `LoadMoreThreadReplies` is a no-op (immediate
        // `reached_start: true`) once the first page already covered the
        // whole thread, so this is safe to always fire.
        send("LoadMoreThreadReplies", { room_id: data.room_id, thread_root_id: data.thread_root_id });
        // Deliberately doesn't auto-mark the thread read just because its
        // events finished loading — see `markCurrentThreadRead`'s doc
        // comment. The panel's own "[ mark read ]" button is now the only
        // way this thread's unread state clears.
      }
      if (
        state.pendingThreadScrollTarget?.roomId === data.room_id &&
        state.pendingThreadScrollTarget?.threadRootId === data.thread_root_id
      ) {
        const eventId = state.pendingThreadScrollTarget.eventId;
        setTimeout(() => tryScrollToThreadMessage(data.room_id, data.thread_root_id, eventId), 50);
      }
      break;
    case "ThreadEventsPrepend":
      if (
        state.rightPanel &&
        state.rightPanel.kind === "thread" &&
        state.rightPanel.roomId === data.room_id &&
        state.rightPanel.root.event_id === data.thread_root_id
      ) {
        const oldEvents = state.rightPanel.events;
        state.rightPanel.events = [...data.events, ...oldEvents];
        if (data.reached_start) {
          state.threadPaginationReachedStart.add(`${data.room_id}|${data.thread_root_id}`);
        } else {
          send("LoadMoreThreadReplies", { room_id: data.room_id, thread_root_id: data.thread_root_id });
        }
        // Inserts above what's already rendered instead of the full
        // `renderSidePanel()` rebuild that used to run once per
        // auto-paged-in page — see `prependThreadReplies`'s own comment.
        if (!prependThreadReplies(data.thread_root_id, data.events, oldEvents)) renderSidePanel();
      }
      if (
        state.pendingThreadScrollTarget?.roomId === data.room_id &&
        state.pendingThreadScrollTarget?.threadRootId === data.thread_root_id
      ) {
        const eventId = state.pendingThreadScrollTarget.eventId;
        setTimeout(() => tryScrollToThreadMessage(data.room_id, data.thread_root_id, eventId), 50);
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
      // Picks up any thread badges the main timeline couldn't show yet —
      // see `renderMessage`'s `threadCount` fallback comment for why the
      // per-message bundled data alone isn't always enough. Patches just
      // the rows that are actually thread roots (most calls are no-ops —
      // `rerenderMessageInPlace` itself skips anything not currently
      // rendered) instead of a full `renderTimeline()` teardown, which
      // used to fire *again* right on top of `LoadTimeline`'s own render
      // (and again once more from the `Members` handler below) every
      // single time a room was opened — three full timeline rebuilds
      // back to back, each with its own scroll-to-bottom jump and image
      // reload, is exactly what read as "the whole screen flickering" on
      // room open.
      if (data.room_id === state.selectedRoom) {
        for (const t of data.threads) rerenderMessageInPlace(data.room_id, t.event_id);
      }
      updateThreadsButtonBadge();
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
      // Same in-place patch as `ThreadsList` above, same reasoning.
      if (data.room_id === state.selectedRoom) {
        for (const t of fresh) rerenderMessageInPlace(data.room_id, t.event_id);
      }
      updateThreadsButtonBadge();
      break;
    }
    case "Members": {
      const hadMembersBefore = !!state.roomMembers[data.room_id];
      state.roomMembers[data.room_id] = data.members;
      // Presence isn't part of `Event::Members` itself (it's a separate,
      // account-wide sync stream — see `Command::GetPresence`'s doc
      // comment) — asked for right after so a room's messages get their
      // presence dots filled in shortly after opening, without querying
      // presence for every user this session has ever seen a message from.
      send("GetPresence", { user_ids: data.members.map((m) => m[0]) });
      // `applyMentionPills` (called from `renderMessage`) needs this same
      // member list to turn a plain "@DisplayName" occurrence into a
      // highlighted pill — but `Command::LoadTimeline`'s response and this
      // one race, with no guaranteed order (see `selectRoom`, which fires
      // both). When `Members` loses that race, whatever was already
      // rendered went out with mentions un-pill-ified and *stayed* that
      // way forever — nothing else ever asked `renderMessage` to look
      // again. Re-rendering once, here, the first time this room's member
      // list actually arrives (not on every later `ListMembers` — those
      // are cache hits that skip sending the command at all, see
      // `selectRoom`'s `if (!state.roomMembers[roomId])` guard) catches it
      // up — always right around room-open time, before the user's had a
      // chance to scroll. Patches every already-rendered row in place
      // (`rerenderMessageInPlace`/`rerenderThreadMessageInPlace`) rather
      // than a full `renderTimeline()`/`renderSidePanel()` teardown —
      // those used to run right on top of `LoadTimeline`'s (and
      // `openThread`'s) own render for every single room/thread open,
      // which is what made opening one look like the whole screen
      // flickering: a full rebuild, scroll-to-bottom jump, and every
      // image reloading from cache, twice over for nothing but adding
      // mention pills.
      if (!hadMembersBefore && data.room_id === state.selectedRoom) {
        for (const ev of state.timelines[data.room_id] || []) {
          rerenderMessageInPlace(data.room_id, ev.event_id);
        }
        if (state.rightPanel?.kind === "thread" && state.rightPanel.roomId === data.room_id) {
          rerenderThreadMessageInPlace(state.rightPanel.root.event_id);
          for (const ev of state.rightPanel.events) rerenderThreadMessageInPlace(ev.event_id);
        }
      }
      break;
    }
    case "Summary": {
      const req = state.summaryRequest;
      if (req && req.roomId === data.room_id && req.threadRootId === data.thread_root_id) {
        const body = document.getElementById("summary-dialog-body");
        // `renderMarkdown`, not `textContent` — the LLM's response is
        // markdown-formatted (headers, **bold**, `- ` bullet lines,
        // blank-line-separated paragraphs), and `textContent` collapses
        // all of that structure into one unbroken wall of text since
        // plain-text newlines don't survive default CSS `white-space`
        // handling. Safe the same way every other `renderMarkdown` call
        // in this app is — it escapes the raw text before adding any
        // markup, so this can't execute anything even though the LLM's
        // output is ultimately derived from other people's messages.
        if (body) body.innerHTML = renderMarkdown(data.text);
      }
      break;
    }
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
    case "MessageSendFailed": {
      if (type === "MessageSendFailed") console.error("send failed:", data.error);
      // The live `/sync` echo (`NewMessage`/`ThreadReply`, both reconciled
      // via `reconcileLocalEcho`) usually reaches `app.js` before this
      // HTTP-response-driven event does and already deletes the
      // `pendingSends` entry — nothing left to update here in that case,
      // the placeholder's already gone.
      const pending = state.pendingSends.get(data.local_id);
      if (!pending) break;
      const placeholderId = `local:${data.local_id}`;
      let events, containerEl, ctx;
      if (pending.threadId) {
        if (state.rightPanel?.kind !== "thread" || state.rightPanel.root.event_id !== pending.threadId) break;
        events = state.rightPanel.events;
        containerEl = document.getElementById("thread-messages");
        ctx = { roomId: pending.roomId, threadId: pending.threadId };
      } else {
        events = state.timelines[pending.roomId];
        if (!events) break;
        containerEl = pending.roomId === state.selectedRoom ? el.timeline : null;
        ctx = { roomId: pending.roomId, threadId: null };
      }
      const idx = events.findIndex((e) => e.event_id === placeholderId);
      if (idx === -1) break;
      if (type === "MessageSendFailed") {
        state.pendingSends.delete(data.local_id);
        events[idx]._sendStatus = "failed";
        events[idx]._sendError = data.error;
      } else {
        // Leave the `pendingSends` entry in place — the real event still
        // hasn't arrived yet (that's what actually replaces this
        // placeholder), this just clears the "sending…" label a little
        // earlier than waiting for it would.
        events[idx]._sendStatus = "sent";
      }
      const itemEl = containerEl?.querySelector(`.msg-item[data-event-id="${CSS.escape(placeholderId)}"]`);
      if (itemEl) itemEl.replaceWith(renderMessageItem(events[idx], ctx));
      break;
    }
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
        state.imageCache[data.key] = bytesToImageUrl(data.bytes, declaredMime);
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
      patchAvatarsWithImage(data.key);
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
      // A pinned image's thumbnail finishing its download — same
      // "reopen in place" refresh `PinnedEvents`/`EventPreview` use for
      // this dialog.
      if (state.pinnedEventsDialogRoomId && document.querySelector(".dialog-box h3")?.textContent === "pinned messages") {
        closeDialog();
        openPinsDialog(state.pinnedEventsDialogRoomId);
      }
      // A thread-list row's own thumbnail (root message or bundled
      // latest-reply) finishing its download — same "just re-render the
      // panel" refresh as everything else above, cheap enough since this
      // panel is never more than a couple hundred rows.
      if (state.rightPanel?.kind === "threads-list") renderThreadsListRows();
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
    case "RoomFavoriteSet": {
      const room = state.rooms.find((r) => r.room_id === data.room_id);
      if (room) {
        room.is_favorite = data.favorite;
        sortRoomsList();
        renderRooms();
      }
      if (data.room_id === state.selectedRoom) updateFavoriteButtonLabel(room);
      break;
    }
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
    case "TypingUsers":
      state.typingUsers[data.room_id] = data.user_ids;
      if (data.room_id === state.selectedRoom) renderTypingIndicator();
      break;
    case "RoomInfo": {
      const info = data;
      state.roomInfo[info.room_id] = info;
      if (info.room_id === state.selectedRoom && info.name) {
        el.timelineTitle.textContent = info.name;
      }
      // Re-render an already-open room settings dialog in place, if any —
      // otherwise a `SetRoomName`/`SetRoomTopic` success would leave the
      // dialog's fields showing what the user just typed rather than what
      // the server actually confirmed (harmless when they match, but a
      // rejected write would otherwise look like it silently succeeded).
      if (document.getElementById("dlg-room-settings-name")) {
        closeDialog();
        openRoomSettingsDialog(info.room_id);
      }
      break;
    }
    case "PinnedEvents":
      state.pinnedEvents[data.room_id] = data.event_ids;
      if (data.room_id === state.selectedRoom) renderPinnedBanner();
      if (document.querySelector(".dialog-box h3")?.textContent === "pinned messages") {
        closeDialog();
        openPinsDialog(data.room_id);
      }
      break;
    case "EventPreview": {
      const key = `${data.room_id}|${data.event_id}`;
      state.eventPreviews[key] = data.event || null;
      // Re-render an open pins dialog in place so a preview that was
      // still "loading..." fills in without needing to close/reopen it.
      if (document.querySelector(".dialog-box h3")?.textContent === "pinned messages") {
        closeDialog();
        openPinsDialog(data.room_id);
      }
      // Same idea for any shared-link preview card(s) still showing
      // "loading shared message..." for this exact event — patch them in
      // place rather than needing a full re-render to pick it up.
      document
        .querySelectorAll(
          `a.shared-link-preview[data-shared-room-id="${CSS.escape(data.room_id)}"][data-shared-event-id="${CSS.escape(data.event_id)}"]`
        )
        .forEach((a) => renderSharedLinkPreviewInto(a, data.room_id, data.event_id, state.selectedRoom));
      break;
    }
    case "MessageSearchResult":
      renderMessageSearchResults(data.results, data.truncated);
      break;
    case "PollUpdated": {
      const poll = data;
      state.polls[poll.poll_event_id] = poll;
      if (state.pollsDialogRoomId === poll.room_id) {
        const body = document.getElementById("polls-dialog-body");
        if (body) {
          openPollsDialog(poll.room_id, false);
        }
      }
      break;
    }
    case "PollsList":
      for (const poll of data.polls) state.polls[poll.poll_event_id] = poll;
      if (state.pollsDialogRoomId === data.room_id) {
        openPollsDialog(data.room_id, false);
      }
      break;
    case "PresenceUpdated":
      state.presence[data.user_id] = data;
      updateMemberListPresenceDots();
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
    case "LvxApiKeyStatus":
      state.lvxApiKeyConfigured = data.configured;
      if (state.rightPanel?.kind === "security") renderSidePanel();
      break;
    case "OwnProfile":
      state.ownProfile = data;
      state.ownProfileSaving = false;
      if (state.rightPanel?.kind === "profile") renderSidePanel();
      break;
    case "RoomKeysImported":
      state.keyImportStatus = `imported ${data.imported} of ${data.total} session${data.total === 1 ? "" : "s"}`;
      showToast(state.keyImportStatus);
      if (state.rightPanel?.kind === "security") renderSidePanel();
      break;
    case "AllUsers":
      state.allUsers = data.users;
      state.allUsersLoading = false;
      renderUserSearchDialogList();
      renderInviteDialogList();
      break;
    case "DirectoryUsers":
      state.directorySearch = { query: data.query, users: data.users };
      renderInviteDialogList();
      break;
    case "UserMessagesSearchResult":
      if (state.rightPanel?.kind === "user-search" && state.rightPanel.userId === data.user_id) {
        state.rightPanel.loading = false;
        state.rightPanel.results = data.results;
        state.rightPanel.truncated = data.truncated;
        renderSidePanel();
      }
      break;
    case "SharedEventResolved": {
      if (
        state.pendingSharedEventResolve?.roomId !== data.room_id ||
        state.pendingSharedEventResolve?.eventId !== data.event_id
      ) {
        // Stale — the user followed a different link (or navigated away)
        // before this round-trip came back.
        break;
      }
      state.pendingSharedEventResolve = null;
      if (!data.found) {
        showToast("couldn't find that message — it may be deleted, or you may not have access to it");
        break;
      }
      const alreadyLoaded = state.timelineLoaded.has(data.room_id);
      if (data.thread_root_id) {
        // Same "needs the room's main timeline loaded first, for the
        // thread root's data" handoff `openMatrixToLink` uses for an
        // explicit `?thread=` link.
        if (alreadyLoaded) {
          resolveThreadLink(data.room_id, data.thread_root_id, data.event_id);
        } else {
          state.pendingThreadLink = {
            roomId: data.room_id,
            threadRootId: data.thread_root_id,
            eventId: data.event_id,
          };
        }
      } else if (alreadyLoaded) {
        findAndScrollToMessage(data.room_id, data.event_id);
      } else {
        state.pendingScrollTarget = { roomId: data.room_id, eventId: data.event_id };
      }
      break;
    }
    case "Error":
      console.error("backend error:", data);
      showToast(data);
      // A failed `Command::Summarize` (bad/missing key, request error,
      // ...) surfaces here as a generic `Event::Error`, not something
      // `case "Summary"` ever sees — without this, an open summary
      // dialog just sat on "summarizing..." forever with no visible
      // explanation beyond a toast that's already faded by the time
      // anyone thinks to look back at the dialog.
      if (state.summaryRequest) {
        const body = document.getElementById("summary-dialog-body");
        if (body) body.textContent = data;
      }
      if (state.ownProfileSaving) {
        state.ownProfileSaving = false;
        if (state.rightPanel?.kind === "profile") renderSidePanel();
      }
      break;
    default:
      break;
  }
}

// A plain `setInterval(pollEvents, 150)` forever, on every platform, was
// tried as a genuine push-based replacement once already (via Tauri's own
// `event.listen`/`emit`) — reverted after a real-device test showed it
// hanging completely at boot: a real race, not a config problem (see
// `poll_events`'s doc comment in lib.rs for the full story — the short
// version is that the worker's own unprompted first `SessionChecked`/
// `LoggedIn` pair routinely fires *before* this script has finished
// loading far enough to register a listener for it, and `emit()` doesn't
// replay anything for a listener that subscribes late). Polling stays,
// but the interval is no longer a flat 150ms everywhere: on Android,
// where `ForegroundSyncService` deliberately keeps this whole process
// (webview included) running around the clock so notifications keep
// arriving while backgrounded, waking something up ~7 times a second, all
// day, every day, regardless of whether the screen's even on was a real,
// continuous, reported battery cost ("app cực kỳ tốn pin") — the process
// never got to reach a deep idle state. `document.visibilityState`
// slows this down while the page isn't visible (screen off, or another
// app in front) and speeds back up the moment it is; a slower background
// tick still delivers a new-message notification within ~1s of it
// arriving, unnoticeable for a chat app, for a meaningful cut in how
// often anything wakes up at all while nobody's looking at the screen.
const POLL_INTERVAL_VISIBLE_MS = 150;
const POLL_INTERVAL_HIDDEN_MS = 1000;
let pollIntervalId = null;
function setPollInterval(ms) {
  if (pollIntervalId !== null) clearInterval(pollIntervalId);
  pollIntervalId = setInterval(pollEvents, ms);
}
async function pollEvents() {
  try {
    const events = await invoke("poll_events");
    for (const evt of events) handleBackendEvent(evt);
  } catch (err) {
    console.error("poll_events failed:", err);
  }
}
setPollInterval(document.hidden ? POLL_INTERVAL_HIDDEN_MS : POLL_INTERVAL_VISIBLE_MS);
document.addEventListener("visibilitychange", () => {
  setPollInterval(document.hidden ? POLL_INTERVAL_HIDDEN_MS : POLL_INTERVAL_VISIBLE_MS);
  // A message that arrived while backgrounded (polled at the slower
  // interval) may have been sitting queued for up to
  // `POLL_INTERVAL_HIDDEN_MS` — catch up immediately on the way back to
  // visible instead of waiting out whatever's left of that tick.
  if (!document.hidden) pollEvents();
});

// =========================================================================
// Resizable panels
// =========================================================================

function setupResizer(handle, panel, { fromRight } = {}) {
  handle.addEventListener("mousedown", (e) => {
    e.preventDefault();
    const startX = e.clientX;
    const startWidth = panel.getBoundingClientRect().width;
    // Read once, outside the move handler — `getComputedStyle` forces a
    // synchronous layout recalculation, and `min-width` doesn't change
    // mid-drag, so re-reading it on every single mousemove event (every
    // few pixels of movement) was pure layout-thrashing for no benefit.
    const min = parseInt(getComputedStyle(panel).minWidth, 10) || 120;
    handle.classList.add("dragging");
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";

    // Throttled to one width write per animation frame — same reasoning
    // as the room list / timeline scroll handlers elsewhere in this file:
    // a mouse can fire `mousemove` far faster than the page can usefully
    // repaint, so writing `style.width` on every one of them just piles
    // up redundant layout work the browser hasn't even finished from the
    // last write yet.
    let pendingEv = null;
    let rafPending = false;
    function applyPendingMove() {
      rafPending = false;
      if (!pendingEv) return;
      const delta = fromRight ? startX - pendingEv.clientX : pendingEv.clientX - startX;
      panel.style.width = Math.max(min, startWidth + delta) + "px";
    }
    function onMove(ev) {
      pendingEv = ev;
      if (rafPending) return;
      rafPending = true;
      requestAnimationFrame(applyPendingMove);
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

// The event ID `scrollToMessage` last marked with `.jump-highlight` (see
// below) — tracked by ID rather than the DOM node itself, because a
// thread panel (and the main timeline, on a full `renderTimeline()`)
// rebuilds its message rows from scratch on plenty of things that aren't
// "the user jumped somewhere else" — e.g. `ThreadEventsPrepend` bringing
// in another page while auto-loading a thread's full history (see
// `resolveThreadLink`'s doc comment). A DOM-node reference would just go
// stale the moment that happened, silently losing the highlight; tracking
// the ID instead lets `renderMessage` re-apply it on every render no
// matter how many times the row underneath gets torn down and rebuilt.
let lastJumpHighlightedEventId = null;
// Clears `lastJumpHighlightedEventId` (and the class along with it) 30s
// after the most recent jump — `null` whenever nothing's pending. Kept as
// a real timer handle (not just a timestamp) so a second jump within that
// window can cancel and restart it, rather than the earlier jump's timeout
// firing partway through and clearing the *new* target early.
let jumpHighlightTimer = null;

/** Jumps to and highlights the message a reply-preview or shared
 * matrix.to link points at. `containerId` is `"timeline"` for the main
 * view or `"thread-messages"` for the thread panel — whichever one this
 * reply-preview was rendered in, since the target can only possibly be
 * loaded there. Silently no-ops (well, a toast) if it isn't currently
 * loaded — e.g. it's further back than "load more messages" has fetched
 * yet. The highlight clears itself after 30s (or immediately, moved
 * rather than left behind, if another jump happens first). */
function scrollToMessage(containerId, eventId) {
  const container = document.getElementById(containerId);
  // `.msg-item`, never the group's own `.msg-row` — the row's own
  // `data-event-id` is just its *first* message's id (see
  // `renderMessageGroup`), which for any message past the first in its
  // group would otherwise resolve to the wrong element (and the wrong
  // one entirely for a plain `[data-event-id]` query, which matches the
  // ancestor row before its descendant item in document order). Every
  // message, first-in-group or not, has its own `.msg-item`.
  const target = container?.querySelector(`.msg-item[data-event-id="${CSS.escape(eventId)}"]`);
  if (!target) {
    showToast("original message isn't loaded — try loading more history");
    return;
  }
  // Instant, not smooth — a smooth scroll animates over however far the
  // target is (can be many screens away, e.g. a shared link into a long
  // room), which would otherwise leave the highlight below appearing on a
  // message that's still sliding into place rather than already settled
  // on screen.
  target.scrollIntoView({ block: "center" });
  if (lastJumpHighlightedEventId && lastJumpHighlightedEventId !== eventId) {
    // Best-effort clear of the *previous* target, wherever its row
    // currently lives (main timeline or a thread panel) — `renderMessage`
    // simply won't re-apply the class once `lastJumpHighlightedEventId`
    // has moved on, so this only matters for a row that's still mounted
    // from before and would otherwise keep showing a stale highlight.
    document
      .querySelectorAll(`.msg-item[data-event-id="${CSS.escape(lastJumpHighlightedEventId)}"].jump-highlight`)
      .forEach((row) => row.classList.remove("jump-highlight"));
  }
  target.classList.add("jump-highlight");
  lastJumpHighlightedEventId = eventId;

  if (jumpHighlightTimer) clearTimeout(jumpHighlightTimer);
  jumpHighlightTimer = setTimeout(() => {
    document
      .querySelectorAll(`.msg-item[data-event-id="${CSS.escape(eventId)}"].jump-highlight`)
      .forEach((row) => row.classList.remove("jump-highlight"));
    lastJumpHighlightedEventId = null;
    jumpHighlightTimer = null;
  }, 30000);
}

// =========================================================================
// Share message (matrix.to links)
// =========================================================================

function buildMatrixToLink(roomId, eventId, threadRootId) {
  // Room/event IDs (`!xyz:server`, `$xyz`) don't contain characters that
  // need percent-encoding, and matrix.to links are conventionally shown
  // "clean" (`:` literal, not `%3A`) — encoding them would still work when
  // clicked here (the handler below decodes), but would look broken
  // pasted anywhere that doesn't bother decoding first.
  const link = `https://matrix.to/#/${roomId}/${eventId}`;
  // Standard matrix.to links have no way to say "this event is a reply
  // inside thread X" — a thread reply is completely absent from a room's
  // main timeline (only the thread's root message appears there, as a
  // reply-count summary), so a plain client following this link has
  // nowhere to even look for it. `?thread=` is our own extension: ignored
  // by every other client (it's just an unrecognized query param), but
  // lets `openMatrixToLink` below go straight to the right thread panel
  // instead of paginating the entire room history hunting for an event
  // that was never going to be there.
  return threadRootId && threadRootId !== eventId
    ? `${link}?thread=${threadRootId}`
    : link;
}

async function shareMessage(roomId, eventId, threadRootId) {
  const link = buildMatrixToLink(roomId, eventId, threadRootId);
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
 * the link points somewhere other than the one currently open. Our own
 * `?thread=` extension (see `buildMatrixToLink`) is picked out of the
 * query string here too, so a shared thread reply routes to
 * `openMatrixToLink`'s thread-aware path instead of the plain-timeline
 * one, which would never find it. */
document.addEventListener("click", (e) => {
  const a = e.target.closest("a[href]");
  if (!a) return;
  const m = a.getAttribute("href")?.match(/^https:\/\/matrix\.to\/#\/(![^/?]+)\/(\$[^/?]+)(\?[^#]*)?/);
  if (!m) return;
  e.preventDefault();
  const threadRootId = m[3] ? new URLSearchParams(m[3]).get("thread") : null;
  openMatrixToLink(decodeURIComponent(m[1]), decodeURIComponent(m[2]), threadRootId ? decodeURIComponent(threadRootId) : null);
});

/** Every other `http(s)://` link `marked`'s autolinker produces inside a
 * rendered message body — routed to `Command::OpenUrl` (the system
 * browser) instead of left to the webview's own default navigation.
 * Without this, clicking one just navigated *this* window's webview to
 * the target page, replacing the whole app in place — clicking "back"
 * isn't wired up anywhere, so from the user's side that looked less like
 * "opened a link" and more like the app breaking. Guarded so it only
 * fires for a plain external link, never the matrix.to handler above
 * (that one's already returned by then, via its own `e.preventDefault()`)
 * or an in-app anchor with no real navigation to hijack (mentions,
 * "javascript:" markdown-toolbar hooks, ...). */
document.addEventListener("click", (e) => {
  if (e.defaultPrevented) return;
  const a = e.target.closest("a[href]");
  if (!a) return;
  const href = a.getAttribute("href") || "";
  if (!/^https?:\/\//i.test(href)) return;
  e.preventDefault();
  send("OpenUrl", { url: href });
});

/** Scrolls to `eventId` in `roomId`'s main timeline like `scrollToMessage`
 * does, but — unlike that one — doesn't just give up with a toast when the
 * message isn't in whatever page happens to be loaded yet. A shared link
 * routinely points at a message from well before the room's most recent
 * page (that's the whole point of sharing one), and landing in the right
 * room with no way to tell which message it was is worse than just
 * waiting a moment: this keeps calling `PaginateBack` (via
 * `state.pendingScrollSearch`, resumed from the `TimelinePrepend` handler
 * in `handleBackendEvent`) until the target turns up or the room's actual start
 * is reached, so the caller only ever needs to fire this once. */
// Generous upper bound on how many `PaginateBack` pages a "jump to shared
// message" search will chase before giving up — without this, a search
// that never finds its target (and never gets a `reached_start: true`
// either — this homeserver has known sync quirks, see `GrowRoomList`'s
// doc comment on the Rust side) would otherwise show its one "loading..."
// toast and then hang silently forever, with zero further feedback and no
// way to tell "still working" from "stuck".
const SCROLL_SEARCH_MAX_ATTEMPTS = 40;
// How long one `PaginateBack` round-trip gets before this assumes the
// response was lost and forces a retry — see the watchdog comment below.
const SCROLL_SEARCH_WATCHDOG_MS = 8000;

function findAndScrollToMessage(roomId, eventId) {
  if (state.selectedRoom !== roomId) {
    // User navigated elsewhere while this was mid-search — drop it rather
    // than keep silently paginating a room they're not even looking at.
    state.pendingScrollSearch = null;
    return;
  }
  if (el.timeline.querySelector(`[data-event-id="${CSS.escape(eventId)}"]`)) {
    state.pendingScrollSearch = null;
    scrollToMessage("timeline", eventId);
    return;
  }
  if (state.reachedStart.has(roomId)) {
    state.pendingScrollSearch = null;
    showToast("couldn't find that message — it may be deleted, or from before you joined this room");
    return;
  }
  const attempts =
    (state.pendingScrollSearch?.roomId === roomId && state.pendingScrollSearch?.eventId === eventId
      ? state.pendingScrollSearch.attempts
      : 0) + 1;
  if (attempts > SCROLL_SEARCH_MAX_ATTEMPTS) {
    state.pendingScrollSearch = null;
    showToast("couldn't find that message after searching a long way back — giving up");
    return;
  }
  if (attempts === 1) {
    showToast("loading older messages to find the shared one...");
  } else if (attempts % 10 === 0) {
    // Periodic proof-of-life — the first toast alone fades in 6s (see
    // `showToast`) long before a deep search finishes, which otherwise
    // looks identical to "nothing is happening" from here on.
    showToast(`still searching for that message... (${attempts} pages so far)`);
  }
  const searchToken = { roomId, eventId, attempts };
  state.pendingScrollSearch = searchToken;
  // `paginateBack` no-ops if a page request for this room is already
  // marked in flight — normally the *next* `TimelinePrepend` clears that
  // flag and re-triggers this function on its own (see that event's
  // handler). If that response is ever lost instead of arriving, nothing
  // else would ever clear the flag, and this search would sit forever on
  // whatever attempt it was on. This watchdog force-clears it and retries
  // rather than trusting the flag indefinitely — `searchToken` identity
  // check means it's a no-op once the search has already moved on
  // (found, gave up, or the room changed) by the time it fires.
  setTimeout(() => {
    if (state.pendingScrollSearch !== searchToken) return;
    state.paginationInFlight.delete(roomId);
    findAndScrollToMessage(roomId, eventId);
  }, SCROLL_SEARCH_WATCHDOG_MS);
  paginateBack(roomId);
}

/** Resolves a `?thread=` link's target: finds the thread root's event
 * data (auto-paginating `roomId`'s main timeline for it, same as
 * `findAndScrollToMessage` — a thread root is just a normal timeline
 * event), opens that thread panel once found, then hands off to
 * `tryScrollToThreadMessage` for the actual reply. Resumed from
 * `TimelinePrepend` via `state.pendingThreadLink` on every retry, so the
 * caller only needs to trigger this once. */
function resolveThreadLink(roomId, threadRootId, eventId) {
  if (state.selectedRoom !== roomId) {
    state.pendingThreadLink = null;
    return;
  }
  const root =
    (state.threadsByRoom[roomId] || []).find((e) => e.event_id === threadRootId) ||
    (state.timelines[roomId] || []).find((e) => e.event_id === threadRootId);
  if (root) {
    state.pendingThreadLink = null;
    const alreadyOpen =
      state.rightPanel?.kind === "thread" &&
      state.rightPanel.roomId === roomId &&
      state.rightPanel.root.event_id === threadRootId;
    if (!alreadyOpen) openThread(roomId, root);
    state.pendingThreadScrollTarget = { roomId, threadRootId, eventId };
    // In case it's already fully loaded (e.g. the thread was open before,
    // or the target is the root itself) — `ThreadEvents` below covers the
    // rest for a fresh load.
    setTimeout(() => tryScrollToThreadMessage(roomId, threadRootId, eventId), 50);
    return;
  }
  if (state.reachedStart.has(roomId)) {
    state.pendingThreadLink = null;
    showToast("couldn't find that thread — the original message may be deleted, or from before you joined this room");
    return;
  }
  const attempts =
    (state.pendingThreadLink?.roomId === roomId &&
    state.pendingThreadLink?.threadRootId === threadRootId &&
    state.pendingThreadLink?.eventId === eventId
      ? // `openMatrixToLink`/`SharedEventResolved` set this initially
        // without an `attempts` field — only this function's own retries
        // add one, so the very first call here needs the `|| 0` fallback
        // too (bare `undefined + 1` is `NaN`, which would never trip the
        // `> SCROLL_SEARCH_MAX_ATTEMPTS` cap below).
        state.pendingThreadLink.attempts || 0
      : 0) + 1;
  if (attempts > SCROLL_SEARCH_MAX_ATTEMPTS) {
    state.pendingThreadLink = null;
    showToast("couldn't find that thread after searching a long way back — giving up");
    return;
  }
  // Same watchdog `findAndScrollToMessage` uses, and for the same reason
  // — see its comment. A thread root is just a normal timeline event
  // found via the exact same `PaginateBack` mechanism, so it's exposed to
  // the exact same "response silently lost" risk.
  const searchToken = { roomId, threadRootId, eventId, attempts };
  state.pendingThreadLink = searchToken;
  setTimeout(() => {
    if (state.pendingThreadLink !== searchToken) return;
    state.paginationInFlight.delete(roomId);
    resolveThreadLink(roomId, threadRootId, eventId);
  }, SCROLL_SEARCH_WATCHDOG_MS);
  paginateBack(roomId);
}

/** Once the right thread panel is open, checks whether its target reply
 * has shown up yet — `Event::ThreadEvents`'s handler already auto-loads a
 * thread all the way back to its start on its own (see that case's
 * comment), so this doesn't need to drive pagination itself, just keep
 * checking after each page that handler brings in until the target turns
 * up or there's nothing left to load. */
function tryScrollToThreadMessage(roomId, threadRootId, eventId) {
  const stillOpen =
    state.rightPanel?.kind === "thread" &&
    state.rightPanel.roomId === roomId &&
    state.rightPanel.root.event_id === threadRootId;
  if (!stillOpen) {
    state.pendingThreadScrollTarget = null;
    return;
  }
  const msgsEl = document.getElementById("thread-messages");
  if (msgsEl?.querySelector(`[data-event-id="${CSS.escape(eventId)}"]`)) {
    state.pendingThreadScrollTarget = null;
    scrollToMessage("thread-messages", eventId);
    return;
  }
  if (state.threadPaginationReachedStart.has(`${roomId}|${threadRootId}`)) {
    state.pendingThreadScrollTarget = null;
    showToast("couldn't find that message in the thread — it may have been deleted");
  }
  // Still loading — left as-is, `ThreadEventsPrepend` re-triggers this.
}

function openMatrixToLink(roomId, eventId, threadRootId) {
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
  if (threadRootId) {
    // Needs the room's timeline loaded first regardless — that's where
    // the thread's root message data comes from (see `resolveThreadLink`).
    if (alreadyLoaded) {
      setTimeout(() => resolveThreadLink(roomId, threadRootId, eventId), 50);
    } else {
      state.pendingThreadLink = { roomId, threadRootId, eventId };
    }
    return;
  }
  // A plain link (no `?thread=` hint — either a link this app made for a
  // non-thread message, or anyone else's ordinary matrix.to link) might
  // still point at a thread reply: those are filtered out of the main
  // timeline entirely (see `Command::ResolveSharedEvent`'s doc comment on
  // the Rust side), so `findAndScrollToMessage` would paginate the *entire*
  // room history and still never find one. Ask the server which case this
  // is before deciding where to look — resolved in the `SharedEventResolved`
  // handler below.
  state.pendingSharedEventResolve = { roomId, eventId };
  send("ResolveSharedEvent", { room_id: roomId, event_id: eventId });
}

// Shows whatever `reloadToLogin` stashed right before this exact reload
// (a session-expired or logout explanation) on the login form it reloaded
// onto, then clears it — a fresh reload with nothing stashed (a normal
// app launch) leaves the login form's error box untouched, same as
// before this existed.
try {
  const postReloadMessage = sessionStorage.getItem("postReloadLoginMessage");
  if (postReloadMessage) {
    sessionStorage.removeItem("postReloadLoginMessage");
    showLoginError(postReloadMessage);
  }
} catch {
  // Same private-browsing-style storage-block tolerance as `reloadToLogin`.
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

// ---- iOS notification tap (no gen/ios project exists yet — see below) ----
// Android's notification-tap routing (`Event::NotificationClicked`, handled
// above in `handleBackendEvent`) goes through a native `MainActivity.kt`
// workaround re-firing the tap as a `matrixtauriclient://notification`
// deep link, because tapping an Android notification just relaunches the
// Activity with no in-process callback of its own (see that file's own
// comment). iOS has no such gap — `tauri-plugin-notification`'s iOS side
// (`NotificationHandler.swift::didReceive`) delivers a tap straight to JS
// as this plugin event, in-process, every time (cold start included) — no
// deep-link workaround needed there at all.
//
// One real gap remains before this actually routes to the right room/
// thread on iOS, though: `platform.rs::show_notification` attaches
// `room_id`/`thread_id` as this notification's `extra` payload (same as
// Android), and the iOS plugin *does* stash that into the system
// notification's `content.userInfo["__EXTRA__"]` when showing it
// (`ios/Sources/Notification.swift::makeNotificationContent`) — but its
// `didReceive` handler's `toActiveNotification()` never reads it back out
// into the `ActiveNotification` struct this JS event actually receives
// (`ios/Sources/NotificationHandler.swift`, tauri-plugin-notification
// 2.3.3). `data.notification` below will have `title`/`body` but no
// `extra` until that's patched. Once `cargo tauri ios init` has generated
// `gen/ios` (needs actual Xcode — not possible on this machine, see the
// conversation this comment was written in), fix it there by:
//   1. In `NotificationHandler.swift`, add `let extra: [String: Any]?` to
//      the `ActiveNotification` struct and set it in `toActiveNotification`
//      from `request.content.userInfo["__EXTRA__"]`.
//   2. Below, read `data.notification.extra.room_id`/`.thread_id` instead
//      of the `TODO` placeholders.
// Left wired up now (rather than skipped entirely) so the one remaining
// change is a small, obvious Swift edit instead of also having to
// rediscover this whole plugin-event/room-routing gap from scratch later.
if (window.__TAURI__?.event?.listen) {
  window.__TAURI__.event.listen("plugin:notification://actionPerformed", (event) => {
    const data = event.payload;
    if (!data || data.actionId !== "tap") return;
    // TODO(ios): swap these two for data.notification.extra.room_id /
    // .thread_id once the Swift-side patch above is in — until then this
    // listener is wired but can't actually know which room to open.
    const roomId = data.notification?.extra?.room_id;
    const threadId = data.notification?.extra?.thread_id;
    if (!roomId) return;
    handleBackendEvent({ type: "NotificationClicked", data: { room_id: roomId, thread_id: threadId || null } });
  });
}
