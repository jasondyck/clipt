# Headless Sway for end-to-end tests of clipt's wlroots paths.
FROM archlinux:latest
RUN pacman -Syu --noconfirm --needed sway swaybg wl-clipboard grim wtype jq procps-ng noto-fonts imagemagick xorg-xwayland ffmpeg >/dev/null && setcap -r /usr/bin/sway && pacman -Scc --noconfirm >/dev/null
ENV XDG_RUNTIME_DIR=/tmp/xdg WLR_BACKENDS=headless WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1
