# Install and update

## From the repository

```powershell
git clone https://github.com/toeeshchaudhary/destro
cd destro
cargo install --path destro-tui --force
cargo install --path crates/destro-runtime --force
```

That puts `destro` (the TUI) and `destro-harness` (the JSON-RPC runtime) in
`%USERPROFILE%\.cargo\bin`, which is already on `PATH` for a standard Rust
installation. Verify:

```powershell
destro --probe
destro-harness --help
```

`--probe` runs the in-process harness once and prints the transcript; it needs
no API key and no terminal.

## Straight from git

```powershell
cargo install --git https://github.com/toeeshchaudhary/destro -p destro-tui --force
cargo install --git https://github.com/toeeshchaudhary/destro -p destro-runtime --force
```

## Updating

```powershell
destro update
```

The updater asks GitHub for the latest release, compares it with the running
build, and replaces the executable in place. It only touches the binary it is
running from; restart `destro` afterwards. See [UPDATES.md](UPDATES.md) for the
release process and asset naming.

Until the first tagged release exists, `destro update` reports that there is no
release yet and exits successfully.
