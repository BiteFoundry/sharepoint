# `@bitefoundry/sharepoint` MCP Server

Start the stdio JSON-RPC 2.0 MCP server:

```bash
sharepoint mcp
```

## Tools (16)

1. `whoami`
2. `list_sites`
3. `list_drives`
4. `list_files`
5. `get_file_info`
6. `read_text_file`
7. `download_file` — returns `{ ok, path, file_name, mime_type, size_bytes, is_image }`
8. `upload_file`
9. `create_folder`
10. `move_or_rename_item`
11. `delete_item` — requires `confirm: true`
12. `search_files`
13. `create_share_link`
14. `list_recent`
15. `list_shared_with_me`
16. `list_versions`
