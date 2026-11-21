# `@bitefoundry/sharepoint` npm Packaging

`@bitefoundry/sharepoint` ships one native Rust binary per supported platform (`win32-x64`, `win32-arm64`, `darwin-x64`, `darwin-arm64`, `linux-x64`, `linux-arm64`). A single install provides two commands.

- `sharepoint` opens SharePoint sites and document libraries.
- `bitefoundry-onedrive` opens personal OneDrive.

## Install

```sh
npm install -g @bitefoundry/sharepoint
sharepoint --version
bitefoundry-onedrive --version
```

Run `cargo install --locked --path .` from a source checkout to install both native commands. For a standalone install, extract both executables from the platform archive on the [GitHub releases page](https://github.com/BiteFoundry/sharepoint/releases) and add their directory to `PATH`. Windows adds `.exe` to both names.

## Upgrade scripts and MCP clients

Update SharePoint scripts that used `sp` and OneDrive scripts that used `od` or `onedrive`.

```sh
# Previous invocation
sp sites --json
od ls /Documents --json
onedrive recent --json

# Current invocation
sharepoint sites --json
bitefoundry-onedrive ls /Documents --json
bitefoundry-onedrive recent --json
```

In a OneDrive MCP configuration, change `{"command":"onedrive","args":["mcp"]}` or `{"command":"od","args":["mcp"]}` to `{"command":"bitefoundry-onedrive","args":["mcp"]}`. For SharePoint, change `{"command":"sp","args":["mcp"]}` to `{"command":"sharepoint","args":["mcp"]}`.

If you installed the PowerShell profile launchers, rerun `scripts/install-profile.ps1` and restart the shell. npm removes the previous package's global launchers during an upgrade. If you manually replaced one of its launcher files with another program's launcher, move that file to a separate `PATH` directory before upgrading.

For a standalone upgrade, `sharepoint update` and the previous `onedrive update` can replace their current executables in place. To adopt the new OneDrive command name, copy `bitefoundry-onedrive` from the release archive into your `PATH`.

## Native launchers

On Unix, `scripts/postinstall.mjs` links the platform binary behind both npm commands. On Windows it links or copies the executable and updates the package's npm shims. The native executable selects the mode from its launcher name. When native linking is unavailable, the JavaScript launchers resolve the installed platform package and set the mode before starting the executable.

For interactive sign-in and delegated Microsoft Graph scopes, see [Company-managed Microsoft Entra sign-in](../README.md#company-managed-microsoft-entra-sign-in).

## Release Verification
```bash
node scripts/sync-version.mjs --check
node scripts/verify-glibc.mjs --self-test
```
