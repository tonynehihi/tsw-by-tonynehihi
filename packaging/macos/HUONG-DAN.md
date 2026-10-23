# TS Word — Hướng dẫn cài đặt sạch & sử dụng

Bản build cá nhân của Tony, dựa trên WordCraft (MIT/Apache-2.0) của storytold, đã đổi:

- Tên app là **TS Word** — ngoài Spotlight / Finder / Dock là **TS Word.app**, icon **W xanh Word** theo ngôn ngữ thiết kế của bộ Tony Suite.
- **Tự kiểm tra & cài bản mới ngay trong app** (tab Help → Check for Updates), kèm changelog hiện trong app.
- **Menu Help gọn + bỏ hết link về bản gốc**: không còn nút Discord ở thanh tiêu đề, không còn link website / Discord / GitHub của upstream ở màn hình bắt đầu hay trong About.

Thư mục settings và dữ liệu vẫn giữ tên WordCraft như bản gốc — cài bản này không làm mất dữ liệu/settings đã có.

## Mới ở bản 1.1.0 — bản TS Word đầu tiên

- **Tự update**: mở app là app tự kiểm tra bản mới 1 lần (có bản mới mới hiện pop-up kèm **What's new** để đọc trước khi quyết định). Hoặc bất cứ lúc nào: tab **Help → Check for Updates**, hoặc mở **About TS Word**, mục **Updates** ở dưới cùng.
- Bấm **Download Update** → **Install Update**: app tự build bản mới **ngay trong app**, hiện tiến trình (đang ở bước nào, đã compile bao nhiêu crate, đã chạy bao lâu), bản đang dùng không bị đụng vào. Build xong bấm **Finish Install**: app thoát **vài giây** để tráo bản mới vào /Applications (bản cũ dời vào thư mục backup `~/Library/Caches/TS Word/updates/backup`) rồi tự mở lại. Tráo lỗi thì bản cũ được khôi phục và mở lại.
- Log khi cập nhật nằm ở `~/Library/Logs/TSWord-update.log`; build lỗi thì xem thêm `build.log` trong `~/Library/Caches/TS Word/updates/build-<version>/`.
- Bản Windows: kiểm tra bản mới trong About được, nhưng chưa tự cài — tải bản mới ở trang release (nút **Open Release Page** trong app).
- Tab **Help** chỉ còn 4 nút: **About**, **Check for Updates**, **System Info**, **Keyboard Shortcuts**.

## Gỡ bản cũ / cài lại sạch

**Nếu đã build bản trước:**
```bash
# Xóa bản build cũ
rm -rf target/
# Xóa app đã cài (nếu đã kéo vào Applications)
rm -rf "/Applications/TS Word.app"
rm -rf "/Applications/WordCraft.app"
```

## Cài đặt (làm 1 lần)

**Bước 1** — Cài Xcode Command Line Tools (nếu chưa có):
```
xcode-select --install
```

**Bước 2** — Cài Rust (nếu chưa có):
```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```
Xong thì **đóng Terminal, mở lại**.

## Build & chạy

**Bước 3** — Giải nén `tsw-by-tonynehihi-1.1.0.zip`.

**Bước 4** — Trong Terminal:
```bash
cd <kéo folder tsw-by-tonynehihi-1.1.0 vào Terminal rồi thả>
cargo build --release -p wordcraft
```
(Lần đầu 10–20 phút.)

**Bước 5** — Chạy thử:
```bash
./target/release/wordcraft
```

**Bước 6** — Đóng gói thành app:
```bash
packaging/macos/package.sh --arch aarch64
```
(Máy Intel cũ: `--arch x86_64`.)
Xong mở file `.dmg` trong `dist/release/`, kéo **TS Word.app** vào Applications.

Từ các bản sau, không cần lặp lại các bước này: mở app và dùng **Check for Updates**.

## App không mở được (macOS chặn app tự đóng gói)

App tự đóng gói không có chữ ký Apple nên macOS hay chặn — không phải app hỏng.

**Trường hợp 1** — macOS báo app "from an unidentified developer":
Chuột phải vào app → chọn **Open** → bấm **Open** trong hộp thoại hiện ra.

**Trường hợp 2** — macOS báo app "is damaged" hoặc "can't be opened":
1. Mở Terminal.
2. Gõ đúng dòng này rồi Enter:
```bash
xattr -cr "/Applications/TS Word.app"
```
3. Mở app lại bình thường.

**Trường hợp 3** — app crash / tắt ngay khi mở:
Kiểm tra lại **Bước 6** đã đúng chip máy chưa — máy Apple Silicon (M1/M2/M3/M4) phải dùng `--arch aarch64`, máy Intel cũ dùng `--arch x86_64`. Đóng gói sai chip là crash luôn, phải đóng gói lại.

## Build báo lỗi

- Thiếu `xcode-select`: chạy `xcode-select --install` rồi build lại.
- Terminal không nhận lệnh `cargo`: đóng Terminal, mở lại (Rust mới thêm vào PATH sau khi cài).
- Lỗi khác: chụp màn hình lỗi gửi tao.
