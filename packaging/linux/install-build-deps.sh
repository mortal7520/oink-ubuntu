#!/bin/sh
set -eu
sudo apt-get update
sudo apt-get install -y --no-install-recommends build-essential pkg-config libgtk-3-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libssl-dev libx11-dev libxext-dev libxtst-dev libxrandr-dev libxfixes-dev libxi-dev libxrender-dev libpulse-dev libasound2-dev libdbus-1-dev libpango1.0-dev
