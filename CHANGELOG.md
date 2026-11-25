# Changelog

## 1.0.1

Windows displays version `1.0.1.0`.

- Load descriptions and icons for visible search results from exact package metadata and verified artwork sources. Descriptions get priority over slower icon probes.
- Show catalog metadata and validated links in Details while WinGet checks installed state. Back restores the correct package immediately.
- Fetch screenshots on demand in Details, including for live search results.
- Use **Save settings locally** and **Cache app icon and data locally** to keep icons, descriptions, and public package details between launches.
- Click an eligible card or row to toggle selection and use **Details** to open its overview. Find **Clear selection** beside the main actions.
- Keep overview links on one row, fix screenshot preview controls, and refresh Downloads progress when installer output changes.
- Verify that a renamed executable can reuse its portable settings and caches.

## 1.0.0

First stable Windows x64 release. Windows displays version `1.0.0.0`.

- Browse, search, and manage apps from the WinGet community source through a portable single EXE.
- Search Scoop Main and Extras when WinGet returns no matches. Use **Also search Scoop** from other search results.
- Review selected installs, uninstalls, and updates before they enter the operation queue.
- Load app icons on demand and keep them in memory. Use Settings to control online artwork and optional portable storage.
- Read artwork credits and source availability in quiet dialogs.
