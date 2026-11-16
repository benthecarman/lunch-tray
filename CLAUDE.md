# Lunch Tray

## The iOS app copies the core rules

Swift cannot use the Rust core, so the iOS app in `ios/` keeps its own copy
of the task model and its rules. When you change one of these in
`crates/core`, make the same change in the Swift file and its tests in
`ios/LunchTrayTests/ModelTests.swift`:

| Rust | Swift |
|---|---|
| `model.rs`: task fields, enums, JSON names, `matches`, `rung`, sections | `ios/LunchTray/Model.swift`, `Store.swift` |
| `model.rs`: transitions (`settle`, `reopen`, `archive`, `delete`, ...) | `ios/LunchTray/Store.swift` (`Store.apply`) |
| `ops.rs`: `Op` and its JSON | `ios/LunchTray/Store.swift` (`Op`) |
| `api.rs`: `Delta`, `Push`, `Hello`, `OpBatch`, routes | `ios/LunchTray/Model.swift`, `WorkerClient.swift` |
| `src/remote.rs`: the link to the Worker | `ios/LunchTray/TrayModel.swift` |

The Worker is the authority: if the copies disagree, the app shows a change
at once and the Worker's answer replaces it on the next push. Keep them in
step so nothing flickers back.
