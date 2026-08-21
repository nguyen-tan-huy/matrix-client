# Matrix egui Client

Bản viết lại của app Matrix (trước đây là Tauri + React) sang **100% Rust**
dùng [egui](https://github.com/emilk/egui)/[eframe](https://docs.rs/eframe) —
không webview, không frontend riêng, style tối giản kiểu terminal (monospace,
không bo góc, không đổ bóng) để tập trung vào bố cục thay vì thẩm mỹ.

## Trạng thái tính năng — đã port đầy đủ từ bản Tauri

- ✅ Login user/pass + **OAuth/OIDC** (mở browser hệ thống qua crate `open`,
  bắt redirect bằng local loopback listener — `matrix/oidc_callback.rs`,
  gần như copy nguyên văn từ bản Tauri vì phần này chưa từng phụ thuộc Tauri)
- ✅ Khôi phục session (sqlite store)
- ✅ Room list, sync realtime
- ✅ Timeline chính, gửi tin nhắn, phân trang lùi
- ✅ Thread: xem 1 thread cụ thể, gửi reply vào thread
- ✅ Danh sách **toàn bộ** threads trong room — gọi thẳng
  `GET /rooms/{roomId}/threads`, tự phân trang (không chỉ quét cache)
- ✅ Tóm tắt hội thoại bằng Claude API
- ✅ **Device verification (emoji SAS)** — nút `[sec]` ở room list mở panel
  Security, "verify this device" gửi request tới thiết bị khác, popup emoji
  tự bật khi 2 bên trao đổi khoá xong (`ui/security.rs` +
  `matrix/verification.rs`)
- ✅ **Khôi phục lịch sử mã hoá** — nhập recovery key trong panel Security
- ✅ **Ảnh inline trong timeline** — dùng `egui_extras::install_image_loaders`
  + `egui::Image::new(url)`, vẽ trực tiếp trong tin nhắn kiểu `image`

## Chạy nền + tray icon

- **Đóng cửa sổ (nút X) không thoát app** — `AppShell::update` bắt
  `close_requested()`, gọi `CancelClose` rồi `Visible(false)` để **ẩn** cửa
  sổ thay vì kết thúc process. Sync loop, notification, mọi thứ vẫn chạy
  nền bình thường.
- **Tray icon** dùng `ksni` — thuần Rust, nói chuyện qua D-Bus
  `org.kde.StatusNotifierItem`, đúng chuẩn mà module tray của
  waybar/swaybar trên sway hỗ trợ. Không cần GTK/libappindicator.
  - **Click trái vào icon** → toggle ẩn/hiện cửa sổ.
  - **Menu chuột phải**: "Toggle window" (giống click trái) và "Quit" (thoát
    thật, `std::process::exit(0)`).
  - Icon dùng tên theo freedesktop icon theme (`mail-message-new`) — không
    cần file ảnh riêng, tự động khớp theme hệ thống đang dùng. Muốn icon
    riêng, đổi `icon_name()` trong `src/tray.rs` sang `icon_pixmap()` với
    ảnh bundle theo app.
  - Trạng thái hiện/ẩn dùng chung 1 `Arc<AtomicBool>` giữa GUI thread và
    tray thread, để 2 cách ẩn (nút X vs click tray) luôn đồng bộ — không bị
    lệch trạng thái khi người dùng đóng bằng nút X rồi bấm tray sau đó.

**Yêu cầu để tray hiện ra trên sway**: cần có 1 tray host trong bar, ví dụ
waybar với module `"tray"` trong config, hoặc `nm-applet`/`swaync` nếu bạn
dùng cách khác để render SNI tray.

## Video

egui **không có video widget** — không thể phát inline như ảnh. Cách xử lý:

- Tin nhắn video hiện **thumbnail** (nếu server gửi kèm, qua
  `info.thumbnail_source`) bằng đúng cơ chế tải-có-auth như ảnh.
- Nút **`[ ▶ play video ]`** → gửi `Command::PlayVideo` → worker tải toàn bộ
  bytes qua `client.media()` (có auth), **ghi thẳng ra file tạm**
  (`std::env::temp_dir()`), rồi gọi `open::that(path)` mở bằng trình phát
  video mặc định của hệ thống (mpv, vlc, Celluloid...).
- **Cố tình không cache video vào RAM** (`image_cache`) — video có thể vài
  chục/trăm MB, đưa vào cùng LRU cap theo số lượng item (không theo dung
  lượng) sẽ phá vỡ giả định "80 ảnh nhỏ" của cache đó. Mỗi lần bấm play là
  1 lần tải mới; nếu muốn cache theo dung lượng thực tế (byte, không phải
  số lượng item) cho cả ảnh lẫn video, đó là bước nâng cấp tiếp theo hợp lý.

## Room list tự cập nhật khi có tin nhắn mới

`sync_with_callback` trong `matrix/worker.rs` gọi `refresh_rooms()` sau
**mỗi** response từ `/sync` (tức là mỗi lần có gì thay đổi) — nên preview
tin nhắn cuối, unread count, và thứ tự room đều tự cập nhật không cần thao
tác gì thêm.

## Desktop notification (mako / sway)

Dùng `notify-rust`, gửi qua chuẩn D-Bus `org.freedesktop.Notifications` —
**không có gì đặc thù cho mako**, hoạt động với bất kỳ daemon nào tuân chuẩn
(mako, dunst, swaync...). Yêu cầu duy nhất: mako (hoặc daemon tương đương)
đang chạy trong session sway.

- `src/notify.rs` — hàm `notify()`, chạy trên thread riêng để không chặn
  frame egui khi chờ D-Bus round-trip.
- `App::maybe_notify()` trong `app.rs` gọi khi có `Event::NewMessage`, với
  2 điều kiện bỏ qua: tin nhắn **của chính mình**, hoặc room đó **đang mở
  sẵn** trên màn hình (tránh notify về thứ đã nhìn thấy).
- Không cần cấu hình `.desktop` file hay app-id đặc biệt để notification
  hiện đúng icon/tên trên mako — `appname("Matrix")` là đủ cho hiển thị cơ
  bản; nếu muốn icon riêng, thêm `.icon("...")` trỏ tới file cài cùng app.

## Cache eviction (LRU) — tránh RAM tăng dần không giới hạn

Được thêm sau khi đánh giá kịch bản dùng thật cả ngày với nhiều room:

- **`timelines` (tin nhắn từng room đã mở)**: cap `MAX_CACHED_ROOMS = 15`
  (đổi hằng số này trong `app.rs`). Khi mở room thứ 16, room ít dùng nhất bị
  evict khỏi RAM — cả `App.timelines` phía UI lẫn `WorkerState.room_timelines`
  phía worker (qua `Command::CloseRoomTimeline`). Room đang mở trên màn hình
  không bao giờ bị evict. Vì dữ liệu gốc vẫn nằm trong sqlite store, mở lại
  room bị evict chỉ tốn 1 lần load lại, không mất tin nhắn.
- **`image_cache` (bytes ảnh đã tải)**: cap `MAX_CACHED_IMAGES = 80`. Ảnh cũ
  nhất bị evict cả khỏi `App.image_cache` lẫn texture cache của egui (qua
  `ctx.forget_image(...)`) để giải phóng cả RAM lẫn GPU memory.
- Cả 2 dùng chung 1 pattern: `VecDeque` giữ thứ tự truy cập gần đây nhất
  (`room_lru` / `image_lru`), "touch" mỗi khi truy cập, evict từ đầu deque
  khi vượt cap.

## Chấp nhận lời mời vào room mới

- `refresh_rooms` (worker) giờ bao gồm cả room ở trạng thái `Invited`, đánh
  dấu `is_invite: true`, luôn hiện ở đầu danh sách room.
- Room list vẽ card riêng cho lời mời với 2 nút **accept** / **decline** —
  không click để mở như room thường.
- `Command::AcceptInvite` → `room.join()`, `Command::DeclineInvite` →
  `room.leave()`, cả 2 tự refresh lại room list sau khi xong.

## Ảnh trong timeline — đã sửa để có auth đúng cách

Thay vì load thẳng URL http (không có access token, sẽ fail với media
endpoint yêu cầu auth), luồng bây giờ là:

1. `matrix/convert.rs` chỉ lấy **mxc URI thô** (`mxc://server/media_id`),
   không convert sang http URL nữa.
2. UI (`ui/timeline.rs`) gọi `app.request_image(event_id, mxc_uri)` khi vẽ 1
   tin nhắn ảnh chưa có trong cache — gửi `Command::FetchImage`.
3. Worker (`matrix/worker.rs`) dùng `client.media().get_media_content(...)`
   — hàm này **tự gắn Authorization header** bằng access token hiện có, và
   tự giải mã nếu ảnh nằm trong room mã hoá (media source encrypted).
4. Bytes trả về qua `Event::ImageBytes`, UI cache vào `app.image_cache`,
   đăng ký với `ctx.include_bytes("bytes://<event_id>", bytes)` rồi vẽ bằng
   `egui::Image::new(uri)` — không phụ thuộc network loader nào tải trực
   tiếp từ URL nữa.

Mỗi ảnh chỉ fetch **1 lần** (theo dõi qua `app.image_requested`), cache theo
`event_id` cho tới khi app đóng.

**Lưu ý API**: tên hàm chính xác trên `client.media()` (`get_media_content`
vs `get_file`) và cách dựng `MediaSource::Plain` từ chuỗi mxc đã đổi vài lần
giữa các bản `matrix-sdk` — nếu build lỗi ở đoạn `Command::FetchImage` trong
`worker.rs`, đối chiếu `cargo doc -p matrix-sdk --open` → module `media`.

## Kiến trúc

```
UI thread (eframe, sync, immediate-mode)
    ↕ Command (UI -> worker) / Event (worker -> UI), qua mpsc channel
tokio worker thread (matrix-rust-sdk, async)
```

- `src/command.rs` — enum `Command`: mọi hành động UI gửi cho worker
  (login, load timeline, gửi tin, mở thread...). Tương đương các
  `#[tauri::command]` trong bản Tauri.
- `src/event.rs` — enum `Event`: worker gửi ngược kết quả/update cho UI.
  Tương đương `app.emit(...)` trong bản Tauri.
- `src/matrix/worker.rs` — vòng lặp chính xử lý `Command`, gọi
  `matrix-rust-sdk`. Đây là nơi giữ `Client` và các `Timeline` đang mở.
- `src/app.rs` — struct `App`: toàn bộ state UI (room list, timeline đang
  xem, thread đang mở...), có `poll_events()` gọi mỗi frame để nhận kết quả
  từ worker không chặn (non-blocking `try_recv`).
- `src/ui/*.rs` — các hàm vẽ từng phần: `login`, `room_list` (cột trái),
  `timeline` (cột giữa), `thread_panel` (cột phải — vừa dùng cho 1 thread cụ
  thể vừa dùng cho danh sách tất cả thread).

## Chạy dev

```bash
cargo run
```

Compile debug, chậm hơn nhưng iterate nhanh. Không cần `npm install`, không
cần Node — chỉ cần Rust toolchain. Cửa sổ mở ra thẳng màn login (hoặc tự
khôi phục session nếu đã đăng nhập trước đó, lưu tại thư mục app-data chuẩn
OS qua crate `directories`).

## Build release + cài đặt

```bash
cargo build --release
sudo cp target/release/matrix-egui-client /usr/local/bin/
```

## Single-instance (quan trọng khi chạy nền)

Vì đóng cửa sổ giờ **ẩn thay vì thoát** (xem mục "Chạy nền + tray icon" bên
dưới), `src/single_instance.rs` dùng file-lock để đảm bảo chạy app lần thứ
2 (ví dụ bấm nhầm từ launcher trong khi app đã chạy nền) không mở thêm
process/tray icon thứ 2 — chỉ in log rồi thoát ngay. **Giới hạn hiện tại**:
launcher không có cách gửi tín hiệu "show cửa sổ" cho instance đang chạy —
dùng tray icon để hiện lại cửa sổ nếu app đang ẩn.

## Autostart cùng sway

Thêm vào cuối `~/.config/sway/config`:

```
exec /usr/local/bin/matrix-egui-client
```

App tự chạy khi sway khởi động. Đóng cửa sổ (nút X) chỉ ẩn nó, app vẫn chạy
nền để tiếp tục nhận tin nhắn/notification qua tray.

## Mở từ app launcher (wofi/rofi)

```bash
cp assets/matrix-egui-client.desktop ~/.local/share/applications/
```

Sửa dòng `Exec=` trong file đó nếu binary không nằm ở `/usr/local/bin/`.
Sau đó app hiện trong wofi/rofi như app bình thường.

## Những gì đã port từ bản Tauri

Xem mục "Trạng thái tính năng" ở đầu file — mọi tính năng của bản Tauri đã
được port sang, không còn phần nào bị bỏ lại.

## Lưu ý về độ ổn định API

Như bản Tauri, `matrix-rust-sdk`/`matrix-sdk-ui` đổi API khá nhanh giữa các
bản. Nếu `cargo build` báo lỗi, đối chiếu `cargo doc -p matrix-sdk -p
matrix-sdk-ui --open` với đúng version đang resolve trong `Cargo.lock`.
