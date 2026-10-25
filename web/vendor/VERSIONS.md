# Vendored UI libraries (served from the embedded binary; the app works offline)

| File | Upstream | Version | sha256 |
|---|---|---|---|
| alpine-3.17.5.min.js | https://cdn.jsdelivr.net/npm/alpinejs@3.17.5/dist/cdn.min.js | 3.17.5 | b4036e9e9ac22fa6f6ae1ba292e015e1f3e9c85d0d79e73e4c8eb9bb139b522e |
| wavesurfer-7.12.12.min.js | https://cdn.jsdelivr.net/npm/wavesurfer.js@7.12.12/dist/wavesurfer.min.js | 7.12.12 | cb9fff59e7ce18932fab801771589ca48bc0362db82218d86cf05f5b60f0bc5e |
| tailwind-play-3.4.17.js | https://cdn.tailwindcss.com/3.4.17 | 3.4.17 | 176e894661aa9cdc9a5cba6c720044cbbf7b8bd80d1c9a142a7c24b1b6c50d15 |

Upgrade by downloading a pinned version, updating this table, and changing the script tags in web/index.html.
Tailwind's play script compiles classes in the browser at runtime; replace it with a precompiled stylesheet later if startup cost matters.
