# 03 — Cấu hình & lưu trữ

## 1. Đường dẫn lưu trữ

Mọi dữ liệu runtime nằm trong `~/.config/novoid-core/` (`config_dir()` trong `src/config.rs`). Không có fallback lưu vào Desktop hay thư mục dùng chung của hệ điều hành khác.

| File | Hàm tạo path | Nội dung |
| :--- | :--- | :--- |
| `config.json` | `config_file_path()` | `AppConfig` dạng JSON (camelCase), ghi atomic qua `config.json.tmp` |
| `accounts.json` | `account_config_path()` | Danh sách account, refresh token, project ID, quota, settings |
| `usage-history.json` | `usage_history_path()` | Lịch sử dùng token theo model/account |
| `signature-store.json` | `signature_store_path()` | LRU cache chữ ký thinking/tool |
| `claude-presets.json` | `claude_presets_path()` | Preset cấu hình Claude CLI do người dùng lưu |
| `server-presets.json` | `server_presets_path()` | Preset tham số server do người dùng lưu |
| `server.pid` | `pid_file_path()` | PID của daemon background |

**Ghi đè path:**
- `account_config_path`: config key `accountConfigPath` (chuỗi, dùng cho multi-instance).
- signature store: env `ANTIGRAVITY_SIGNATURE_STORE` (tên biến giữ lại để tương thích; giá trị mặc định vẫn là `~/.config/novoid-core/signature-store.json`).

**Load config:** ưu tiên `~/.config/novoid-core/config.json`; nếu chưa có thì thử `./config.json` trong working directory (tương thích legacy). `deep_merge` trộn user config vào default, chặn key `__proto__`, `constructor`, `prototype`.

## 2. `AppConfig` — giá trị mặc định

Định nghĩa: `src/config.rs` (`#[serde(rename_all = "camelCase")]`).

| Key | Type | Mặc định | Ý nghĩa |
| :--- | :--- | :--- | :--- |
| `apiKey` | string | `""` | Khóa bảo vệ `/v1/*` |
| `webuiPassword` | string | `""` | Mật khẩu WebUI + `/api/*` |
| `debug` | bool | `false` | Log debug |
| `devMode` | bool | `false` | Chế độ dev |
| `logLevel` | string | `"info"` | Mức log |
| `maxRetries` | usize | `2` | Số retry upstream tối đa |
| `retryBaseMs` | u64 | `3000` | Backoff retry cơ sở |
| `retryMaxMs` | u64 | `60000` | Trần backoff retry |
| `persistTokenCache` | bool | `false` | Lưu cache token xuống đĩa |
| `defaultCooldownMs` | u64 | `30000` | Cooldown mặc định khi rate-limit |
| `maxWaitBeforeErrorMs` | u64 | `120000` | Thời gian chờ tối đa trước khi trả lỗi (sticky) |
| `maxAccounts` | usize | `10` | Số account tối đa |
| `globalQuotaThreshold` | f64 | `0.0` | Ngưỡng quota global (0 = tắt) |
| `requestThrottlingEnabled` | bool | `false` | Bật delay giữa các upstream call |
| `requestDelayMs` | u64 | `200` | Delay khi throttle bật |
| `rateLimitDedupWindowMs` | u64 | `5000` | Cửa sổ chống trùng thông báo rate-limit |
| `maxConsecutiveFailures` | u32 | `2` | Số lỗi liên tiếp trước khi đưa account vào cooldown dài |
| `extendedCooldownMs` | u64 | `180000` | Cooldown mở rộng |
| `maxCapacityRetries` | usize | `2` | Retry tối đa cho `MODEL_CAPACITY_EXHAUSTED` |
| `switchAccountDelayMs` | u64 | `8000` | Delay trước khi switch account |
| `capacityBackoffTiersMs` | u64[] | `[10000, 20000, 40000, 60000]` | Backoff theo tầng khi capacity exhausted |
| `modelMapping` | object | `{}` | Ánh xạ model name → model name khác / pin / hide |
| `antigravityClientVersion` | string | `""` | Ghi đè `X-Client-Version` |
| `antigravityIdeVersion` | string | `""` | Ghi đè version trong User-Agent |
| `accountSelection` | object | xem bên dưới | Cấu hình chiến lược chọn account |
| `port` | u16? | `null` | Port server (bị `PORT` env ghi đè) |
| `tokenCacheTtlMs` | u64? | `null` | TTL cache token, mặc định runtime `50 phút` |
| `accountConfigPath` | string? | `null` | Ghi đè đường dẫn `accounts.json` |
| `firstRetryDelayMs` | u64? | `null` | Ghi đè delay retry đầu tiên (mặc định `3000`) |
| `rateLimitStateResetMs` | u64? | `null` | Ghi đè chu kỳ reset trạng thái rate-limit (mặc định `120000`) |

### `accountSelection`

| Key | Mặc định | Ý nghĩa |
| :--- | :--- | :--- |
| `strategy` | `"hybrid"` | `hybrid`, `sticky`, `round-robin` |
| `healthScore.initial` | `70` | Điểm sức khỏe ban đầu |
| `healthScore.successReward` | `1` | Cộng điểm khi request thành công |
| `healthScore.rateLimitPenalty` | `-10` | Trừ điểm khi dính rate-limit |
| `healthScore.failurePenalty` | `-20` | Trừ điểm khi lỗi khác |
| `healthScore.recoveryPerHour` | `10` | Điểm hồi phục mỗi giờ |
| `healthScore.minUsable` | `50` | Ngưỡng điểm tối thiểu để dùng |
| `healthScore.maxScore` | `100` | Trần điểm |
| `tokenBucket.maxTokens` | `50` | Số token bucket tối đa |
| `tokenBucket.tokensPerMinute` | `6` | Tốc độ hồi token |
| `tokenBucket.initialTokens` | `50` | Token ban đầu |
| `quota.lowThreshold` | `0.10` | Ngưỡng quota thấp (10%) |
| `quota.criticalThreshold` | `0.05` | Ngưỡng quota nguy hiểm (5%) |
| `quota.staleMs` | `300000` | Thời gian coi quota là cũ (5 phút) |
| `weights.health` | `2` | Trọng số health |
| `weights.tokens` | `5` | Trọng số token bucket |
| `weights.quota` | `3` | Trọng số quota |
| `weights.lru` | `0.1` | Trọng số LRU |

## 3. Biến môi trường

Đọc trực tiếp trong `src/config.rs`, `src/cli/mod.rs`, `src/constants.rs`, `src/server/*`:

| Biến | Mặc định | Nơi đọc | Ý nghĩa |
| :--- | :--- | :--- | :--- |
| `PORT` | `8080` | `cli::get_port()` | Port bind (ưu tiên hơn config `port`) |
| `HOST` | `0.0.0.0` | `cli::get_host()` | Địa chỉ bind |
| `API_KEY` | `""` | `load_initial_config()` | Ghi đè `apiKey` |
| `WEBUI_PASSWORD` | `""` | `load_initial_config()` | Ghi đè `webuiPassword` |
| `DEBUG` | — | config + CLI | `true` → bật debug/dev |
| `DEV_MODE` | — | config + CLI | `true` → bật dev mode |
| `FALLBACK` | `false` | CLI | `true` → bật model fallback |
| `ACCOUNT_STRATEGY` | `hybrid` | WebUI/runtime | Ghi đè chiến lược chọn account |
| `OAUTH_CLIENT_ID` | client mặc định | `oauth_client_id()` | Google OAuth Web Client ID |
| `OAUTH_CLIENT_SECRET` | secret mặc định | `oauth_client_secret()` | Google OAuth secret |
| `OAUTH_REDIRECT_URI` | `http://localhost:<callbackPort>/oauth-callback` | `oauth_default_redirect_uri()` | Ghi đè redirect URI |
| `OAUTH_CALLBACK_PORT` | `51121` | `oauth_callback_port()` | Port callback local; fallback `51122–51126` |
| `ANTIGRAVITY_CLIENT_VERSION` | từ `product.json` hoặc `1.110.0` | `get_client_version()` | Header `X-Client-Version` |
| `ANTIGRAVITY_CLIENT_VERSION_FALLBACK` | `1.110.0` | `get_client_version()` | Version dự phòng |
| `FALLBACK_ANTIGRAVITY_VERSION` | `2.32.0` | `get_platform_user_agent()` | Version trong UA `antigravity/hub/...` |
| `ANTIGRAVITY_SIGNATURE_STORE` | `~/.config/novoid-core/signature-store.json` | `signature_store_path()` | Đường dẫn signature store |
| `ANTIGRAVITY_SIGNATURE_CACHE_MAX_TOOL` | `20000` | `signature_cache.rs` | Số tool signature tối đa trong LRU (phải > 0) |
| `ANTIGRAVITY_SIGNATURE_CACHE_MAX_THINK` | `10000` | `signature_cache.rs` | Số thinking signature tối đa trong LRU (phải > 0) |
| `ANTIGRAVITY_SCRUB_IDENTITY` | — | `request_builder.rs` | Bổ sung rule scrub `term=>replacement` (phân cách bằng dấu phẩy) cho system prompt gửi upstream |
| `ANTIGRAVITY_DISABLE_THINKING_RECOVERY` | — | `thinking.rs` | `1` → tắt thinking recovery cho đích Gemini |
| `CLAUDE_CONFIG_PATH` | `~/.claude` | `claude_config.rs` | Thư mục chứa `settings.json` của Claude CLI |
| `NOVOID_ACCOUNTS_JSON` / `ACCOUNTS_JSON` | — | `account/storage.rs` | JSON `AccountsFile` hoặc mảng `Account[]` dùng để tự khởi tạo `accounts.json` trên container/Railway khi file chưa tồn tại |
| `HTTP_PROXY` / `HTTPS_PROXY` | — | `utils::http_client()` | Outbound proxy tới Google (có thể dùng lowercase; reqwest cũng đọc `ALL_PROXY`) |

> Các biến `ANTIGRAVITY_*` là tên kỹ thuật dùng để tương thích với Google Cloud Code; không phải thương hiệu dự án.

## 4. Model mapping & fallback

### Built-in alias (`resolve_builtin_model_mapping`)

| Model client gửi | Được đổi thành |
| :--- | :--- |
| `claude-3-5-sonnet-20241022` | `claude-sonnet-4-6` |
| `claude-3-7-sonnet-20250219` | `claude-sonnet-4-6` |
| `claude-opus-5-5`, `claude-opus-5` | `claude-opus-4-6-thinking` |
| `claude-sonnet-5-5`, `claude-sonnet-5` | `claude-sonnet-4-6` |
| `claude-haiku-5-5` | `claude-sonnet-4-6` |

### Fallback chain (`get_fallback_model`, chỉ khi `--fallback`)

```
gemini-4-hydrogen-high   → claude-opus-4-6-thinking
gemini-4-hydrogen-medium → claude-sonnet-4-6
gemini-3-flash           → gemini-2.5-flash
gemini-2.5-flash         → claude-sonnet-4-6
claude-opus-4-6-thinking → claude-sonnet-4-6
claude-sonnet-4-6        → gemini-3-flash
```

Header `x-novoid-no-fallback: 1` trong request `/v1/messages` vô hiệu hóa fallback cho request đó.

### `modelMapping` trong config

```json
{
  "modelMapping": {
    "my-custom-name": { "mapping": "claude-sonnet-4-6", "pinned": true, "hidden": false }
  }
}
```

- `mapping`: đổi tên model trước khi gửi upstream.
- `pinned`: giữ model này khi client yêu cầu tương đương.
- `hidden`: ẩn khỏi `/v1/models`.

## 5. Presets

### Server presets (built-in, `default_server_presets()`)

| Tên | Strategy | Đặc điểm chính |
| :--- | :--- | :--- |
| `Default (3-5 Accounts)` | hybrid | Cấu hình mặc định như bảng trên |
| `Many Accounts (10+)` | hybrid | `maxAccounts: 50`, cooldown ngắn hơn, ưu tiên health, quota threshold 10% |
| `Conservative` | sticky | Retry/cooldown dài, `globalQuotaThreshold: 0.20`, ưu tiên prompt cache |

### Claude presets (built-in, `default_claude_presets()`)

| Tên | Model chính | Dùng cho |
| :--- | :--- | :--- |
| `Claude Thinking` | `claude-opus-4-6-thinking` | Claude Code với reasoning |
| `Gemini 1M` | `gemini-3-flash` | Ngữ cảnh dài / chi phí thấp |

Cả hai đặt `ANTHROPIC_BASE_URL=http://localhost:8080` và `ANTHROPIC_AUTH_TOKEN=test`. Preset do người dùng tạo lưu tại `claude-presets.json` / `server-presets.json`, quản lý qua API.

**Lưu ý:** 3 server preset built-in **không thể** bị ghi đè, đổi tên hay xóa (`server_presets.rs`). Claude preset cùng tên sẽ bị thay thế bởi preset mới. `config.json` và `accounts.json` ghi atomic qua file `.tmp`; `server-presets.json` ghi trực tiếp (không atomic).

## 6. Hằng số upstream quan trọng

| Hằng số | Giá trị | Ý nghĩa |
| :--- | :--- | :--- |
| `DEFAULT_PROJECT_ID` | `rising-fact-p41fc` | Project ID khi không discover được |
| `REQUEST_BODY_LIMIT_BYTES` | `50 MB` | Giới hạn body HTTP |
| `MAX_EMPTY_RESPONSE_RETRIES` | `1` | Retry khi upstream trả rỗng |
| `RATE_LIMIT_STATE_RESET_MS` | `120000` | Reset trạng thái rate-limit |
| `FIRST_RETRY_DELAY_MS` | `3000` | Delay retry đầu |
| `MIN_BACKOFF_MS` | `5000` | Backoff tối thiểu |
| `CAPACITY_JITTER_MAX_MS` | `10000` | Jitter tối đa |
| `MIN_SIGNATURE_LENGTH` | `50` | Độ dài tối thiểu của signature hợp lệ |
| `GEMINI_MAX_OUTPUT_TOKENS` | `65536` | Trần output token |
| `MODEL_VALIDATION_CACHE_TTL_MS` | `300000` | TTL cache validate model |
| `QUOTA_EXHAUSTED_BACKOFF_TIERS_MS` | `[60s, 300s, 1800s, 7200s]` | Backoff khi hết quota |
| `BACKOFF_RATE_LIMIT_EXCEEDED_MS` | `60000` | Backoff 429 |
| `BACKOFF_MODEL_CAPACITY_EXHAUSTED_MS` | `30000` | Backoff capacity exhausted |
| `BACKOFF_SERVER_ERROR_MS` | `45000` | Backoff 5xx |
| `BACKOFF_UNKNOWN_MS` | `120000` | Backoff lỗi không rõ |
| `DEFAULT_PORT` | `8080` | Port mặc định |

## 7. Cập nhật config an toàn

- Dùng `POST /api/config` (deep merge, ghi atomic) hoặc sửa `config.json` khi server đã dừng.
- `get_public_config()` che `apiKey` và `webuiPassword` thành `********` khi trả về WebUI.
- Sau khi đổi cấu hình ảnh hưởng quota/account, gọi lại `/account-limits?refresh=true` để invalidate cache 10s.
