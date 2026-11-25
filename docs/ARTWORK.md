# Catalog artwork and attribution

SlimStore identifies package artwork by the canonical WinGet ID or the Scoop bucket and package name. Names and marks belong to their respective owners.

SlimStore is made by NaeTech. The SlimStore icon was created for this project and is included as SVG, PNG, and Windows executable icon assets.

## Package icons

An installed Scoop package can supply its Windows icon when SlimStore verifies the bucket and package and finds a local executable inside its installation. SlimStore reads the icon resource without running the executable. The [Scoop mapping](../catalog/scoop.txt) supplies preferred executable paths and matches for some WinGet IDs.

With **Load app icons and descriptions online** enabled, SlimStore first tries the checksum-pinned icon indexes for visible cards. Missing icons trigger a lookup using the exact WinGet package and version or an official Scoop manifest. SlimStore can fetch WinGet's `en-US` file directly when it is marked as the default locale and matches the exact package and version. For other default locales, it follows the version manifest. Cards display description text as soon as their manifest supplies it, while icon discovery can continue. SlimStore verifies hashes for WinGet CDN icons. When a WinGet homepage is a regular website, SlimStore also checks the publisher, support, and release notes fields for a repository tied to the app. It skips repository fallback when those fields identify conflicting repositories. The UniGetUI database and common icon paths supply candidates only within the identified repository or known dedicated publisher domain. Project artwork fetched this way has source validation but may change without a predeclared checksum.

- [WinGet links](../catalog/online-icons.txt) use SHA-256 digests documented in WinGet community manifests.
- [Publisher links](../catalog/publisher-icons.txt) use official sites and source repositories for exact package IDs, including packages outside the curated catalog. NanaZip artwork is by Shomnipotence and Czkawka artwork is by jannuary. [NanaZip attribution](https://github.com/M2Team/NanaZip/blob/main/License.md) [Czkawka attribution](https://github.com/qarmin/czkawka/blob/master/czkawka_gui/README.md)
- [Reviewed UniGetUI links](../catalog/uniget-icons.txt) preserve the source database key alongside SlimStore's exact package ID. UniGetUI was created by Martí Climent and is maintained by Devolutions. The upstream MIT notice accompanies the index. Linked artwork remains subject to its owner's terms. [Source database](https://github.com/Devolutions/UniGetUI/blob/ce8ef7b5ded0341f72c4d806fb4d0bd483dbc866/WebBasedData/screenshot-database-v2.json) [MIT notice](../catalog/UNIGETUI-LICENSE.txt)

Two background workers start queued descriptions in request order before continuing icon probes. A description and icon for the same card share the manifest already fetched. The workers discard queued artwork for the previous page or search and prioritize recent icon requests when scrolling. They fetch over HTTPS with redirects, cookies and automatic authentication disabled. Image responses are limited to 512 KiB, with dimensions capped at 256 pixels per side for ICO and 512 for PNG. SlimStore keeps icons and descriptions in memory by default. A package with no available verified icon shows its initials.

**Cache app icon and data locally** saves up to 192 thumbnails in `SlimStore-data/icons-v1.bin` (under 4 MiB) and 128 packages’ public metadata in `SlimStore-data/app-data-v1.bin` (under 2 MiB) on close. Metadata includes bounded description, publisher and license text, and validated HTTPS links. SlimStore matches metadata by source, package ID, and exact version. Pinned icons can also match by asset checksums. Entries expire after 14 days, and cached content is available with online artwork fetching disabled. Installation status and operations use live verification. Turning off the cache removes both files.

For a new icon, resolve the exact package ID, confirm the artwork matches that product, record the direct HTTPS asset URL and SHA-256, then run the live artwork test.

## Screenshots

**Load screenshots online** appears in Details for WinGet community packages and supported Scoop Main and Extras packages, including live search results. SlimStore tries the [reviewed screenshot index](../catalog/screenshots.txt) first. Those images come from commit-pinned project URLs and must match their SHA-256 checksums.

For other packages, SlimStore reads the exact WinGet manifest or official Scoop manifest to identify the app’s project. It checks the UniGetUI index for image links and can look for screenshots in a project repository’s README or standard paths. It only fetches candidates from that same repository or a verified dedicated publisher host. Dynamically found images have source validation. They may change independently of the package version.

Each request shows up to two PNG or JPEG images, limited to 2 MiB and 2,560 pixels per side. Resized images stay in memory while Details is open. Some projects have no screenshot in a supported source or format.

The PowerToys gallery shows screenshots from Microsoft Learn, by Microsoft and the documentation contributors. The repository's [CC BY 4.0 license](../catalog/artwork-source/Microsoft-Learn-LICENSE) applies to these documentation images.

- [FancyZones screenshot](https://github.com/MicrosoftDocs/windows-dev-docs/blob/a3812316d9d6678be4e526ffb4b1c4f3958d956d/hub/powertoys/images/fancyzones/fancy-zones2.png)
- [Command Palette settings screenshot](https://github.com/MicrosoftDocs/windows-dev-docs/blob/a3812316d9d6678be4e526ffb4b1c4f3958d956d/hub/powertoys/images/command-palette/cmdpal-settings.png)
- [HandBrake 1.8 encoding queue](https://github.com/HandBrake/HandBrake-docs/blob/3553ecc22468475017614d6e0e79e3e095f7c41c/source/docs/en/images/windows/queue-1.8.0.png) and [audio defaults](https://github.com/HandBrake/HandBrake-docs/blob/3553ecc22468475017614d6e0e79e3e095f7c41c/source/docs/en/images/windows/audio-defaults-win-1.8.0.png) by the HandBrake Team and documentation contributors. [CC BY-SA 4.0](../catalog/artwork-source/HandBrake-Docs-LICENSE)
- [Windows Terminal split panes](https://github.com/microsoft/terminal/blob/cf5e6bf22727a251aced8bc30491c87428a4e116/doc/images/panes.png) by Microsoft. [MIT](../catalog/artwork-source/Microsoft-Terminal-LICENSE)
- [Visual Studio Code editor and sidebar](https://github.com/microsoft/vscode-docs/blob/3d196e838cf2a85ced6b8ac679319de42954c576/docs/editing/images/userinterface/hero.png) by Microsoft. [CC BY 3.0 US](../catalog/artwork-source/VSCode-Docs-LICENSE.md)
