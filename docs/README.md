# Novoid Core — Tài liệu kỹ thuật

Bộ tài liệu đầy đủ cho **Novoid Core v2.7.7** — proxy server Rust tương thích Anthropic Messages API, kết nối Google Cloud Code, cân bằng tải đa tài khoản và WebUI nhúng.

## Mục lục

| Tài liệu | Nội dung |
| :--- | :--- |
| [01 — Architecture](01-architecture.md) | Kiến trúc tổng thể, sơ đồ luồng dữ liệu, vòng đời request, các lớp runtime |
| [02 — Getting Started](02-getting-started.md) | Build, cài đặt, CLI, khởi chạy daemon, kết nối client |
| [03 — Configuration](03-configuration.md) | `config.json`, giá trị mặc định, biến môi trường, presets, đường dẫn lưu trữ |
| [04 — API Reference](04-api-reference.md) | Toàn bộ route HTTP (`/v1/*`, `/api/*`, WebUI, OAuth), xác thực, lỗi |
| [05 — Account Management](05-account-management.md) | OAuth PKCE, lưu trữ tài khoản, 3 chiến lược chọn tài khoản, cooldown & quota |
| [06 — Cloud Code & Format](06-cloudcode-and-format.md) | Upstream handler, retry/capacity, SSE streaming, chuyển đổi Anthropic ↔ Google, schema & thinking |
| [07 — WebUI](07-webui.md) | Kiến trúc frontend Alpine.js, stores, components, views, translations |
| [08 — Module Map](08-module-map.md) | Tham chiếu từng file/thư mục trong repository |
| [09 — Operations](09-operations.md) | Triển khai production, giám sát, xử lý sự cố, bảo mật |

## Đọc nhanh theo vai trò

- **Người dùng mới:** [Getting Started](02-getting-started.md) → [Configuration](03-configuration.md) → [WebUI](07-webui.md).
- **Backend developer:** [Architecture](01-architecture.md) → [Account Management](05-account-management.md) → [Cloud Code & Format](06-cloudcode-and-format.md).
- **Frontend developer:** [WebUI](07-webui.md) → [API Reference](04-api-reference.md).
- **DevOps / SRE:** [Configuration](03-configuration.md) → [Operations](09-operations.md) → [API Reference](04-api-reference.md).
- **Ai cần tìm một file cụ thể:** [Module Map](08-module-map.md).

## Quy ước tên gọi

Thương hiệu công khai của dự án là **Novoid**. Một số định danh kỹ thuật bên trong vẫn giữ tên gốc (`ANTIGRAVITY_ENDPOINT_*`, `antigravity_headers()`, `X-Client-Name: antigravity`, `userAgent: "antigravity"`) vì đó là giá trị mà endpoint Google Cloud Code yêu cầu để tương thích giao thức — không phải tên thương hiệu của dự án. Mọi dữ liệu runtime được lưu tại `~/.config/novoid-core/`.

## Liên kết ngoài

- Repository: <https://github.com/hanovoid/novoid-core>
- Model Router hai tầng & script cài đặt CLI: <https://github.com/hanovoid/novoid-proxy>
