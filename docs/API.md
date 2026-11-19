# `@shinick/sharepoint` Local HTTP JSON API

Run locally on port `8790` (default):

```bash
sharepoint serve --port 8790
# Optional Bearer API key protection:
sharepoint serve --port 8790 --api-key my-secret
```

## Endpoints

- `GET /health` — health status, version, signed-in user ID
- `GET /openapi.yaml` — OpenAPI 3.1 schema
- `GET /v1/whoami` — user profile & OAuth scope diagnostics
- `GET /v1/sites?search=&limit=&offline=` — followed & discovered SharePoint sites
- `GET /v1/drives?site=&offline=` — OneDrive + SharePoint document libraries
- `GET /v1/items?drive=&site=&parent_id=&path=&limit=` — list folder contents
- `GET /v1/items/{id}?drive=&site=` — item metadata
- `GET /v1/items/{id}/children` — list child items
- `GET /v1/items/{id}/content?max_bytes=` — text/code preview
- `GET /v1/items/{id}/versions` — version history
- `POST /v1/items/folder` — create folder (`{ drive?, site?, parent_id?, name }`)
- `POST /v1/items/upload` — upload (`{ drive?, site?, parent_id?, local_path?, file_name?, content_base64?, content_text? }`)
- `PATCH /v1/items/{id}` — rename/move (`{ drive?, site?, name?, parent_id? }`)
- `DELETE /v1/items/{id}?drive=&site=` — delete item
- `POST /v1/items/{id}/share` — create sharing link (`{ type, scope, expires? }`)
- `GET /v1/recent`, `GET /v1/shared` — OneDrive recent & shared items
- `GET /v1/search?q=&drive=&site=&remote=&limit=` — FTS5 + Graph search
- `POST /v1/download` — download file (`{ ok, path, file_name, mime_type, size_bytes, is_image }`)
