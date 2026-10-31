# Other Wayland compositors for clipt's end-to-end tests: labwc (wlroots,
# headless backend) and niri (Smithay; nested in a headless Sway, software
# GL). Hyprland is absent because its headless backend needs a GPU.
FROM archlinux:latest
RUN pacman -Syu --noconfirm --needed labwc niri sway swaybg mesa grim wl-clipboard jq imagemagick noto-fonts procps-ng >/dev/null \
 && pacman -Scc --noconfirm >/dev/null \
 && setcap -r /usr/bin/sway /usr/bin/labwc /usr/bin/niri 2>/dev/null; true
ENV XDG_RUNTIME_DIR=/tmp/xdg WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 LIBGL_ALWAYS_SOFTWARE=1
