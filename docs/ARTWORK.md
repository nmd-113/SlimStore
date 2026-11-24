# Catalog artwork and attribution

SlimStore identifies package artwork by the canonical WinGet ID or the Scoop bucket and package name. Names and marks belong to their respective owners.

SlimStore is made by NaeTech. The SlimStore icon was created for this project and is included as SVG, PNG, and Windows executable icon assets.

## Package icons

An installed Scoop package can supply its Windows icon when SlimStore verifies the bucket and package and finds a local executable inside its installation. SlimStore reads the icon resource without running the executable. The [Scoop mapping](../catalog/scoop.txt) supplies preferred executable paths and matches for some WinGet IDs.

With **Load verified app icons online** enabled, SlimStore first tries the checksum-pinned indexes for visible cards. Missing icons trigger a lookup using the exact WinGet package and version or an official Scoop manifest. SlimStore verifies hashes for WinGet CDN icons. It uses the UniGetUI database for candidates and accepts a link only when it matches the project repository or known dedicated publisher domain identified by the package manifest. Project artwork fetched this way has source validation but may change without a predeclared checksum.

- [WinGet links](../catalog/online-icons.txt) use SHA-256 digests documented in WinGet community manifests.
- [Publisher links](../catalog/publisher-icons.txt) use official sites and source repositories. NanaZip artwork is by Shomnipotence and Czkawka artwork is by jannuary. [NanaZip attribution](https://github.com/M2Team/NanaZip/blob/main/License.md) [Czkawka attribution](https://github.com/qarmin/czkawka/blob/master/czkawka_gui/README.md)
- [Reviewed UniGetUI links](../catalog/uniget-icons.txt) preserve the source database key alongside SlimStore's exact package ID. UniGetUI was created by Martí Climent and is maintained by Devolutions. The upstream MIT notice accompanies the index. Linked artwork remains subject to its owner's terms. [Source database](https://github.com/Devolutions/UniGetUI/blob/ce8ef7b5ded0341f72c4d806fb4d0bd483dbc866/WebBasedData/screenshot-database-v2.json) [MIT notice](../catalog/UNIGETUI-LICENSE.txt)

Two background workers fetch over HTTPS with redirects, cookies and automatic authentication disabled. Image responses are limited to 512 KiB and 256 pixels per side. SlimStore accepts ICO and PNG, and keeps images in memory for the session. The pinned indexes cover 133 of the 136 curated packages. A package with no available verified icon shows its initials.

For a new icon, resolve the exact package ID, confirm the artwork matches that product, record the direct HTTPS asset URL and SHA-256, then run the live artwork test.

## Screenshots

The PowerToys gallery contains two screenshots from Microsoft Learn, by Microsoft and the documentation contributors. The repository's [CC BY 4.0 license](../catalog/artwork-source/Microsoft-Learn-LICENSE) applies to these documentation images. SlimStore includes resized preview and thumbnail copies.

- [FancyZones screenshot](https://github.com/MicrosoftDocs/windows-dev-docs/blob/a3812316d9d6678be4e526ffb4b1c4f3958d956d/hub/powertoys/images/fancyzones/fancy-zones2.png)
- [Command Palette settings screenshot](https://github.com/MicrosoftDocs/windows-dev-docs/blob/a3812316d9d6678be4e526ffb4b1c4f3958d956d/hub/powertoys/images/command-palette/cmdpal-settings.png)
- [HandBrake 1.8 encoding queue](https://github.com/HandBrake/HandBrake-docs/blob/3553ecc22468475017614d6e0e79e3e095f7c41c/source/docs/en/images/windows/queue-1.8.0.png) and [audio defaults](https://github.com/HandBrake/HandBrake-docs/blob/3553ecc22468475017614d6e0e79e3e095f7c41c/source/docs/en/images/windows/audio-defaults-win-1.8.0.png) by the HandBrake Team and documentation contributors. [CC BY-SA 4.0](../catalog/artwork-source/HandBrake-Docs-LICENSE)
- [Windows Terminal split panes](https://github.com/microsoft/terminal/blob/cf5e6bf22727a251aced8bc30491c87428a4e116/doc/images/panes.png) by Microsoft. [MIT](../catalog/artwork-source/Microsoft-Terminal-LICENSE)
- [Visual Studio Code editor and sidebar](https://github.com/microsoft/vscode-docs/blob/3d196e838cf2a85ced6b8ac679319de42954c576/docs/editing/images/userinterface/hero.png) by Microsoft. [CC BY 3.0 US](../catalog/artwork-source/VSCode-Docs-LICENSE.md)

The six screenshots above remain inside the portable executable for use offline.
