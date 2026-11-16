# Other Wayland compositors for clipt's end-to-end tests: labwc (wlroots,
# headless backend) and niri (Smithay; nested in a headless Sway, software
# GL). Hyprland is absent because its headless backend needs a GPU.
FROM archlinux:latest
# labwc ships cap_sys_nice=ep, which a container denied that capability can't
# exec at all ("Operation not permitted"). setcap -r stops at the first file
# without capabilities, so strip each on its own and forgive the ones that
# need nothing stripped. A failed pacman is a failed build: a cached image
# without the compositors makes the suites test nothing.
RUN pacman -Syu --noconfirm --needed labwc niri sway swaybg mesa grim wl-clipboard jq imagemagick noto-fonts procps-ng >/dev/null \
 && pacman -Scc --noconfirm >/dev/null \
 && for b in sway labwc niri; do setcap -r /usr/bin/$b 2>/dev/null || true; done
ENV XDG_RUNTIME_DIR=/tmp/xdg WLR_RENDERER=pixman WLR_LIBINPUT_NO_DEVICES=1 LIBGL_ALWAYS_SOFTWARE=1
