# AppSwitchClaude

Chuyển nhanh giữa nhiều tài khoản Claude trong ứng dụng **Claude Desktop** (macOS và Windows) mà không phải đăng xuất rồi đăng nhập lại. Viết bằng Rust, giao diện [egui](https://github.com/emilk/egui).

## Cách dùng

1. **Lưu tài khoản đang đăng nhập.** Mở ứng dụng khi Claude Desktop đã đăng nhập. Thẻ trên cùng báo "Tài khoản chưa lưu": đặt tên (ví dụ *Công việc*) rồi bấm **Lưu tài khoản này**.
2. **Thêm tài khoản khác.** Bấm **+ Thêm tài khoản**. Claude khởi động lại ở màn hình đăng nhập; tài khoản cũ đã được lưu và không bị đăng xuất khỏi máy chủ. Đăng nhập tài khoản mới trong Claude, quay lại ứng dụng, đặt tên và lưu.
3. **Chuyển.** Bấm **Chuyển** ở tài khoản muốn dùng. Claude tự thoát, đổi phiên và mở lại bằng tài khoản đó.

> **Đừng bấm "Log out" trong Claude** với tài khoản muốn giữ: thao tác đó huỷ phiên trên máy chủ nên bản đã lưu cũng mất hiệu lực. Muốn đăng nhập tài khoản khác, hãy dùng **+ Thêm tài khoản**.

Claude không lưu e-mail ở dạng đọc được trên máy, nên tên tài khoản do bạn tự đặt (đổi được bằng **Đổi tên**). **Xoá** chỉ bỏ bản lưu trên máy, không đụng tới tài khoản Claude.

## Build và chạy

Cần [Rust](https://rustup.rs) (bản stable mới).

**macOS**

```sh
cargo run --release              # chạy thử
./scripts/bundle-macos.sh        # tạo target/release/bundle/AppSwitchClaude.app
```

**Windows** (cần thêm "Build Tools for Visual Studio" với C++ build tools)

```powershell
cargo build --release
.\target\release\app-switch-claude.exe
```

## Cách hoạt động

Claude Desktop là ứng dụng Electron: "đang đăng nhập" nghĩa là thư mục dữ liệu của nó chứa cookie `sessionKey` của claude.ai, web storage, và các khoá `oauth:*` trong `config.json`. Ứng dụng này:

- **Lưu** một tài khoản bằng cách sao chép đúng những mục đó (`Cookies`, `Local Storage`, `Session Storage`, `IndexedDB`, `WebStorage`, các khoá `oauth:*` và `lastKnownAccountUuid`) vào kho riêng.
- **Chuyển** bằng cách: thoát Claude → lưu lại phiên đang dùng (token và cookie thay đổi theo thời gian, bản mới nhất mới chắc chắn dùng được) → chép phiên của tài khoản kia vào → mở Claude.
- Không đụng tới phần dùng chung: cache, extension, cấu hình MCP (`claude_desktop_config.json`), các thiết lập khác trong `config.json`.

Cookie và token **không bao giờ được giải mã**. Chúng vẫn được mã hoá bằng khoá do hệ điều hành giữ (Keychain trên macOS, DPAPI trên Windows), nên bản lưu chỉ dùng được trên chính máy và tài khoản người dùng này. Ứng dụng không kết nối mạng.

| | Dữ liệu Claude Desktop | Kho của ứng dụng này |
|---|---|---|
| macOS | `~/Library/Application Support/Claude` | `~/Library/Application Support/AppSwitchClaude` |
| Windows (MSIX) | `%LOCALAPPDATA%\Packages\Claude_…\LocalCache\Roaming\Claude` | `%APPDATA%\AppSwitchClaude` |
| Windows (bản cài cũ) | `%APPDATA%\Claude` | `%APPDATA%\AppSwitchClaude` |

## Giới hạn

- Phiên đăng nhập của Claude có hạn (khoảng 4 tuần). Ứng dụng hiện ngày hết hạn; khi hết, bấm **Chuyển** sang tài khoản đó rồi đăng nhập lại trong Claude là bản lưu được làm mới.
- Mỗi lúc Claude Desktop chỉ dùng một tài khoản; chuyển tài khoản là khởi động lại Claude (vài giây).
- Trên Windows không có cách yêu cầu Claude tự thoát khi nó thu về khay hệ thống, nên ứng dụng phải kết thúc tiến trình Claude. Việc đang làm dở trong Claude lúc bấm chuyển có thể mất.
- Nếu Claude Desktop đổi cách lưu phiên ở một bản cập nhật sau này, ứng dụng cần được cập nhật theo.

## Kiểm tra và chẩn đoán

```sh
cargo test                                               # test đơn vị, test luồng, test giao diện
cargo run --example probe                                # in những gì phát hiện được trên máy (chỉ đọc)
cargo test --test real_data -- --ignored --nocapture     # diễn tập trên BẢN SAO dữ liệu Claude thật
```

`probe` và `real_data` chỉ đọc dữ liệu của Claude và không in giá trị bí mật nào. Nên chạy cả hai trên một máy hoặc kiểu cài đặt mới (nhất là Windows) trước khi dùng thật.

Hai biến môi trường để thử nghiệm: `APP_SWITCH_CLAUDE_DATA_DIR` (thư mục dữ liệu Claude) và `APP_SWITCH_CLAUDE_STORE_DIR` (kho lưu tài khoản).

## Cấu trúc mã

| Tệp | Nhiệm vụ |
|---|---|
| `src/switcher.rs` | Các thao tác: lưu, chuyển, thêm, đổi tên, xoá |
| `src/session.rs` | Sao chép / khôi phục các mục chứa phiên đăng nhập |
| `src/config_json.rs` | Đọc và vá `config.json` (kể cả file journal của Claude) |
| `src/cookies.rs` | Đọc tên và hạn cookie từ CSDL SQLite của Chromium (không đọc giá trị) |
| `src/claude_app.rs` | Tìm, thoát và mở Claude Desktop |
| `src/paths.rs` | Tìm thư mục dữ liệu trên từng hệ điều hành |
| `src/store.rs` | Danh sách tài khoản đã lưu |
| `src/ui/` | Cửa sổ (egui) và luồng chạy nền |
