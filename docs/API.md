# `@bitefoundry/sharepoint` Local HTTP JSON API

Run locally on port `8790` (default):

```bash
# A random Bearer key is generated and printed once to stderr
sharepoint serve --port 8790
# Or choose the key yourself
sharepoint serve --port 8790 --api-key my-secret
```

## Access rules

- **Key.** Every `/v1` route needs `Authorization: Bearer <key>`. Without `--api-key`, the server generates a random key at startup and prints it once to stderr. `--allow-keyless` serves without a key, on a loopback address only.
- **Bind address.** The default is `127.0.0.1`. A non-loopback `--bind` needs `--api-key`.
- **Host header.** Requests must name the server as `localhost`, `127.0.0.1`, `::1` or the bound address. Add a reverse-proxy name with `--allowed-host <name>` (repeatable). Other hosts get `403`, on every route including `/health`.
- **CORS.** Browser origins on a loopback host (any port) are allowed. Add others with `--cors-origin <scheme://host[:port]>` (repeatable, exact match).
- **Local paths.** `local_path` on upload and `dir` on download read and write this machine's files. They work on a server with a key. On a keyless server they return `403` unless it was started with `--allow-local-paths`; send `content_base64` / `content_text`, or omit `dir` to use the configured download directory.

## Cache and cost

`/v1/sites`, `/v1/drives`, `/v1/items` and `/v1/items/{id}/children` ask Microsoft Graph by default. `offline=true` answers from the local cache without a Graph request. `refresh=true` forces the Graph request even when `offline=true` is also set. `/v1/recent` and `/v1/shared` accept `offline=true` too. `/v1/drives` without `site` makes one Graph request per site (up to 200 sites).

## Endpoints

- `GET /health` — health status, version, signed-in user ID
- `GET /openapi.yaml` — OpenAPI 3.1 schema
- `GET /v1/whoami` — user profile & OAuth scope diagnostics
- `GET /v1/sites?search=&limit=&offline=&refresh=` — followed & discovered SharePoint sites
- `GET /v1/drives?site=&offline=&refresh=` — OneDrive + SharePoint document libraries
- `GET /v1/items?drive=&site=&parent_id=&path=&limit=&offline=&refresh=` — list folder contents
- `GET /v1/items/{id}?drive=&site=` — item metadata
- `GET /v1/items/{id}/children?drive=&site=&limit=&offline=&refresh=` — list child items
- `GET /v1/items/{id}/content?max_bytes=` — UTF-8 / BOM UTF-16 text preview (`415` for binary or unsupported encodings; use `POST /v1/download` for raw bytes)
- `GET /v1/items/{id}/versions` — version history
- `POST /v1/items/folder` — create folder (`{ drive?, site?, parent_id?, name }`)
- `POST /v1/items/upload` — upload (`{ drive?, site?, parent_id?, local_path?, file_name?, content_base64?, content_text? }`; `local_path` follows the local-path rule above)
- `PATCH /v1/items/{id}` — rename/move (`{ drive?, site?, name?, parent_id? }`)
- `DELETE /v1/items/{id}?drive=&site=` — delete item
- `POST /v1/items/{id}/share` — create sharing link (`{ type, scope, expires? }`)
- `GET /v1/recent?limit=&offline=`, `GET /v1/shared?limit=&offline=` — OneDrive recent & shared items
- `GET /v1/search?q=&drive=&site=&remote=&limit=` — FTS5 + Graph search
- `POST /v1/download`, `POST /v1/items/{id}/download` — download file (`{ ok, path, file_name, mime_type, size_bytes, is_image }`; `dir` follows the local-path rule above)
