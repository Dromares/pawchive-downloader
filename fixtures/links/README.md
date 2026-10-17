# Fixtures captured for DAN-1609 (external links in post content)

Captured 2026-10-17 from the live sites by DAN-1610. The post is https://pawchive.pw/patreon/user/5163822/post/162546844
("Super Girl" by Melowh). Everything is public. Stripped: cookies, response headers other than the whitelist in
`http-exchanges.txt`, CSP nonces, the signed Mediafire download token (`REDACTED_SIGNED_TOKEN`), the Drive share owner's
e-mail, display name and numeric user ids, and the Drive virus-scan `uuid`, and the (public, client-side) Google API keys embedded in the Drive page.

| file | what it is |
|---|---|
| `post-162546844.api.json` | `GET /api/v1/patreon/user/5163822/post/162546844` (flat object, `content` holds the two links) |
| `creator-5163822.list-o0.api.json` | `GET /api/v1/patreon/user/5163822?o=0`: the *listing* also carries `content` for every post |
| `post-162546844.page.html` | the post's HTML page on pawchive.pw |
| `mediafire-uikej85vz8b2h7z.file-page.html` | Mediafire file page; direct URL is the `href` of `<a id="downloadButton">` |
| `mediafire-deleted-key.file-page.html` | negative control: Mediafire page for a key that does not exist (302 to `error.php?errno=320`, then 404) |
| `drive-folder-14tXDG5k.html` | public Drive folder page; its listing is the `_DRIVE_ivd` JS string |
| `drive-folder-14tXDG5k.ivd.json`, `drive-subfolder-*.ivd.json` | the `_DRIVE_ivd` string decoded (`\xNN` and `\/` unescaped) = JSON; row = `[id, [parent], name, mime, ..., size at [13]]` |
| `drive-folder-404.html` | negative control: Drive folder id that does not exist (HTTP 404) |
| `drive-virus-scan-confirm.html` | the "Google Drive can't scan this file for viruses" page. No file in the example folder triggers it (the largest is 89,567,214 B, under Drive's ~100 MB scan limit), so this is the public CelebA `img_align_celeba.zip` (1.3 GB), file id `0B7EVK8r0v71pZjFTYXZWM3FlRnM` |
| `http-exchanges.txt` | status chains and whitelisted headers for every request above, including the Drive single-file 303 -> 206 and the Mediafire direct 206 |
