# `@bitefoundry/sharepoint` MCP Server

Start the stdio JSON-RPC 2.0 MCP server:

```bash
sharepoint mcp
```

## Tools (19)

1. `whoami`
2. `list_sites`
3. `list_drives`
4. `list_files`
5. `get_file_info`
6. `read_text_file`
7. `download_file` — returns `{ ok, path, file_name, mime_type, size_bytes, is_image, drive_id, item_id, e_tag, c_tag }`
8. `upload_file`
9. `create_folder`
10. `move_or_rename_item`
11. `delete_item` — requires `confirm: true`
12. `search_files`
13. `create_share_link`
14. `list_recent`
15. `list_shared_with_me`
16. `list_versions`
17. `list_items`
18. `list_columns`
19. `update_list_item_field`

## Drives

Without `site`, `list_drives` scans up to 200 followed and discovered SharePoint
sites alongside personal OneDrive, and returns `drives`, `returned`,
`sites_scanned`, `sites_failed`, and `has_more`.

## File reads

`get_file_info` defaults to compact output (`compact: false` returns every item
field) containing `drive_id`, `id`, `name`, `parent_path`, `is_folder`, `size`,
`last_modified_at`, `e_tag`, `omitted`, and `hint`. `list_files` still accepts
`compact: true` (`drive: { "id": "…" }` and core item fields), defaults `limit`
to `100`, and adds `returned`, `has_more`, and `source` (`"remote"` or
`"cache"`). Successful responses carry the same JSON in `structuredContent` and
as minified (single-line) JSON in `content[0].text`.

`read_text_file` decodes UTF-8 with an optional BOM and UTF-16LE or UTF-16BE
with a BOM. It preserves line endings. `max_bytes` defaults to 256 KiB, and
`max_bytes` and `size_bytes` count the original bytes, including any BOM. Binary
files can be saved with `download_file`. Malformed text and files above the
byte limit return `isError: true`.

`list_versions` reads the complete history by default and includes `returned`
and `has_more`. Pass a non-negative integer `limit` to cap results and stop
requesting pages when the cap is reached; `limit: 0` returns `versions: []` and
`has_more: null` without querying Graph. An empty history returns `versions: []`
and `has_more: false`. Graph failures return `isError: true`.

## Conditional replacement

Read the current `e_tag` with `get_file_info`. Pass that value as `if_match`
along with `item_id` and the replacement content to `upload_file`. You can
provide `local_path`, `content_base64`, or `content_text` (UTF-8) with a
`file_name` for inline content. Conditional replacement uses an upload session
at any file size. If the eTag is stale, `upload_file` reports an error.

## SharePoint List rows

All three tools take a SharePoint `site_id` and `list_id`. Call `list_columns`
to find each column's internal `name`, its `display_name`, and its `read_only`
flag. [Graph column discovery](https://learn.microsoft.com/en-us/graph/api/list-list-columns?view=graph-rest-1.0) · [Column properties](https://learn.microsoft.com/en-us/graph/api/resources/columndefinition?view=graph-rest-1.0)

`list_items` reads one page (default `limit` `50`, range `1`–`200`) and returns
`items`, `returned`, `has_more`, and `next_cursor`. Each row includes `id`,
`e_tag` when available, and `fields` (`@odata.etag` is removed from `fields`;
use the row's `e_tag`). `next_cursor` is an opaque token that can only be reused
with the same `site_id`, `list_id`, and `fields` set (order independent), and
cannot be combined with `limit`. A mismatched target, changed field set, invalid
cursor, or combined `limit` returns `isError: true` without sending a Graph
request. Omit `fields` to expand the default Graph field set, or pass an array
such as `["ReviewStatus", "Quantity"]` to select those internal names. Graph's
default expansion can represent lookup fields with `LookupId` keys. [Graph list items](https://learn.microsoft.com/en-us/graph/api/listitem-list?view=graph-rest-1.0) · [Field values and lookup rules](https://learn.microsoft.com/en-us/graph/api/resources/fieldvalueset?view=graph-rest-1.0)

Example arguments for `update_list_item_field`:

```json
{
  "site_id": "contoso.sharepoint.com,site-collection-id,web-id",
  "list_id": "list-id",
  "item_id": "17",
  "field": "Complete",
  "value": false,
  "if_match": "\"row-etag\""
}
```

Pass a JSON value that the column accepts. The tool sends one field key and
returns the requested row ID, field name, Graph's returned field values, and
the new `e_tag` when Graph supplies it. If the response lacks an eTag, read the
row again to obtain its current tag. Graph rejects a stale `if_match` with
`412`. [Graph field update](https://learn.microsoft.com/en-us/graph/api/listitem-update?view=graph-rest-1.0)

Graph documents delegated work or school access for these endpoints. Reads
require `Sites.Read.All` or a supported higher scope, and writes require
`Sites.ReadWrite.All`. The signed-in user also needs access to the target
list. [Read permissions](https://learn.microsoft.com/en-us/graph/api/listitem-list?view=graph-rest-1.0#permissions) · [Write permissions](https://learn.microsoft.com/en-us/graph/api/listitem-update?view=graph-rest-1.0#permissions)
