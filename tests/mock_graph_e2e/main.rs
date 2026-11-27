//! End-to-end tests against a local mock Microsoft Graph server (`axum`) covering:
//! - Personal OneDrive (`/me/drive`, `/recent`, `/sharedWithMe`)
//! - SharePoint Sites (`/me/followedSites`, `/sites?search=*`, `/sites/{id}/drives`)
//! - Folder children listing with `@odata.nextLink` pagination + path-based addressing
//! - Simple upload (`< 4 MiB`) and resumable chunked upload session (`createUploadSession` + multi-chunk `Content-Range`)
//! - Download (`/content` 302 redirect with explicit Authorization header stripping!)
//! - mkdir, rename/move, delete, versions, share links
//! - SQLite WAL + FTS5 search including Korean `<3` char `LIKE` fallback
//! - HTTP API (`serve::build_router`, port 8790 contract)
//! - MCP Server (`mcp::handle_rpc_line`, all 16 tools + `delete_item` `confirm: true` guard)

mod content_and_tree;
mod core_api;
mod navigation_regressions;
mod server;
mod sync_and_cache;
mod upload_regressions;
