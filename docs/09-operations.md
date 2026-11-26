# 09 — Vận hành, triển khai & xử lý sự cố

## 1. Khởi chạy production

### Local / VPS

```bash
cargo build --release
export API_KEY="..."
export WEBUI_PASSWORD="..."
export PORT=8080
./target/release/novoid-core serve      # foreground, dễ dùng với systemd
```

systemd unit tối giản:

```ini
[Unit]
Description=Novoid Core
After=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/novoid-core serve
Restart=on-failure
Environment=PORT=8080
Environment=API_KEY=...
Environment=WEBUI_PASSWORD=...

[Install]
WantedBy=multi-user.target
```

### Docker / Railway

- Dùng `serve` làm CMD để PID 1 nhận `SIGTERM`.
- **Bắt buộc mount persistent volume** vào thư mục config của HOME (`/root/.config/novoid-core` trong image phổ biến). Nếu không mount, mỗi lần redeploy sẽ mất `accounts.json` và server rơi vào trạng thái `No accounts configured`.
- Set `HOST=0.0.0.0`, `PORT` theo platform.
- Với domain public, dùng luồng OAuth `remote-loopback` trong WebUI hoặc set `OAUTH_CLIENT_ID` + `OAUTH_CLIENT_SECRET` + `OAUTH_REDIRECT_URI` trỏ về domain.

```bash
# Kiểm tra sau deploy
curl https://<domain>/health
curl https://<domain>/account-limits?refresh=true
```

## 2. Giám sát

| Tín hiệu | Nơi xem |
| :--- | :--- |
| Log realtime qua SSE | WebUI tab Logs hoặc `GET /api/logs/stream?history=true` |
| Ring buffer 1000 dòng | `GET /api/logs` |
| Trạng thái account & quota | `GET /health`, `GET /account-limits?includeHistory=true` |
| Health score/token bucket | `GET /api/strategy/health` |
| Usage history | `GET /api/stats/history`, `usage-history.json` |
| PID daemon | `novoid-core status`, `~/.config/novoid-core/server.pid` |

Mẫu log bình thường:

```
[Server] Account pool initialized: 1 total, 1 available, 0 rate-limited, 0 invalid
SUCCESS Server started successfully on port 8081
[API] Request for model: gemini-4-hydrogen-xhigh, stream: true
[POST] /v1/messages 200 (3296ms)
```

## 3. Xử lý sự cố theo log

### 3.1 `No accounts configured in /root/.config/novoid-core/accounts.json` + `/v1/models 503`

**Nguyên nhân:** instance khởi động với HOME=/root và volume chưa có `accounts.json` (deploy mới/redeploy mất volume).

**Xử lý:**
1. Mount volume vào `/root/.config/novoid-core`.
2. WebUI → Accounts → **Import** file `accounts.json` backup, hoặc **Connect Google Account** lại.
3. Hoặc CLI: `novoid-core accounts add --no-browser` (nhớ dừng server trước).
4. Verify: `novoid-core accounts verify`, `curl /account-limits?refresh=true`.

### 3.2 Bão `[Onboarding] onboardUser failed ... 429 Too Many Requests`

**Triệu chứng cũ:** `/v1/models` mất ~6.5s, `/v1/messages` chậm ~4s, log spam 429 trên cả 2 endpoint.

**Nguyên nhân:** `fetch_and_save_subscription()` được gọi khi `subscription.tier == "unknown"` dù account đã có `projectId`; hàm này gọi `discover_project()` → `onboardUser()` trên **mọi request**. Khi `onboardUser` dính 429, subscription vẫn `unknown` → lặp vô hạn.

**Đã xử lý trong code hiện tại:**
- `fetch_and_save_subscription()` gọi thẳng `get_subscription_tier()` (`loadCodeAssist` thuần), không gọi `onboardUser`.
- `unknown` được chuẩn thành `free` + `detectedAt` để không retry.
- `get_project_for_account()` có cooldown onboard 10 phút (`onboard_attempted_at`) và giữ subscription extract được ngay cả khi onboard thất bại.
- `onboard_user()` trả về ngay khi gặp `429` thay vì thử endpoint tiếp theo.

**Nếu vẫn thấy 429:** kiểm tra `accounts.json` xem `subscription.tier` có còn `unknown` không; đảm bảo deploy đúng bản đã sửa; bật `requestThrottlingEnabled` với `requestDelayMs` ~200–500ms.

### 3.3 `503 MODEL_CAPACITY_EXHAUSTED` chờ cứng 10s dù Google trả `retryDelay: 0s/3s/6s`

**Đã xử lý:** handler đọc `parse_reset_time(Some(&resp_headers), &err_text)`, ưu tiên `RetryInfo.retryDelay` thực tế, clamp trong `[300ms, fallback_tier]`; log hiển thị `ms` cho giá trị < 1s nhờ `format_duration()` cập nhật.

Nếu vẫn chờ lâu: giảm `maxCapacityRetries` hoặc chỉnh `capacityBackoffTiersMs` qua `POST /api/config`; kiểm tra `globalQuotaThreshold`.

### 3.4 `/v1/messages` trả 400 `RESOURCE_EXHAUSTED`

Đây là thiết kế có chủ đích (xem [API Reference](04-api-reference.md)): trả 400 thay vì 429 để client không retry storm. Đọc message để biết `Quota will reset after ...`. Dùng `/account-limits?format=table` để xem chi tiết từng account/model.

### 3.5 Account bị invalid / banned / cần verify

- Log/JSON chứa `ACCOUNT_BANNED` hoặc "violation of terms of service" → account bị chặn vĩnh viễn, cần xóa/thay.
- `validation required` → lấy `verifyUrl` từ `/api/accounts` và yêu cầu người dùng xác thực.
- `AUTH_INVALID` → thử `POST /api/accounts/:email/refresh`; nếu vẫn lỗi thì xóa và OAuth lại.

### 3.6 Tài khoản dính cooldown dù quota còn

Kiểm tra `consecutiveFailures` (runtime), `coolingDownUntil`, `modelRateLimits` qua `/api/strategy/health`. Reset bằng `POST /refresh-token` hoặc restart server. Giảm `maxConsecutiveFailures`/`extendedCooldownMs` nếu quá conservative.

## 4. Sao lưu & khôi phục

```bash
# Backup
cp ~/.config/novoid-core/accounts.json ./accounts.json.bak
cp ~/.config/novoid-core/config.json ./config.json.bak

# Restore
novoid-core stop
cp ./accounts.json.bak ~/.config/novoid-core/accounts.json
novoid-core start --log
```

WebUI Accounts tab có nút **Export / Import** tương đương. Không commit `accounts.json` vào git (chứa refresh token).

## 5. Bảo mật

- Đặt `API_KEY` và `WEBUI_PASSWORD` khi expose ra internet.
- `/api/config` GET không cần password để WebUI boot, nhưng đã che secret; vẫn nên đặt reverse proxy + IP whitelist nếu cần.
- CORS hiện `Any` — siết lại trong `build_router()` nếu deploy public với domain cụ thể.
- File `accounts.json` chứa refresh token plaintext: `chmod 600`, mount volume có permissions đúng.
- Không log token: `redact.js` che ở UI; log server chỉ ghi method/path/status.
- Rotate key định kỳ; `POST /refresh-token` xóa cache sau khi rotate.

## 6. Build & release checklist

```bash
cargo test
cargo build --release
# kiểm tra binary
./target/release/novoid-core --version
./target/release/novoid-core status
# smoke test
./target/release/novoid-core serve &
curl localhost:8080/health
curl localhost:8080/v1/models
```

Khi sửa code: cập nhật docs tương ứng (`docs/03`, `05`, `06`, `09`) và chỉ commit/push khi được yêu cầu.

## 7. Liên kết hệ sinh thái

- Layer 2 / Model Router + cài đặt CLI: <https://github.com/hanovoid/novoid-proxy>
- Novoid Core: <https://github.com/hanovoid/novoid-core>
