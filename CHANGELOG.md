# Changelog

## TS Word 1.1.1

- **RENAMED APP BUNDLE**: the macOS app bundle is now **TS Word.app** (it was TsW.app), so Spotlight finds the app when you search "Word". The bundle id, documents, settings and data folders are unchanged — everything keeps working exactly as before, and the built-in updater handles the rename by itself.

— tonynehihi

## TS Word 1.1.0

First release of TS Word, a personal build based on WordCraft 0.9.0 by storytold.

- **NEW NAME AND ICON**: the app is now "TS Word" everywhere — window title, About screen and the macOS bundle **TsW.app** — with a new Word-blue icon drawn in the Tony Suite design language. Settings and document folders keep their original names, so existing data is untouched.
- **SELF-UPDATE BUILT IN**: Help → **Check for Updates** (and a one-time check at launch) asks this repo's Releases for a newer version and shows its changelog in the app. On macOS the update downloads, builds and installs itself; the app closes only for a few seconds at the end to swap versions, keeps the previous version as a backup, and reopens on its own.
- **UPSTREAM LINKS REMOVED**: the Help tab is now exactly About / Check for Updates / System Info / Keyboard Shortcuts. The Discord button in the title bar, the Discord entry in the File menu, and the website / Discord / source links in About are gone — nothing in the app points back to the upstream project anymore.
- Everything else is WordCraft 0.9.0 as released by storytold; TS Word tracks the upstream project and folds its new features into future releases.

— tonynehihi
