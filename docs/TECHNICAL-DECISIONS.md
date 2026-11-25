# Technical decisions and research

This document describes the app's integration decisions. See the [release guide](RELEASE.md) for build and verification commands.

Researched against official sources on 23 November 2026. The build uses Rust stable 1.100.0, Slint 1.19.0 (current stable), and the installed WinGet 1.29.380. Cargo.lock records exact dependency versions.

## Native portable architecture

Rust compiles the UI and assets into one Windows GUI executable. Slint uses its Winit backend and software renderer; Qt, GPU renderers, interpreter, live preview, system tray, and web backends are disabled. Native Windows title bars, resizing and snapping remain available. Segoe UI is a Windows system font, not a separately installed font.

Slint's build-time `EmbedResourcesKind::EmbedFiles` includes all referenced UI assets. winresource embeds the supplied ICO, product/version data, and an `asInvoker`, PerMonitorV2 manifest. The PNG is the PNG payload extracted without alteration from the supplied ICO; no separate PNG was supplied.

The x64 MSVC target uses `-C target-feature=+crt-static`. Release optimization uses size optimization, LTO, one codegen unit, stripped symbols, and aborting panics. Portability is checked by inspecting PE imports and launching copied and moved executables in folders containing only that EXE; compilation alone is not evidence of portability.

## WinGet: investigated COM, selected CLI

The official COM contract supports structured metadata but requires appropriate activation/identity/permissions. The production executable includes a read-only `CoCreateInstance` diagnostic probe using Microsoft's production PackageManager CLSID. This environment returned `0x80040154` (class not registered) for the unpackaged caller. SlimStore does not register COM classes, acquire package identity, deploy shims, or bundle Windows Package Manager DLLs. The existing CLI is the compatible backend.

WinGet 1.29's actual `search`, `show`, and `list` help does not expose JSON output. `source export` does produce JSON and is parsed structurally. For unsupported CLI layouts, SlimStore fails closed instead of guessing package actions.

The isolated `winget.rs` compatibility adapter:

- Resolves WinGet from the registered Microsoft App Installer package through Windows package APIs. Commands use separate arguments and exact IDs. The community source check verifies the exported endpoint, package identifier, type, and trust flags.
- Reads column positions from each table header, counts Unicode display width, and identifies packages from the ID column. Informational footers stay outside the inventory.
- Rejects uncertain exact matches, truncated IDs, and unsupported layouts. WinGet remains the authority on applicability, version comparisons, and pins.
- Keeps the full `show` output in the user's system language. Details also display recognized description, publisher, license, and HTTPS link fields from that response. The browser opens validated links only when clicked.
- Checks installed state and the exact candidate before review, then rechecks before execution. Install and update commands pass the reviewed version explicitly. A fresh install uses WinGet’s `--no-upgrade` option to protect against changes to inventory during launch. [WinGet install options](https://learn.microsoft.com/en-us/windows/package-manager/winget/install)

The adapter is a compatibility compromise. It is not a documented machine-readable WinGet schema. Future CLI formats, names containing certain unusual ID-like tokens, multiword localized unknown-version strings, and truncated old-client output can require adapter updates. No fallback silently converts an uncertain row into an install command.

### Package backend and recovery plan

The packaged WinGet CLI handles reviewed package changes and reports their result. Microsoft’s `Microsoft.WinGet.Client` PowerShell module was tested with a read-only exact package lookup. It uses Windows Package Manager and requires the PowerShell module on the host. A structured API adapter would first need separate read-only compatibility checks for catalog/source identity and exact installed versions on the supported Windows versions, followed by scoped mutation tests. [Microsoft WinGet documentation](https://learn.microsoft.com/en-us/windows/package-manager/winget/)

`Add-AppxPackage` and the Windows package deployment APIs manage signed MSIX/AppX packages. Windows Installer manages MSI packages. A catalog-wide fallback using these tools would need to reproduce WinGet’s installer matching, package scope, dependency handling, and validation across formats. Retrying a failed write through a second backend can also repeat a partially completed change. [Windows package deployment API](https://learn.microsoft.com/en-us/windows/win32/appxpkg/package-deployment-api) · [Windows Installer and Restart Manager](https://learn.microsoft.com/en-us/windows/win32/msi/using-windows-installer-with-restart-manager)

## Operations and concurrency

One background read worker handles queries. Search waits 380 ms after editing. Generation tokens cancel obsolete read-only WinGet processes and reject stale results. Individual read processes time out after 90 seconds. Startup checks WinGet and refreshes installed status for the initial catalog page.

A separate serial operations worker owns install, upgrade, and uninstall commands. There are at most 200 pending operations, duplicate package operations are rejected, and the last 80 terminal operations are retained with 48 KiB diagnostic tails. The worker sleeps on a condition variable; no WinGet polling occurs when idle. Progress uses the actual CLI output without invented percentages.

Close confirmation stops new work and cancels waiting operations under the worker’s queue lock. An active operation can finish before SlimStore closes. After 45 minutes, the operation log asks the user to check for installer or UAC windows.

Update and uninstall confirmation asks the user to save work and close the affected apps. For registered WinGet portable packages with a verified local package directory, Windows process paths and file-sharing checks can stop an update or removal before launch. Other app formats rely on the publisher installer and WinGet for file-in-use outcomes. SlimStore displays these outcomes in Downloads and requires a fresh review to retry. Installer-busy results hold the queue for an explicit resume. Restart-required results cancel waiting entries for the session, and users can review those changes after restarting Windows. Successful changes are checked against exact inventory. A missing, mismatched, or ambiguous result is shown as **Check installed state**. [WinGet return codes](https://github.com/microsoft/winget-cli/blob/master/doc/windows/package-manager/winget/returnCodes.md)

After operations, cached installed state and details are invalidated; visible package/inventory pages refresh. The read worker is cancelled and joined on normal exit; no SlimStore package-management process is left running. UAC belongs to the publisher's installer/WinGet, never an elevated SlimStore GUI.

## Discovery and categories

The embedded catalog is an editorial selection of exact IDs. Featured Apps, Top Apps (editorial picks), Essential Apps, Community Favorites, Developer Picks, and Creative Essentials are explicitly curated. There is no ratings/download-count provider and no trustworthy feed-wide update timestamp, so factual ratings, download rankings, and "Recently Updated" rankings are omitted. No giant package catalog is fetched.

The category browser can search the corresponding WinGet manifest tags. Search is bounded to 250 results per request. Sorting uses application names or editorial order.

Visible cards try local Scoop executable resources, the opt-in portable icon cache, and checksum-pinned artwork. A miss triggers exact package metadata lookup, which can supply a hash-pinned WinGet icon or identify a publisher or project for dynamic discovery. UniGetUI metadata supplies candidates and the package manifest establishes project identity. SlimStore bounds network and image sizes. [Artwork source and selection](ARTWORK.md)

### Scoop install fallback

Scoop Main and Extras are indexed on first use on a separate worker, using recent local official checkouts when available or bounded official archives read in memory. The compact catalog can be cached beside SlimStore by opting in. Exact package identity includes its bucket. WinGet failures and verified empty searches retain distinct states.

SlimStore uses the existing Scoop PowerShell entry point for supported package installs under a standard-user session. Before review and launch, it rechecks installed state and matches the local manifest to the live official manifest. Scoop verifies the installer hash. The review excludes package-defined scripts and automatic dependency setup. Other results show instructions for Scoop. The app links to the official setup guide and can redetect Scoop in the same session. A failed WinGet change stays in Downloads because the installer may have partially changed the host.

## Storage and privacy

Settings and metadata start in memory. Opt-in settings are saved to `SlimStore-data/settings.json` through a unique temporary file, atomic replacement, and a per-directory lock. The reader checks directory and file types and limits reads to 16 KiB. A linked, oversized, corrupt, or unwritable sidecar falls back to memory.

SlimStore reads Windows' theme/accent registry values; it never changes them. System theme is detected on startup and when System is selected. Theme changes made outside SlimStore during the session may need reselecting System. The optional accent affects SlimStore highlights; Slint's built-in Fluent controls can also use the system accent.

With online icons enabled, visible cards can request package metadata and artwork from WinGet, GitHub, and verified project hosts. SlimStore keeps decoded icons and public metadata in memory by default. The optional cache saves bounded thumbnail and public metadata files beside the executable on close. The **Load screenshots online** action can contact GitHub and verified project hosts while Details is open. Scoop catalog refreshes and install reviews contact GitHub independently of the icon setting. The system WinGet and publisher installers manage their own logs and telemetry. [Microsoft WinGet settings](https://github.com/microsoft/winget-cli/blob/master/doc/Settings.md#telemetry)

## Sources

- [Slint 1.19 Rust crate and features](https://docs.slint.dev/latest/docs/rust/slint/)
- [Slint resource embedding](https://docs.slint.dev/latest/docs/rust/slint_build/)
- [Slint backends and renderers](https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backends_and_renderers/)
- [Rust static CRT linkage](https://doc.rust-lang.org/reference/linkage.html#static-and-dynamic-c-runtimes)
- [Microsoft COM API specification](https://github.com/microsoft/winget-cli/blob/master/doc/specs/%23888%20-%20Com%20Api.md)
- [Production COM CLSIDs](https://github.com/microsoft/winget-cli/blob/master/src/Microsoft.Management.Deployment/Public/ComClsids.h)
- [WinGet search](https://learn.microsoft.com/windows/package-manager/winget/search)
- [WinGet list](https://learn.microsoft.com/windows/package-manager/winget/list)
- [WinGet error codes](https://github.com/microsoft/winget-cli/blob/master/doc/windows/package-manager/winget/returnCodes.md)
- [WinGet settings and telemetry](https://github.com/microsoft/winget-cli/blob/master/doc/Settings.md)
