# Packaging

| Channel | Where | Notes |
|---|---|---|
| GitHub releases | `.github/workflows/release.yml` | x86_64 + aarch64 tarballs, `.deb`, `.rpm`, checksums |
| crates.io | same workflow | needs a `CARGO_REGISTRY_TOKEN` repository secret |
| AUR | `aur/clipt`, `aur/clipt-bin` | push each directory to its AUR git repo; update `pkgver` and `sha256sums` (`updpkgsums`) per release, regenerate `.SRCINFO` with `makepkg --printsrcinfo` |
| Nix | `/flake.nix` | `nix run github:jasondyck/clipt` |
| Flatpak (prototype) | `flatpak/` | not published; for KDE and GNOME; on wlroots and niri it needs `clipt-launch`, see `flatpak/README.md` |

Distribution packagers: `cargo build --release --locked`, then install
`target/release/clipt` to `bin/` and `data/clipt.desktop` to
`share/applications/`. The desktop entry matters: Plasma 6.0–6.7 only let
binaries listed in one capture through KWin silently (6.8 dropped that
check), and GNOME's portal needs it to remember the screenshot permission. Runtime needs: libxkbcommon, fontconfig (`fc-match`) and any sans-serif
font. Recording also wants ffmpeg, and on KDE and GNOME libpipewire-0.3,
which clipt loads only when it records (it isn't linked, so screenshots
work without it); worth a recommends/optional dependency.
