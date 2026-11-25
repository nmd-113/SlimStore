# SlimStore

SlimStore is a portable Windows app browser and package manager written in Rust and Slint. It uses the system's WinGet installation and can use Scoop for supported installs.

SlimStore is an independent project made by NaeTech.

## Download and run

Download **SlimStore.exe** from [Releases](https://github.com/nmd-113/SlimStore/releases). The current release is **1.0.1** and Windows displays **1.0.1.0**. Run the EXE directly, rename it, or copy it to another folder or drive. Keep the optional `SlimStore-data` folder alongside the EXE to carry saved settings and caches. The release includes the matching source archive and SHA-256 checksums. [Changelog](CHANGELOG.md)

SlimStore requires Windows 10/11 x64 and uses the registered Microsoft App Installer package with the default Microsoft WinGet community source. Validation used WinGet 1.29.380. SlimStore also starts without WinGet and provides Microsoft’s setup and repair link in Settings.

## Features

- Unified header with native Windows drag, resize, maximize, restore, and caption hit testing, plus Grid and List views over virtualized package results.
- Light, dark, and system themes, a theme-aware border on Windows 10, and a scrollbar gutter that only takes space when browsing results overflow.
- 36 picks on Home and 136 curated packages resolved by exact ID against WinGet, with 14 subject categories and editorial collections in Discover.
- Debounced WinGet search with embedded matches while WinGet responds. A confirmed empty result searches Scoop Main and Extras. **Also search Scoop** is available alongside WinGet results or when WinGet is unavailable.
- Details reuse descriptions already loaded for search cards and display WinGet metadata as it arrives, including the publisher, license, links, and catalog version. View the installation state and expand the full WinGet output when needed.
- Grid and list results show curated or Scoop descriptions where available. With online artwork enabled, cards can also use the exact WinGet package manifest. Opening Details makes its live description available to the same package in the current session.
- Visible results resolve icons on demand from local Scoop apps, checksum-pinned indexes, exact WinGet manifests and verified publisher or project sources. Details can look up screenshots from verified project sources on request, including for live search results. [Artwork and attribution](docs/ARTWORK.md)
- Select apps across Home, Discover, Categories, and live search. **Review selection** opens **Install Queue**, where you can remove apps or inspect Details. **Install selected** verifies the remaining choices and opens one confirmation.
- Grid and List show installation status and version. The catalog includes offline version snapshots, and the system WinGet inventory supplies installed versions.
- Select multiple entries in Installed Apps, then choose **Uninstall selected** for a verified group review. Updates also support selected or all eligible upgrades.
- Serial operation queue with exact-version review, preflight and post-operation checks, duplicate protection, and bounded session history. Downloads offers a fresh review after file locks clear and can pause waiting work while another installer runs.
- **Install Scoop** opens the official setup guide after explaining the per-user installation. **Check for Scoop** detects it without restarting SlimStore.
- Click an eligible card or row to select it, or use its checkbox. The Details button opens the package overview.
- Settings, icons, and catalog indexes stay in memory by default. Portable storage options are in Settings.

Package actions change the host system even though SlimStore itself is portable. WinGet and publisher installers may request UAC, require a restart, use system storage, and manage their own logs/settings. Review the publisher's agreements before confirming a change.

## Portable architecture

The SlimStore logo, Slint UI, curated metadata, and small checksum-pinned artwork indexes are embedded at compile time. Package icons, screenshots, and Scoop bucket data load on demand. Slint uses Winit and its software renderer, and the MSVC CRT is statically linked. The app can run from a read-only folder.

**Save settings locally** stores settings in `SlimStore-data/settings.json` beside `SlimStore.exe`. With saved settings enabled, **Cache app icon and data locally** saves newly loaded icons and public package details on close in `icons-v1.bin` and `app-data-v1.bin` in the same folder. SlimStore can reuse these for matching packages with online artwork fetching disabled. Installation state and package actions use live checks. The separate Scoop cache option stores compact Main and Extras indexes. Unavailable storage falls back to memory. Activity stays in memory. Moving the EXE by itself starts a fresh session.

### Scoop fallback

Main and Extras are searchable before Scoop is installed. The index loads on first use and updates in a background worker. Use **Refresh** on Scoop results to request fresh catalog data. Settings can also include additional buckets already configured in Scoop.

In-app Scoop installation requires a standard-user session, an existing official Main or Extras bucket, and a local manifest that matches the live official manifest over HTTPS. Review lists the bucket, package, version and download URL. SlimStore rechecks the manifest and installed state at launch, and Scoop verifies the payload hash. SlimStore accepts compatible x64 archives or executables with SHA-256 or SHA-512 and hands off packages with unsupported install steps using **Scoop instructions**. Scoop manages its own shims, cache, apps, updates and removals. [Scoop install options](https://github.com/ScoopInstaller/Scoop/blob/master/libexec/scoop-install.ps1)

See [technical decisions](docs/TECHNICAL-DECISIONS.md) for the integration design.

## Build from source

Install a current stable Rust MSVC x64 toolchain (Rust 1.95 or newer), Visual Studio C++ build tools, and the Windows SDK. These are development dependencies only. The workspace also supports a local toolchain under `.tools/` without modifying the system PATH.

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\build.ps1
powershell -NoProfile -ExecutionPolicy Bypass -File .\scripts\verify.ps1 -Live
```

Equivalent Cargo commands:

```powershell
cargo test --locked
cargo test --locked live_ -- --ignored --nocapture  # live read-only package and artwork checks
cargo build --release --locked
```

The release compiler output is `target/x86_64-pc-windows-msvc/release/SlimStore.exe`. The build script copies that file into `dist/` and accepts the optional `SlimStore-data` folder there for local use. Share the EXE itself as the portable release. `Cargo.lock` pins dependencies. `.cargo/config.toml` selects x64 MSVC and the static CRT; the release profile enables LTO and strips debug information.

To sign a distribution build, make Windows SDK `signtool.exe` available in the developer shell. Pass `-SigningThumbprint` with the publisher’s code-signing certificate from the current user certificate store and `-TimestampUrl` with its RFC 3161 endpoint. The build verifies the staged signature before copying it into `dist/` and prints the final SHA-256 hash.

The PowerShell execution-policy option above applies only to that development process; it does not change the machine's policy.

Run `scripts/package.ps1` after building to generate a matching source ZIP and checksums. See the [release guide](docs/RELEASE.md) for GitHub publishing.

## Project layout

```text
src/main.rs          GUI entry point
src/app.rs           UI state, navigation, search workers and callbacks
src/winget.rs        command construction, bounded process runner and CLI adapter
src/scoop.rs         verified Scoop mappings, manifest review and per-user install
src/scoop_catalog.rs bounded Scoop bucket index and search
src/operations.rs    serial queue, cancellation, diagnostic history
src/packages.rs      package identities, matching and eligible update state
src/metadata.rs      embedded collections, categories and text normalization
src/icons.rs         icon loading, local Scoop artwork, and in-memory caching
src/icon_cache.rs    optional portable icon and package metadata caches
src/icon_discovery.rs exact package metadata and project artwork discovery
src/screenshots.rs   on-demand screenshots from checksum-pinned sources
src/http.rs          bounded WinHTTP reads and SHA-256 checking
src/storage.rs       portable settings and cache file storage
src/platform.rs      Windows theme, links, COM probe and file replacement
src/chrome.rs        Native Win32 caption, drag and resize handling
src/errors.rs        WinGet error messages
src/smoke.rs         explicitly enabled read-only GUI verification
ui/                  Slint shell, page layouts, controls, tokens, and artwork mapping
catalog/             Exact catalog snapshot, editorial input, and artwork notices
assets/              supplied branding and embedded executable manifest
scripts/             build, packaging, release verification and window capture
docs/                release guide, technical decisions and license notices
dist/SlimStore.exe     single-file portable release
release/             generated source ZIP and checksums
```

## Limitations

- WinGet manages the community `winget` source. Scoop search covers Main, Extras, and opted-in local buckets. In-app Scoop installs cover verified x64 packages from Main and Extras. Configured WinGet sources are shown in Settings.
- WinGet exposes search and inventory as tables on this client. The adapter reads the displayed columns and rejects unsupported layouts. Details retain WinGet’s localized output. Future formats may require adapter updates.
- Details fill gaps in curated descriptions and labels from recognized live WinGet fields and prefer safe HTTPS links from the current response. Expand WinGet output for the full localized metadata. Packages without a trustworthy icon use initials.
- Live search and tag queries are limited to 250 matches per request. Use category, name, or ID to narrow broad queries. Unmapped packages appear under Other.
- Ratings, download statistics, and recent-update rankings are omitted because there is no verified provider for comparable data.
- Queued operations can be canceled. Closing also cancels pending verification. SlimStore lets a running installer finish and displays a warning after 45 minutes.
- Close the affected apps before updating or removing them. SlimStore checks files for registered WinGet portable packages when it can verify the local package directory. Other installer types report locks through WinGet. Restart outcomes clear waiting changes for the session so they can be reviewed again after restarting.
- System theme is read on launch and when System is selected. External theme changes may require reselecting System. Minimum window size is 820 × 600 logical pixels.
- Physical USB drives, Windows 11 and separate clean machines, real elevation/reboot paths, and actual install/upgrade/uninstall changes require manual validation. Automated validation never changes installed applications. The development host runs Windows 10 IoT Enterprise LTSC, build 19044.

## Privacy and licensing

SlimStore has no analytics or telemetry client. **Load app icons and descriptions online** allows visible cards to request exact package metadata and artwork from WinGet, GitHub, and verified project hosts. Turn it off to skip these background requests for cards. **Load screenshots online** can contact GitHub and verified project hosts when clicked. An on-demand Scoop catalog refresh requests bucket data from GitHub and searches locally. System WinGet controls its own telemetry. [Microsoft WinGet settings](https://github.com/microsoft/winget-cli/blob/master/doc/Settings.md#telemetry)

SlimStore source is licensed under [GPL-3.0-only](LICENSE). Slint is used under its GPL-3.0-only option. Dependency names, versions, license declarations, and upstream source links are listed in [third-party notices](docs/THIRD-PARTY.md). Supplied SlimStore artwork is retained as provided; no additional rights are asserted over that artwork. Package names and trademarks belong to their respective owners. No proprietary store assets or fabricated app statistics are included.

Redistributing the EXE under the selected GPL license also requires making the corresponding source and applicable license notices available. Those development/source documents are not runtime dependencies.
