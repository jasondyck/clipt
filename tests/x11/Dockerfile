# Xvfb + Openbox for end-to-end tests of clipt's X11 backend.
FROM archlinux:latest
RUN pacman -Syu --noconfirm --needed xorg-server xf86-video-dummy xorg-server-xvfb xorg-xrandr xorg-xdpyinfo xdotool xclip openbox imagemagick noto-fonts procps-ng ffmpeg xorg-xsetroot xorg-xlogo >/dev/null && pacman -Scc --noconfirm >/dev/null
ENV DISPLAY=:1
