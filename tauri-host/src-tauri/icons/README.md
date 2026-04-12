# Icons

This directory must contain real icon assets before producing a signed, bundled release build:

- `icon.ico` (Windows)
- `icon.png` (Linux)
- `32x32.png`
- `128x128.png`
- `128x128@2x.png`

For development workflows (`cargo check`, `cargo build`) the scaffold has `bundle.active = false`
in `tauri.conf.json`, so these icon files are not required for the scaffold to compile.

Generate them with:

```sh
cargo tauri icon path/to/source-icon.png
```

once a source icon is available.
