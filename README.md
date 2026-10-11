# Notepad (Rust)

A minimal plain-text editor for macOS, Windows and Linux.

* Opens files of any size (tested with 1 GB+) without loading them into memory:
  the file is read on demand, edits are kept in a piece table.
* Plain text only — paste formatted text, copy it back, and the formatting is gone.
* Every launch starts with an empty, untitled document.
* Asks whether to save unsaved changes when the window is closed.
* One document per window; `Cmd/Ctrl+N` opens another window (a separate process).
* Find and replace (case-insensitive search also works for Cyrillic etc.).
  "Replace All" streams through the file, so it works on 1 GB files too.
* Edit → Clear (new document) empties the window.
* Syntax highlighting (Language menu, detected from the file extension).
* View → Theme: System, Light or Dark (remembered between launches).
* Help → About Notepad shows the version.

## Build and run

```sh
cargo run --release                 # empty document
cargo run --release -- path/to/file # open a file
./build_scripts/bundle-macos.sh     # builds target/release/Notepad.app
./build_scripts/bundle-macos.sh --universal  # Apple Silicon + Intel, in target/universal-apple-darwin/release
cargo test --release                # tests
```

The app icon is `assets/icon.png`, drawn by `build_scripts/make_icon.py` (needs Pillow).

## Prebuilt macOS app

Download `Notepad-<version>-macos-universal.zip` (Apple Silicon + Intel) or
`Notepad-<version>-macos-arm64.zip` from
[Releases](https://github.com/MarketKernel/notepad/releases).

To publish a release, set `version` in `Cargo.toml`, commit, and push a matching
`v<version>` tag; GitHub Actions builds both archives and attaches them to a new
release (a tag that doesn't match `Cargo.toml` fails the build):

```sh
git tag v0.1.0
git push origin v0.1.0
```

The app is not signed or notarized, so macOS blocks it after download. Unzip it
and remove the quarantine flag:

```sh
xattr -dr com.apple.quarantine Notepad.app
```

Alternatively, try to open the app once, then click "Open Anyway" in
System Settings → Privacy & Security.

## Shortcuts (Cmd on macOS, Ctrl elsewhere)

| Keys | Action |
|---|---|
| N / O / S / Shift+S | New window / Open / Save / Save As |
| W, Q | Close the window (asks to save) |
| Z / Shift+Z (Ctrl+Y) | Undo / Redo |
| F, G / Shift+G, F3 | Find, next / previous match |
| Option+F (i.e. Cmd+Option+F on macOS), H (Windows/Linux) | Find and replace |
| L | Go to line |
| = / - / 0 | Font size |

Files can also be dropped onto the window.

## Limitations

* Text is treated as UTF-8 (invalid bytes are shown as `�` and saved unchanged).
* Lines longer than 64 KB are shown truncated (they can still be edited and are saved in full).
* Copying more than 256 MB to the clipboard is refused.
* Saving writes a temporary file next to the target and renames it over the original.
* macOS "Open With…" from Finder is not supported yet (use the menu, drag and drop, or the command line).
* Windows and Linux builds compile but have not been tested yet.
