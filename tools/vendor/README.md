# Vendored files

- `Karabiner-DriverKit-VirtualHIDDevice-8.7.0.pkg`: the virtual HID keyboard that
  types into the macOS lock screen (docs/33-macos.md, "Locked screen"), from
  https://github.com/pqrs-org/Karabiner-DriverKit-VirtualHIDDevice/releases/tag/v8.7.0,
  signed and notarized by its author. Released into the public domain (The Unlicense).
  Kept here because downloads of release files from the CI failed (HTTP 504);
  tools/package-macos.sh checks its SHA-256 and copies it into the app.
