# 04 — API Reference

Base URL mặc định: `http://localhost:8080`. Port đổi qua `PORT` hoặc config `port`.

## 1. Xác thực

| Phạm vi | Cơ chế | Header / query |
| :--- | :--- | :--- |
| `/v1/*` | `API_KEY` (nếu đặt) | `Authorization: Bearer <key>` **hoặc** `x-api-key: <key>` |
| `/api/*`, `/health`, `/account-limits` | `WEBUI_PASSWORD` (nếu đặt) | `x-webui-password: <pwd>` hoặc `?password=<pwd>` |
| `GET /api/config`, `GET /api/auth/url`, `/oauth-callback` | Không yêu cầu password | cho phép WebUI khởi tạo & OAuth callback |

Nếu `API_KEY`/`WEBUI_PASSWORD` trống, middleware tương ứng cho qua. Response unauthorized:

```json
{ "type": "error", "error": { "type": "authentication_error", "message": "Invalid or missing API key" } }
```

Mọi request được log bởi `request_logging_middleware` dạng `[METHOD] /path status (Xms)`; log `count_tokens`, `/api/event_logging/batch`, `/.well-known/` ở mức debug.

## 2. API Anthropic-compatible

| Method | Path | Auth | Mô tả |
| :--- | :--- | :--- | :--- |
| `POST` | `/v1/messages` | `API_KEY` | Gửi message tới Google Cloud Code; hỗ trợ stream SSE. Header đặc biệt: `x-novoid-no-fallback: 1` (tắt fallback cho request này) |
| `GET` | `/v1/models` | `API_KEY` | Danh sách model khả dụng, union từ các account (cache 5 phút). `503` khi không có account enabled |
| `POST` | `/v1/messages/count_tokens` | `API_KEY` | Luôn trả `501 not_implemented` — client nên tự skip token counting |

### Request mẫu

```http
POST /v1/messages
content-type: application/json
x-api-key: <API_KEY>

{
  "model": "claude-sonnet-4-6",
  "max_tokens": 1024,
  "stream": true,
  "system": "You are helpful.",
  "messages": [{ "role": "user", "content": "Hello" }],
  "tools": [{ "name": "get_weather", "input_schema": { "type": "object", "properties": {} } }]
}
```

`max_tokens` được tự điền nếu thiếu. `stream=true` trả `text/event-stream` với các event: `message_start`, `thinking_delta`, `signature_delta`, `text_delta`, `tool_use`, `inlineData` (image), `message_delta`, `message_stop`.

## 3. Health, quota & tiện ích

| Method | Path | Query | Mô tả |
| :--- | :--- | :--- | :--- |
| `GET` | `/health` | — | Trạng thái server + summary account: `summary`, `counts`, `accounts[]` (latency, modelRateLimits, cooldownRemaining) |
| `GET` | `/account-limits` | `format=json|table`, `includeHistory=true`, `refresh=true` | Quota chi tiết từng account/model + thời gian reset. Cache 10s, single-flight; `refresh=true` bypass cache |
| `POST` | `/refresh-token` | — | Xóa token cache + cache model + signature caches, invalidate limits cache |
| `POST` | `/test/clear-signature-cache` | — | Xóa toàn bộ LRU signature cache (test endpoint) |
| `POST` | `/api/event_logging/batch` | — | Silent `{"status":"ok"}` — hấp thụ telemetry từ client |
| `GET` | `/` | — | WebUI shell (embedded) |
| `POST` | `/` | — | Silent OK |
| `*` | fallback | — | Mọi path không khớp được phục vụ bởi embedded asset handler (SPA fallback) |

Ví dụ `/account-limits?format=table` trả bảng text để dán vào terminal; `includeHistory=true` thêm `history` từ `usage-history.json`.

## 4. Account API (`/api/accounts`)

| Method | Path | Mô tả |
| :--- | :--- | :--- |
| `GET` | `/api/accounts` | Danh sách account (email, source, enabled, projectId, subscription, quota, rate limits) |
| `POST` | `/api/accounts/reload` | Reload `accounts.json` vào AccountManager |
| `GET` | `/api/accounts/export` | Export accounts dạng JSON để backup |
| `POST` | `/api/accounts/import` | Import accounts (accept `{accounts:[...]}` hoặc array) |
| `POST` | `/api/accounts/:email/refresh` | Làm mới access token cho account |
| `POST` | `/api/accounts/:email/toggle` | Bật/tắt account (`{"enabled": true|false}`) |
| `DELETE` | `/api/accounts/:email` | Xóa account |
| `PATCH` | `/api/accounts/:email` | Cập nhật metadata: `quotaThreshold`, `modelQuotaThresholds`, ... |

## 5. Config & settings API

| Method | Path | Mô tả |
| :--- | :--- | :--- |
| `GET` | `/api/config` | Config công khai (password/API key che `********`) |
| `POST` | `/api/config` | Deep-merge cấu hình mới, ghi atomic vào `config.json` |
| `POST` | `/api/config/password` | Đổi WebUI password (`oldPassword`, `newPassword`) |
| `GET` | `/api/settings` | Runtime settings (debug, devMode, logLevel, throttle, strategy) |
| `POST` | `/api/models/config` | Cập nhật `modelMapping` (`modelId`, `config`) |

## 6. Claude CLI config & presets

| Method | Path | Mô tả |
| :--- | :--- | :--- |
| `GET` | `/api/claude/config` | Đọc `~/.claude/settings.json` (env `ANTHROPIC_*`) |
| `POST` | `/api/claude/config` | Ghi cấu hình Claude CLI |
| `POST` | `/api/claude/config/restore` | Khôi phục cấu hình gốc |
| `GET` | `/api/claude/mode` | Xem chế độ đang dùng |
| `POST` | `/api/claude/mode` | Đổi chế độ (body `{"mode": "..."}`) |
| `GET` | `/api/claude/presets` | Danh sách preset (built-in + user) |
| `POST` | `/api/claude/presets` | Tạo preset (`name`, `config`) |
| `DELETE` | `/api/claude/presets/:name` | Xóa preset |
| `GET` | `/api/server/presets` | Danh sách server preset |
| `POST` | `/api/server/presets` | Tạo server preset |
| `PATCH` | `/api/server/presets/:name` | Cập nhật server preset |
| `DELETE` | `/api/server/presets/:name` | Xóa server preset |

## 7. Logs, stats & strategy

| Method | Path | Mô tả |
| :--- | :--- | :--- |
| `GET` | `/api/logs` | Toàn bộ ring-buffer log (tối đa 1000 dòng) |
| `GET` | `/api/logs/stream` | SSE realtime (`history=true` để gửi lại lịch sử trước khi stream) |
| `GET` | `/api/strategy/health` | Health score, token bucket, quota và trạng thái từng account theo strategy hiện tại |
| `GET` | `/api/stats/history` | Lịch sử usage từ `usage-history.json` |

## 8. OAuth WebUI flow

| Method | Path | Query / body | Mô tả |
| :--- | :--- | :--- | :--- |
| `GET` | `/api/auth/url` | `local=0` | Tạo PKCE state + verifier, trả authorization URL. Khi deploy public trả URL dạng `remote-loopback` |
| `GET` | `/api/auth/status` | `state=<state>` | Poll trạng thái flow: `pending`, `success` (kèm email), `error` |
| `POST` | `/api/auth/complete` | `{callbackInput, state}` | Hoàn tất flow bằng cách dán callback URL hoặc authorization code |
| `GET` | `/oauth-callback` | `code`, `state`, `error` | Google redirect về; đổi code lấy token, lưu account |

Luồng chi tiết tại [Account Management](05-account-management.md).

## 9. Định dạng lỗi

Mọi lỗi từ handler `/v1/messages` được map qua `NovoidError::to_anthropic_response()`:

```json
{
  "type": "error",
  "error": {
    "type": "invalid_request_error",
    "message": "RESOURCE_EXHAUSTED: You have exhausted your capacity on the model. Please wait for your quota to reset."
  }
}
```

| Error variant | HTTP | `error.type` |
| :--- | :--- | :--- |
| `ModelNotSupported` | 404 | `not_found_error` |
| `AuthInvalid`, `AuthInvalidPermanent`, `UpstreamApi 401` | 401 | `authentication_error` |
| `RateLimited`, `CapacityExhausted`, `InvalidRequest`, quota exhaustion | 400 | `invalid_request_error` |
| `AccountForbidden`, `AccountBanned`, `UpstreamApi 403` | 403 | `permission_error` |
| `NoAccounts`, "All endpoints failed" | 503 | `api_error` |
| `UpstreamApi 404` | 404 | `not_found_error` |
| Mặc định | 500 | `api_error` |

> Rate-limit/capacity cố tình trả **400** thay vì 429 để tránh client retry storm; thông tin reset nằm trong message (`Quota will reset after 5m...`).

## 10. CORS & body limit

- CORS: `allow_origin(Any)`, `allow_methods(Any)`, `allow_headers(Any)`.
- Body limit: 50 MB (`DefaultBodyLimit::max`).
- Middleware thứ tự: CORS → body limit → request logging → router (auth middleware trên từng nhóm route).
