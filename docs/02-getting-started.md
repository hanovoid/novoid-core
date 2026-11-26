# 02 — Getting Started

## 1. Yêu cầu

- Rust toolchain (edition 2021; `rustc` + `cargo`).
- Mạng outbound tới `accounts.google.com`, `oauth2.googleapis.com`, `www.googleapis.com`, `daily-cloudcode-pa.googleapis.com`, `cloudcode-pa.googleapis.com`.
- Tài khoản Google có quyền truy cập Gemini Cloud Code (được onboard qua OAuth).
- Tùy chọn: trình duyệt để hoàn tất OAuth (dùng `--no-browser` cho server headless).

## 2. Build & cài đặt

```bash
git clone https://github.com/hanovoid/novoid-core.git
cd novoid-core
cargo build --release

# Chạy trực tiếp binary vừa build
./target/release/novoid-core start --log

# Hoặc cài vào PATH (cung cấp 2 lệnh: novoid-core và acc)
cargo install --path .
acc start --log
```

Kiểm tra build:

```bash
cargo test          # hiện tại: 0 unit test, nhưng đảm bảo compile sạch
cargo build --release
```

Binary `acc` (`src/bin/acc.rs`) là shorthand gọi chung `novoid_core::cli::run_cli()`.

## 3. CLI reference

### Cờ global (dùng trước hoặc sau subcommand)

| Cờ | Mô tả |
| :--- | :--- |
| `--debug` (alias `--dev-mode`) | Bật debug/dev mode, log chi tiết hơn |
| `--fallback` | Bật model fallback khi quota cạn |
| `--strategy <hybrid\|sticky\|round-robin>` | Ghi đè chiến lược chọn tài khoản |

Tương đương biến môi trường: `DEBUG=true`, `DEV_MODE=true`, `FALLBACK=true`, `ACCOUNT_STRATEGY` (xem chi tiết tại [Configuration](03-configuration.md)).

### Subcommand

| Lệnh | Hành vi |
| :--- | :--- |
| `novoid-core` / `acc serve` | Chạy server foreground (không daemonize) — dùng cho Docker/Railway/systemd |
| `start` | Spawn background process, lưu PID vào `~/.config/novoid-core/server.pid`; nếu đã chạy thì in thông tin PID/port |
| `start --log` | Chạy foreground có log (tương đương `serve` nhưng in banner đầy đủ) |
| `stop` | Gửi `SIGTERM` tới PID và xóa file PID |
| `restart` | `stop` + spawn lại sau 1s |
| `restart --log` | `stop` + chạy foreground |
| `status` | Hiện PID, port, API URL, dashboard URL hoặc trạng thái offline |
| `ui` | Đảm bảo server chạy, sleep 800ms, mở `http://localhost:<PORT>/` bằng `xdg-open`/`open`/`cmd /c start` |
| `accounts add [--no-browser]` | OAuth PKCE; mở browser callback local hoặc yêu cầu dán callback URL/code |
| `accounts list` | Liệt kê email + cờ `rate-limited` + đường dẫn config |
| `accounts verify` | Thử refresh từng refresh token và gọi userinfo để xác thực |
| `accounts remove` | Chọn account theo số để xóa (interactive) |
| `accounts clear` | Xóa toàn bộ account (cần confirm `y`) |
| `accounts --no-browser ...` | Form global của `--no-browser` |

> **Lưu ý:** `accounts add`, `accounts remove`, `accounts clear` yêu cầu **server đang chạy phải dừng** (`ensure_server_stopped_for_cli()` kiểm tra `127.0.0.1:<PORT>`). `accounts list` và `verify` chạy được khi server đang chạy.

## 4. Khởi chạy lần đầu

```bash
# 1) Start server
novoid-core start --log

# 2) Thêm tài khoản Google (1 trong 3 cách)
novoid-core accounts add                 # browser local
novoid-core accounts add --no-browser    # headless: dán URL callback/code
# Hoặc mở http://localhost:8080 → Accounts → Connect Google Account

# 3) Kiểm tra
curl http://localhost:8080/health
curl http://localhost:8080/account-limits
curl http://localhost:8080/v1/models
```

Khi chưa có tài khoản, `/v1/models` trả `503` và log hiển thị:

```
[AccountManager] No accounts file found at ~/.config/novoid-core/accounts.json
[AccountManager] No accounts configured ... Add accounts via WebUI or CLI.
```

Đây là trạng thái bình thường trước khi nạp account (xem [Operations](09-operations.md)).

## 5. Kết nối client

### Claude Code CLI

```bash
export ANTHROPIC_BASE_URL="http://localhost:8080"
export ANTHROPIC_API_KEY="dummy"     # hoặc giá trị API_KEY nếu bạn đặt
claude
```

WebUI cung cấp sẵn 2 preset (`Claude Thinking`, `Gemini 1M`) tại tab **Settings → Claude CLI config**, có thể ghi thẳng vào `~/.claude/settings.json`.

### Các harness khác

Bất kỳ client nào nói Anthropic Messages API đều dùng được:

```bash
export ANTHROPIC_BASE_URL="http://<host>:<port>"
export ANTHROPIC_API_KEY="<API_KEY hoặc dummy>"
```

Để dùng **tier routing** (`novoid-claude-opus-high`, v.v.), đặt `novoid-proxy` ở phía trước Core: <https://github.com/hanovoid/novoid-proxy>.

### Ví dụ request

```bash
curl http://localhost:8080/v1/messages \
  -H "content-type: application/json" \
  -H "x-api-key: $API_KEY" \
  -d '{
    "model": "claude-sonnet-4-6",
    "max_tokens": 1024,
    "stream": false,
    "messages": [{"role": "user", "content": "Xin chào"}]
  }'
```

## 6. Chạy dưới process manager / container

Khuyến nghị dùng `serve` (foreground) để PID 1 nhận tín hiệu:

```dockerfile
CMD ["novoid-core", "serve"]
```

```ini
[Service]
ExecStart=/usr/local/bin/novoid-core serve
Restart=on-failure
Environment=PORT=8080
```

Mount volume vào `~/.config/novoid-core/` để giữ `accounts.json` qua các lần restart/redeploy. Xem thêm [Operations](09-operations.md).
