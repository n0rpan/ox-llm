#!/bin/sh
# Renders the view components offscreen with stand-ins for Omarchy's kit (tests/panel_preview/qs).
# Checks that the QML loads and lays out; the real kit and theme are only in the running shell.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
# QT_QPA_PLATFORMTHEME= (empty) disables the qtengine platform theme: under the isolated
# XDG_CONFIG_HOME it has no config file and would print "qtengine.configwatcher: No config file
# path available, config watching disabled" (and a QFSFileEngine warning) to stderr.
QT_QPA_PLATFORMTHEME= QT_QPA_PLATFORM=offscreen QT_FORCE_STDERR_LOGGING=1 QML_XHR_ALLOW_FILE_READ=1 \
  timeout 30 qml6 -I "$here" "$here/preview.qml" -- "$(realpath "$1")" "$(realpath -m "$2")"
