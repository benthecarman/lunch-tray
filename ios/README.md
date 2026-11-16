# Lunch Tray for iOS

A native SwiftUI app for the same task list as the desktop app. It talks
to your Worker the same way: it downloads the list once, then the Worker
pushes each change down a WebSocket while the app is open. A change you
make shows at once and goes to the Worker, which applies it with the same
rules as the desktop app and the MCP tools.

## Features

- Pinned, Active, Settled, and Archived sections, each collapsible, sorted
  by last activity.
- Tap a row to open its pull request or issue. A note with no link opens its
  editor.
- Swipe right to pin, unpin, or reopen. Swipe left to settle, archive, or
  delete. Long-press for every action, including rename and share.
- Search by org, repo, title, or number. Every word must match, so
  `ldk 291` finds ldk-server #291. A search also looks through the archive.
- CI results and merge conflicts on open pull requests, and a mustard dot
  for new activity.
- Pull to refresh. The menu has Sync Now, Mark All as Read, and Archive
  Older Than a Year.
- Works offline: the app keeps the last list and the changes it has not
  sent yet, and sends them when the Worker is back. A resend after a lost
  answer is safe, since the Worker remembers each batch.
- The desktop palette in light and dark mode, or a fixed theme in
  Settings.

The socket only lives while the app is in front. When it comes back, the app
catches up over HTTP, asking only for what changed, then opens the socket
again. Without a socket it checks every 10 seconds.

## Build

You need Xcode 16 or newer and [XcodeGen](https://github.com/yonaskolb/XcodeGen)
(`brew install xcodegen`). The Xcode project is generated from
`project.yml`:

```sh
cd ios
xcodegen generate
open LunchTray.xcodeproj
```

Set your team under Signing & Capabilities, or change `DEVELOPMENT_TEAM` in
`project.yml`, then run it on your phone. From the command line:

```sh
xcodebuild -project LunchTray.xcodeproj -scheme LunchTray \
  -destination 'generic/platform=iOS' -configuration Release \
  -derivedDataPath build -allowProvisioningUpdates build
xcrun devicectl device install app --device <device id> \
  "build/Build/Products/Release-iphoneos/Lunch Tray.app"
```

`xcrun devicectl list devices` shows the device id.

On first launch, Settings asks for the Worker's URL and its `API_TOKEN`,
the same two values as the desktop app's `config.toml`. The token is kept in
the keychain.

## Develop

The model and its rules mirror `crates/core`: `Model.swift` follows the
JSON of `model.rs` and `api.rs`, and `Store.swift` follows `ops.rs` and the
transitions in `model.rs`. A change to either crate file should be made
here too. `TrayModel.swift` is the link to the Worker, as `src/remote.rs`
is for the desktop app.

```sh
xcodebuild test -project LunchTray.xcodeproj -scheme LunchTray \
  -destination 'platform=iOS Simulator,name=<a simulator>'
```

The icon is drawn from the desktop mark by `scripts/make-icon.swift`:

```sh
swift scripts/make-icon.swift LunchTray/Assets.xcassets/AppIcon.appiconset/AppIcon.png
```
