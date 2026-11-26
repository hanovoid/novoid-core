# 06 — Cloud Code upstream & chuyển đổi định dạng

## 1. Upstream endpoints

| Hằng số | URL | Dùng cho |
| :--- | :--- | :--- |
| `ANTIGRAVITY_ENDPOINT_DAILY` | `https://daily-cloudcode-pa.googleapis.com` | `generateContent`, `streamGenerateContent` (thử trước), `onboardUser` |
| `ANTIGRAVITY_ENDPOINT_PROD` | `https://cloudcode-pa.googleapis.com` | fallback, `loadCodeAssist` (thử trước) |
| `LOAD_CODE_ASSIST_ENDPOINTS` | PROD → DAILY | Discover project / subscription |
| `ONBOARD_USER_ENDPOINTS` | DAILY → PROD | Onboard account mới |

> Tên hằng số giữ dạng `ANTIGRAVITY_*` để tương thích với giao thức Google; không phải thương hiệu dự án.

Header gửi upstream (`antigravity_headers()`, `build_headers()`):

| Header | Giá trị |
| :--- | :--- |
| `Authorization` | `Bearer <access_token>` |
| `Content-Type` | `application/json` |
| `User-Agent` | `antigravity/hub/<version> <os>/<arch>` |
| `X-Client-Name` | `antigravity` |
| `X-Client-Version` | config/env hoặc `product.json` hoặc `1.110.0` |
| `x-goog-api-client` | `gl-node/18.18.2 fire/0.8.6 grpc/1.10.x` |
| `Accept` | `text/event-stream` (SSE) hoặc `application/json` |
| session header | `sessionId` từ `derive_session_id()` |

HTTP client dùng `reqwest` + `rustls` + HTTP/2 connection pooling, có outbound proxy qua `HTTP_PROXY`/`HTTPS_PROXY` và optional throttle (`maybe_throttle()`).

## 2. Vòng lặp gửi request (`src/cloudcode/handler.rs`)

```
max_attempts = max(config.maxRetries, số account)
for attempt in 0..max_attempts:
    clear_expired_limits()
    available = accounts có thể dùng cho model
    if available empty:
        - all invalid → NoAccounts (kèm lý do)
        - all rate-limited → chờ minWait (nếu > maxWaitBeforeErrorMs → fallback hoặc RateLimited)
        - không account support model → ModelNotSupported
    select_account(model) → (account, wait_ms)
    nếu wait_ms > 0 → sleep (throttling / fallback mode)
    token = get_token_for_account()
    project = get_project_for_account()
    payload = build_cloudcode_request(anthropic_request, project, email)
    for endpoint in [daily, prod]:
        POST /v1internal:generateContent | streamGenerateContent?alt=sse
        xử lý status (xem bảng bên dưới)
```

### Xử lý status upstream

| Status / điều kiện | Hành vi |
| :--- | :--- |
| `200` | Trả `ReadyStream` (response + model + account) |
| `400` + `MODEL_NOT_SUPPORTED` | `ModelNotSupported`; mark unsupported cho account; thử account khác |
| `401` | `AuthInvalid` / `AuthInvalidPermanent`; thử account khác |
| `403` + banned | `AccountBanned` (từ `is_account_banned()`) |
| `403` + validation required | `InvalidRequest` kèm `verifyUrl` |
| `429` | `RateLimited`; parse `resetTime` từ header/body; mark cooldown; thử account khác |
| `503` / `529` + `MODEL_CAPACITY_EXHAUSTED` | Retry trong cùng endpoint theo `capacityBackoffTiersMs`, ưu tiên `RetryInfo.retryDelay` từ body (clamp 300ms … fallback tier). Sau `maxCapacityRetries` → `CapacityExhausted`, mark rate-limited, đổi account |
| `404` | `ModelNotSupported` |
| `>= 500` khác | log warn, sleep 3s, thử endpoint tiếp; đếm consecutive failure → extended cooldown sau `maxConsecutiveFailures` |
| Empty response | `MAX_EMPTY_RESPONSE_RETRIES = 1` — thử lại trước khi trả `EmptyResponse` |

**Smart backoff** (`src/cloudcode/rate_limit_state.rs`): lưu trạng thái per `(email, model)`, tăng dần theo số lần dính limit, reset sau `RATE_LIMIT_STATE_RESET_MS = 120s` (background cleanup mỗi 60s). Delay = `base * 2^(attempt-1)`, tối đa 60s. Nếu cùng key dính limit trong `rateLimitDedupWindowMs` (5s) thì đánh dấu duplicate và **không** tăng attempt. `calculate_smart_backoff()` ưu tiên reset từ server (clamp ≥5s), nếu không có thì dùng tier theo lý do; capacity cộng jitter ±5s.

### Rate limit parser (`src/cloudcode/rate_limit_parser.rs`)

`parse_reset_time(headers, error_text)` đọc theo thứ tự:
1. `Retry-After` (giây hoặc HTTP-date), `x-ratelimit-reset` (epoch giây), `x-ratelimit-reset-after` (giây).
2. Body JSON: `quotaResetDelay`, `quotaResetTimeStamp`, `retryAfterMs` / `retryDelay`, `retry after N sec`, `Nh Nm Ns`, `reset:<ISO8601>`.
3. Text `quota will reset after ...`.

Chuẩn hóa cuối: ≤0 → 500ms; <500ms → +200ms. `parse_rate_limit_reason()` trả `RateLimitReason` (`RateLimitExceeded`, `QuotaExhausted`, `ModelCapacityExhausted`, `ServerError`, `Unknown`) — ưu tiên quota → capacity → rate-limit → server error; status 503/529 → capacity, 500 → server error.

### Model fallback

Khi `--fallback` bật (hoặc `FALLBACK=true`) và mọi account cạn/quota vượt `maxWaitBeforeErrorMs`, handler đổi `request.model` sang `get_fallback_model(model)` rồi đệ quy lại với `fallback_enabled=false` (tránh vòng lặp). Header `x-novoid-no-fallback: 1` từ client tắt hành vi này cho từng request.

## 3. Model API (`src/cloudcode/model_api.rs`)

- `fetch_available_models(token, email)`: gọi `/v1internal:fetchAvailableModels`, lấy model + quota, lọc placeholder/disabled/hidden.
- `populate_model_cache_for_accounts()`: single-flight (`fetch_lock`) + TTL `MODEL_VALIDATION_CACHE_TTL_MS = 5 phút`.
- `list_models_for_accounts()`: union model từ mọi account, trả định dạng OpenAI-compatible (`created = now/1000`, `owned_by: "anthropic"`, description từ `displayName`, sort theo id). Ẩn model kết thúc `-disabled`, `gemini-pro-agent`, và model family không phải `claude`/`gemini`.
- `get_model_quotas()`: quota per model từ cache hoặc gọi lại upstream; ưu tiên entry có `quotaInfo` khi merge nhiều account.
- `model_registry.rs`: registry RAM ghi nhận `family`, `supportsThinking`, `maxOutputTokens`, `displayName` từ `fetchAvailableModels`; chỉ nhận model có `claude`/`gemini` trong tên hoặc nằm trong `agentModelSorts`/`tieredModelIds` với provider hợp lệ. `clear_model_registry()` hiện chưa có caller trong `src/`.
- `get_subscription_tier(token)`: `loadCodeAssist` → `paidTier`/`currentTier`/`allowedTiers` → `parse_tier_id()`. Trả lỗi `ACCOUNT_BANNED` nếu 403 kèm "disabled ... violation of terms of service".
- `is_valid_model()`: fail-open nếu cache chưa có.

## 4. Session ID (`src/cloudcode/session_manager.rs`)

- `derive_session_id(anthropic_request, account_email)`: nếu có user message đầu tiên → SHA-256 của `"<email|anonymous>\0<first user text>"`, cắt thành dạng UUID-like + tail thập phân — cùng prompt đầu + email cho cùng `sessionId` (system prompt **không** tham gia hash).
- Nếu không có text: dùng ID đã lưu theo email trong RAM (`RUNTIME_SESSION_STORE`), hoặc tạo mới (UUID v4 + epoch ms). Email rỗng → luôn tạo mới. `clear_session_store()` để reset; state mất sau restart.

## 5. Request builder (`src/cloudcode/request_builder.rs`)

- `build_cloudcode_request(anthropic_request, project, email)`: gọi `convert_anthropic_to_google()`, thêm `project` vào metadata, prepend system prompt của caller (sau khi scrub identity), trả `(payload, session_id)`.
- `identity_scrub_rules()`/`scrub_client_identity()`: loại bỏ thông tin nhận dạng client nhạy cảm (`Nous Research`, `Hermes Agent`, `Claude Code`, `Anthropic`, ...) khỏi system text; mở rộng qua env `ANTIGRAVITY_SCRUB_IDENTITY` (`term=>replacement`, phân cách dấu phẩy).
- Caller system prompt sau khi scrub được đưa vào `contents` dạng `<system-instructions>...</system-instructions>` (prepend vào message user đầu hoặc tạo message user mới); `systemInstruction` gốc bị thay bằng `ANTIGRAVITY_SYSTEM_INSTRUCTION` + block `[ignore]`.
- Payload envelope: `userAgent: "antigravity"`, `requestType: "agent"`, `requestId: "agent-<uuid>"`.
- `build_headers(model, accept, session_id)`: header mục 1 + `X-Machine-Session-Id`; với Claude thinking thêm `anthropic-beta: interleaved-thinking-2025-05-14`.

## 6. SSE streaming (`src/cloudcode/sse_streamer.rs`)

`stream_sse_response(response, model)` đọc từng chunk `data: {...}` từ Google và phát event Anthropic:

| Upstream chunk | Event Anthropic |
| :--- | :--- |
| Bắt đầu | `message_start` (role `assistant`, usage input) |
| Thought part | `thinking_delta` + `signature_delta` (kèm `signature`) |
| Text part | `text_delta` |
| `functionCall` | `tool_use` (id, name, input JSON) |
| `inlineData` | image block |
| Kết thúc | `message_delta` (stop_reason, output tokens) + `message_stop` |

Chi tiết: `message_start` chỉ phát khi parts đầu tiên có nội dung; nếu không có thì stream trả `EmptyResponse` để handler retry. Usage: `input_tokens = promptTokenCount - cachedContentTokenCount`, `output_tokens = candidatesTokenCount + thoughtsTokenCount`. `tool_use` không có id từ upstream được cấp `toolu_<uuid>`; image → block `image` base64.

`parse_thinking_sse_response()` dùng cho non-streaming: gộp toàn bộ SSE thành một response JSON Anthropic (có `thinking` block). `empty_response_fallback_events()` phát event tối thiểu kết thúc bằng text `"[No response after retries - please try again]"` để client không treo.

## 7. Chuyển đổi định dạng (`src/format/*`)

### 7.1 Request: Anthropic → Google (`request.rs`, `content.rs`)

| Anthropic | Google |
| :--- | :--- |
| `system` (string hoặc blocks) | `systemInstruction.parts[]` |
| `messages[].role=user/assistant` | `contents[].role=user/model` |
| `content: "text"` / `{type:"text"}` | `parts[].text` |
| `{type:"image", source:{data,media_type}}` | base64 → `parts[].inlineData`; URL → `parts[].fileData` (default `image/jpeg`) |
| `{type:"document"}` | base64 → `parts[].inlineData` (`application/pdf`); URL → `fileData` hoặc text |
| `{type:"tool_use"}` | `parts[].functionCall {name,args}` |
| `{type:"tool_result"}` | `parts[].functionResponse {name,response}` |
| `thinking` block (có signature) | `parts[].thought` + `thoughtSignature` |
| `tools[].input_schema` | `functionDeclarations[].parameters` (đã sanitize) |
| `tool_choice` | `toolConfig.functionCallingConfig` |
| `max_tokens`, `temperature`, `top_p`, `top_k` | `generationConfig` |
| `thinking.budget_tokens` | `generationConfig.thinkingConfig.thinkingBudget` (clamp theo model) |

`strip_billing_header()` loại bỏ dòng `x-anthropic-billing-header:` khỏi text system (case-insensitive). Với Claude thinking + tools, hint interleaved-thinking được thêm vào system text. `cache_control` bị loại khỏi messages. Tên tool chỉ giữ `[A-Za-z0-9_-]`, các ký tự khác → `_`, tối đa 64 ký tự.

Thinking budget: Claude mặc định 32.000 và tự nâng `maxOutputTokens` lên `budget + 8192` nếu quá nhỏ; Gemini budget được clamp (2.5 → 24.576), `max_tokens <= 1024` → budget 32; trần cứng `GEMINI_MAX_OUTPUT_TOKENS = 65536`.

### 7.2 Response: Google → Anthropic (`response.rs`)

Chuyển `candidates[0].content.parts[]` thành `content[]` blocks; map `finishReason` → `stop_reason` (`MAX_TOKENS`→`max_tokens`, tool call→`tool_use`, còn lại `end_turn`); gom `usageMetadata` thành `usage` (`input_tokens` trừ cached tokens, `output_tokens` gồm thought tokens). Message id `msg_<uuid>`; nếu không có block nào thì chèn một text block rỗng. Chấp nhận cả response bọc trong `response` hoặc raw.

### 7.3 Schema sanitizer (`schema_sanitizer.rs`)

Google API chỉ chấp nhận subset JSON Schema. `sanitize_schema()` và `clean_schema()` thực hiện:

- Allowlist giữ lại: `type`, `description`, `properties`, `required`, `items`, `enum`, `title`; schema object rỗng được cấp placeholder property `reason: string`.
- `$ref` → `{ type: "object", description: "See: <def>" }` (`$defs`/`definitions` bị loại bỏ).
- `enum` (2–10 giá trị) & `additionalProperties: false` → thêm description hint.
- `allOf` → merge các object (`merge_all_of`).
- `anyOf`/`oneOf` → flatten thành lựa chọn có điểm cao nhất (`score_schema_option`) + hint `"Accepts: ..."` (`flatten_any_of_one_of`).
- `type` dạng array (nullable) → chọn type đầu tiên khác `null`, bỏ property nullable khỏi `required` (`flatten_type_arrays`).
- Constraint hints (`minLength`, `maxLength`, `pattern`, `minimum`, `maximum`, `minItems`, `maxItems`, `format`) → nối vào `description` (`move_constraints_to_description`).
- `to_google_type()` map sang `OBJECT`, `ARRAY`, `STRING`, `NUMBER`, `INTEGER`, `BOOLEAN` (`null` → `STRING`).
- `clean_schema_final()` xóa 19 key không hỗ trợ, đảm bảo `ARRAY` có `items: STRING`, xóa `properties`/`required` khỏi non-`OBJECT` và prune các mục `required` không tồn tại trong `properties`.

### 7.4 Thinking & signature (`thinking.rs`, `signature_cache.rs`)

- `clamp_gemini_thinking_budget()` giới hạn budget theo model (mặc định 16.000, limit 128.000; Gemini 2.5 clamp 24.576; trần output `GEMINI_MAX_OUTPUT_TOKENS = 65536`).
- `filter_unsigned_thinking_blocks()` / `sanitize_thinking_part()` loại thinking block không có chữ ký hợp lệ (`MIN_SIGNATURE_LENGTH = 50`). Với Gemini `functionCall`, nếu thiếu `thoughtSignature` (trong block hoặc cache) thì dùng `GEMINI_SKIP_SIGNATURE = "skip_thought_signature_validator"`.
- `needs_thinking_recovery()` + `close_tool_loop_for_thinking()` khôi phục thinking bị thiếu trong tool-loop (chèn synthetic message `"[Tool call was interrupted.]"` hoặc `"[Tool execution completed.]"` + `"[Continue]"`). Đặt `ANTIGRAVITY_DISABLE_THINKING_RECOVERY=1` để tắt cho đích Gemini.
- `restore_thinking_signatures()` loại Claude thinking block thiếu signature hợp lệ; `reorder_assistant_content()` chuẩn hóa thứ tự `thinking → text → tool_use`.
- `signature_cache.rs`: `OrderedLru` dùng `BTreeMap` (mặc định 20.000 tool signatures, 10.000 thinking signatures), flush mỗi 10s qua file tạm `json.tmp-<pid>` ra `signature-store.json` (và flush đồng bộ lúc shutdown); clear qua `POST /test/clear-signature-cache` hoặc `POST /refresh-token`.

## 8. Kiểm tra nhanh

```bash
# Non-stream
curl localhost:8080/v1/messages -H 'content-type: application/json' \
  -d '{"model":"gemini-3-flash","max_tokens":64,"messages":[{"role":"user","content":"hi"}]}'

# Streaming
curl -N localhost:8080/v1/messages -H 'content-type: application/json' \
  -d '{"model":"claude-sonnet-4-6","max_tokens":64,"stream":true,"messages":[{"role":"user","content":"hi"}]}'
```
