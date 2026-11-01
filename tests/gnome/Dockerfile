# Headless GNOME Shell for testing clipt's portal + xdg-shell fallback paths.
FROM archlinux:latest
RUN pacman -Syu --noconfirm --needed gnome-shell xdg-desktop-portal xdg-desktop-portal-gnome wl-clipboard imagemagick jq glib2 noto-fonts procps-ng python python-gobject python-dbusmock evolution-data-server pipewire wireplumber ffmpeg >/dev/null && pacman -Scc --noconfirm >/dev/null
ENV XDG_RUNTIME_DIR=/tmp/xdg XDG_CURRENT_DESKTOP=GNOME XDG_SESSION_TYPE=wayland
