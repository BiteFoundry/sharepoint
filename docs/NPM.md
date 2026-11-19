# `@bitefoundry/sharepoint` npm Packaging

`@bitefoundry/sharepoint` ships a single prebuilt native Rust binary across 6 optional platform packages (`win32-x64`, `win32-arm64`, `darwin-x64`, `darwin-arm64`, `linux-x64`, `linux-arm64`) with 4 launchers:

- `sharepoint` / `sp`: SharePoint Sites & Document Libraries persona
- `onedrive` / `od`: Personal OneDrive persona (`SHAREPOINT_PERSONA=onedrive` + `argv[0]` detection)

## Zero-Node-Process Linking
`scripts/postinstall.mjs` hardlinks the platform binary directly to `bin/{sharepoint,sp,onedrive,od}` so invocations run with 0ms Node.js overhead while preserving persona detection via `argv[0]`.

## Release Verification
```bash
node scripts/sync-version.mjs --check
node scripts/verify-glibc.mjs --self-test
```
