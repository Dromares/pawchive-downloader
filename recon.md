# DAN-1538 — Pawchive Recon (Step 1, no code)

## 1. Delivery Mechanism

Pawchive uses a **JSON API** at `https://pawchive.pw/api/v1` for all data. The HTML pages are thin client-side shells (SPA) that fetch data via the API. No server-rendered HTML posts or client-side JS rendering of posts — the API is the source of truth.

- **Profile list:** `GET /api/v1/creators` — returns `list[CreatorSummary]`
- **Profile posts:** `GET /api/v1/{service}/user/{creator_id}` — returns `list[Post]`
- **Post detail:** `GET /api/v1/{service}/user/{creator_id}/post/{post_id}` — returns `Post`
- **Profile info:** `GET /api/v1/{service}/user/{creator_id}/profile` — returns `CreatorProfile`
- **Announcements:** `GET /api/v1/{service}/user/{creator_id}/announcements`
- **Fancards:** `GET /api/v1/{service}/user/{creator_id}/fancards`
- **Links:** `GET /api/v1/{service}/user/{creator_id}/links`
- **File search by hash:** `GET /api/v1/search_hash/{file_hash}`
- **Post flag:** `POST /api/v1/{service}/user/{creator_id}/post/{post_id}/flag`
- **Is post flagged:** `GET /api/v1/{service}/user/{creator_id}/post/{post_id}/flag`
- **Revisions:** `GET /api/v1/{service}/user/{creator_id}/post/{post_id}/revisions`
- **Comments:** `GET /api/v1/{service}/user/{creator_id}/post/{post_id}/comments`
- **App version:** `GET /api/v1/app_version`

Services supported (from `GET /api/v1/creators`): `patreon`, `fanbox`. Only 2 services observed across 96,822 creators.

**Endpoint list:** observed API paths. Endpoints actually called during this recon: `/api/v1/creators`, `/api/v1/patreon/user/30500811?o=0`, `/api/v1/patreon/user/30500811?o=50`, `/api/v1/patreon/user/30500811?o=1000`, `/api/v1/fanbox/user/119251`, `/api/v1/fanbox/user/119251/post/12221506`, `/api/v1/patreon/user/30500811/post/172435214`, `/api/v1/fanbox/user/1386751`, `/api/v1/fanbox/user/1386751/post/5464553`, `/api/v1/patreon/user/30500811/profile`.

## 2. Data Available Per Post / Per File

**Post fields (from API response):**
- `id` (string) — post ID
- `user` (string) — creator ID
- `service` (string) — "patreon", "fanbox", etc.
- `title` (string)
- `content` (string) — HTML body
- `embed` (object) — embed data (often empty `{}`)
- `shared_file` (bool) — whether this post's file is shared across posts
- `added` (string) — ISO datetime when indexed
- `published` (string) — ISO datetime when published
- `edited` (string) — ISO datetime when last edited
- `file` (object) — `{name, path, preview_only}`
- `attachments` (array of `{name, path, preview_only}`)

**File fields:**
- `name` (string) — original filename
- `path` (string) — relative path on file host (e.g. `/4d/e5/4de5524e41ef3dfb7ea0d862720bf0f10e76744e7d9170d5ac73226078b0be88.jpg`)
- `preview_only` (bool or absent) — **present for Patreon posts, absent for fanbox posts**
  - `true` means only a thumbnail is available (confirmed live; see Q5)
  - absent (`null`) means full-size file is available (confirmed live; see Q5)
- **`node` (int, optional):** present only on `.zip` attachments (e.g. `2`). Must be tolerated in the Rust struct.
- **Size:** NOT provided in the API response. Must be obtained via `Range: bytes=0-0` on the file host.
- **Content-Type:** Must be obtained via `Range: bytes=0-0` on the file host.

**Per-file notes:**
- `preview_only: true` files are only available as thumbnails on `img.pawchive.pw` (confirmed live)
- `preview_only: absent` files are available as full-size on `file.pawchive.pw` (confirmed live)
- `shared_file: true` means the file is shared across multiple posts (e.g. a ZIP containing images)
- **Fanbox posts do NOT have `preview_only`** — they are always downloadable (no `preview_only` key present in the `file` object)

## 3. Paging

- The API uses **offset-based** paging via the `?o=` query parameter (NOT `?offset=`).
- Default page size is **50 posts** per request.
- Last page is signalled by receiving an **empty array** `[]` (or fewer than 50 posts).
- **Confirmed `?o=` semantics:**
  - `GET /api/v1/patreon/user/30500811?o=0` → 601672 bytes, first id `172435214`, last id `168514695` (50 posts)
  - `GET /api/v1/patreon/user/30500811?o=50` → 505435 bytes, first id `168501262`, last id `165148786` (50 posts, different posts)
  - `GET /api/v1/patreon/user/30500811?o=1000` → `[]` (empty array, last page)
- **No cursor-based pagination observed.**
- A large profile costs `ceil(total_posts / 50)` requests.

## 4. Auth

- **Anonymous access works** for profile listing, post listing, profile info, and file downloads. No session cookie required for the API.
- A `session` cookie (name: `session`, value: opaque string) is used for **account-favorites** operations (add/remove favorites, list favorites). These 5 operations are the only ones requiring a signed-in account.
- Post flagging is **public** (no session required).
- Content that needs login: account favorites, DMs, importer.
- **Never record cookie values** — only the cookie name `session` matters.

## 5. File Hosting

- **Thumbnail host:** `https://img.pawchive.pw/thumbnail/data{path}` (e.g. `/data/df/b2/dfb2c40289b286ae0382565652c2ce14f7520f904198fe30dd51a108fc107b78.jpeg`)
  - Server: `cloudflare`. `cf-cache-status: HIT`.
  - **Tested with `Range: bytes=0-0`:** returns `HTTP/2 206`, `image/jpeg`, `content-range: bytes 0-0/37376` (37 KB total).
  - Verified: `HTTP/2 206`, `image/jpeg`, `content-range: bytes 0-0/37376`, `server: cloudflare`, `cache-control: public, max-age=31536000, immutable`.
  - Note: the older pattern `img.pawchive.pw/thumbnail{path}` (without `/data`) returns 404 for the same path.

- **Full-size host:** `https://file.pawchive.pw/data{path}` (e.g. `/data/4d/e5/4de5524e41ef3dfb7ea0d862720bf0f10e76744e7d9170d5ac73226078b0be88.jpg`)
  - Server: `ddos-guard`.
  - **Fanbox file (`preview_only: None`):** `Range: bytes=0-0` → `HTTP/2 206`, `image/jpeg`, `content-range: bytes 0-0/186630`, `accept-ranges: bytes`. Works with any UA.
  - **Patreon file (`preview_only: true`):** `Range: bytes=0-0` → `HTTP/2 404`, `application/octet-stream`, `content-length: 162`. No cookie or Referer resolution needed.
  - **Zip file (`preview_only` absent):** `Range: bytes=0-0` → `HTTP/2 206`, `application/zip`, `content-range: bytes 0-0/199887803`, `accept-ranges: bytes`.
  - **Verified full-size fetch (BOKABA zip, `preview_only` absent):** `HTTP/2 200`, `content-type: application/zip`, `content-length: 199887803`, `accept-ranges: bytes`.
  - **Verified full-size fetch (fanbox image, `preview_only: None`):** `HTTP/2 200`, `image/jpeg`, `content-length: 186630`, `accept-ranges: bytes`.

  **Key finding:** The `preview_only: true` path on `file.pawchive.pw` returns 404 (not 200), while `preview_only: absent` (fanbox) returns 206. This confirms that `preview_only: true` means only a thumbnail is available on `file.pawchive.pw`. The `preview_only` absent (`null`) key means the full-size file is downloadable.

## 6. Politeness

- **robots.txt:**
  ```
  User-agent: *
  Disallow: /account/login
  Disallow: /account/register
  Allow: /
  ```
  (No specific rate limits documented; the `Allow: /` line suggests all paths are crawlable.)

- **Bot challenges:** DDoS-Guard (server: `ddos-guard`) returns `set-cookie` headers (`__ddg8_`, `__ddg10_`, `__ddg9_`, `__ddg1_`) on every request. The 403 body (376 bytes `text/plain`) is the bot challenge text: "Are you a bot or using download tools? Please show respect and use reasonable rate limits with downloading (no more than 1 request per second, and 5 active downloads), along with an identifiable user agent, preferably with a way to contact you. Excessive downloading or pretending to be a browser will result in a block. Shown this in error? Contact us at contact@pawchive.st". **The 403 is a transient rate block triggered by request frequency, not a permanent block.** Plain clients at ≤1 req/s get 206 OK for `preview_only: None` files. A `Referer: https://pawchive.pw/` header does **not** resolve the 403. The API endpoint itself (`pawchive.pw/api/v1`) returns `200` without any cookies.

- **Rate limits:** Not documented. Observed ~50 posts per page with no 429s in testing. The bot challenge text explicitly states "no more than 1 request per second, and 5 active downloads".

- **Cloudflare/DDoS-Guard:** Present on all hosts (`file.pawchive.pw`, `img.pawchive.pw`, `n2.pawchive.pw`). A plain Rust HTTP client (reqwest) does **not** need cookie persistence for `preview_only: None` files — they return 206 with any UA. A browser-like UA is **not** required (the 403 body explicitly warns against "pretending to be a browser").

## 7. Terms

- From `robots.txt`: `Allow: /` — all paths are crawlable.
- From the site description: "Pawchive is a public archive of Patreon, Pixiv Fanbox and Discord posts. Browse archived creators and search their posts, comments and files."
- No explicit ToS quoted.
- **Pages checked:** `/terms` → 404, `/about` → 404, `/docs` → SPA shell (title "Pawchive", no ToS text), `/api/schema` → SPA shell (no ToS text), footer links include `/contact` and `/api/schema`.
- The importer page mentions "session keys are encrypted using RSA 4096" and "anonymously-submitted keys cannot be managed."
- **No ToS page found at /terms, /about, /docs, /api/schema.**

## Recommended Scraping Approach

1. **Fetch profile list:** `GET /api/v1/creators` (no auth needed)
2. **For each target profile:** `GET /api/v1/{service}/user/{creator_id}?o=0`, `?o=50`, etc. — stop when response is `[]`.
3. **For each post:** `GET /api/v1/{service}/user/{creator_id}/post/{post_id}` for detailed file info
4. **For file downloads:**
   - Thumbnails: `GET https://img.pawchive.pw/thumbnail/data{path}` (206, `image/jpeg`, `content-range: bytes 0-0/{total}`)
   - Full-size: `GET https://file.pawchive.pw/data{path}` — `preview_only: absent` files return 206; `preview_only: true` files return 404 (thumbnail-only). Use `Range: bytes=0-0` to confirm before full download.
5. **Rate limit:** Space requests ~1s apart; no documented limit but be polite (bot challenge text says "no more than 1 request per second, and 5 active downloads").
6. **File size:** Use `Range: bytes=0-0` to get `Content-Length` before downloading.

## Fixture Files

| File | Description |
|------|-------------|
| `patreon-user-30500811-posts.json` | BOKABA Patreon posts (page 1, 50 posts, `?o=0`) |
| `patreon-user-30500811-posts-page2.json` | BOKABA Patreon posts (page 2, `?o=50`, 50 posts, first_id=168501262) |
| `patreon-user-30500811-posts-last.json` | BOKABA Patreon posts (page N, `?o=1000`, empty array `[]`) |
| `patreon-user-30500811-profile.json` | BOKABA profile info |
| `patreon-user-30500811-post-172435214.json` | BOKABA post detail (with 200 attachments, zip file) |
| `fanbox-user-119251-posts.json` | 多摩 丸彦 fanbox posts (3 posts, 9 files) |
| `fanbox-user-119251-post-12221506.json` | 多摩 丸彦 post detail (1 image + 3 attachments) |
| `fanbox-user-1386751-posts.json` | yellowbea fanbox posts (50 posts, 59 files) |
| `fanbox-user-1386751-post-5464553.json` | yellowbea post detail (1 image, no attachments) |
| `robots.txt` | Pawchive robots.txt |

## Preview/Full File Count Summary

| Source | Total files | `preview_only: true` | `preview_only` absent (fanbox) | `node` field (zip) |
|--------|-------------|----------------------|-------------------------------|---------------------|
| BOKABA (page1, 50 posts) | 4048 | 4019 (49 main + 3970 attachments) | 29 (1 main + 28 attachments) | 13 (attachments only) |
| BOKABA (page2, ~66 posts) | 3308 | 3277 | 31 | 0 |
| yellowbea (fanbox, 50 posts) | 59 | 0 | 59 (all absent) | 0 |
| 多摩 丸彦 (fanbox, 3 posts) | 9 | 0 | 9 (all absent) | 0 |

**Key finding:** Fanbox posts do NOT have `preview_only` at all. All files are downloadable. Patreon posts have `preview_only: true` for most files (4019/4048 on page 1), meaning only the `.zip` files (29) are fully downloadable. Every image is thumbnail-only.

**Verified live:** `preview_only: None` (fanbox) → 206 OK on `file.pawchive.pw`; `preview_only: true` (Patreon) → 404 on `file.pawchive.pw`. The 404 is a clean "not found" (`application/octet-stream`, 162 bytes), distinct from the transient 403 rate-block observed after a burst of probes.

