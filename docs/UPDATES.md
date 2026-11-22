# Updates

Destro ships as bare executables attached to GitHub releases in
[`toeeshchaudhary/destro`](https://github.com/toeeshchaudhary/destro). There are
no archives and no installers.

## Cutting a release

1. Bump the version in the root `Cargo.toml` (`[workspace.package] version`).
2. Commit, then push a tag named `v<semver>`:

   ```sh
   git tag v0.2.0
   git push origin v0.2.0
   ```

3. The `Release` workflow (`.github/workflows/release.yml`) runs on Windows,
   builds `cargo build --release --workspace`, and creates the GitHub release
   named after the tag with generated notes.

It uploads exactly two assets:

| Local file                          | Released as                                  |
| ----------------------------------- | -------------------------------------------- |
| `target/release/destro.exe`         | `destro-x86_64-pc-windows-msvc.exe`          |
| `target/release/destro-harness.exe` | `destro-harness-x86_64-pc-windows-msvc.exe`  |

## Asset naming

Assets are named `<binary>-<target-triple>[.exe]`, using the triple Rust reports
for the build host:

- `destro-x86_64-pc-windows-msvc.exe`
- `destro-aarch64-pc-windows-msvc.exe`
- `destro-x86_64-unknown-linux-gnu`
- `destro-aarch64-unknown-linux-gnu`
- `destro-x86_64-apple-darwin`
- `destro-aarch64-apple-darwin`

The updater (`destro-tui/src/updater.rs`) derives the same name from
`target_triple()` and `asset_name()`, so a new platform only needs its workflow
assets added with that exact spelling.

## What the updater does

On `destro update` the updater:

1. Requests `https://api.github.com/repos/toeeshchaudhary/destro/releases/latest`
   with a `destro-updater/<version>` user agent (a 404 means no releases yet).
2. Parses `tag_name` into a version and compares it with the running build's
   `CARGO_PKG_VERSION`. If the release is not newer, it reports "up to date".
3. Downloads the asset matching `<binary>-<target-triple>` into memory
   (128 MiB cap).
4. Writes it beside the running executable as `<exe>.new`, then swaps it in:
   on Windows the running exe is renamed to `<exe>.old` first (stale `.old`
   files are cleared), on Unix the new file is made executable and renamed over
   the old one.

The replaced binary takes effect the next time `destro` starts.

## Manual update

```sh
destro update
```

Not wired into the CLI yet — the command is added when `updater` is declared in
`destro-tui/src/main.rs`. Until then, download the asset for your platform from
the latest release page and replace the executable by hand.
