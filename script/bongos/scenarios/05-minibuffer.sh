# Scenario for feature 05-minibuffer. Sourced by script/bongos/smoke after the base flow (the app is
# quit, final.png exists), inside the bwrap sandbox, with set -euo pipefail, RUN, HOME, DISPLAY, the
# proxy and the helpers of lib.sh.
#
# The minibuffer is on by default, so settings.json says nothing about `minibuffer`; it holds only
# `"base_keymap": "Emacs"` (M-x is bound there) and the theme.
#   1. One Light: the app launches; the strip is below the status bar, with a border above it. M-x
#      opens the command palette in the strip: the rows at the bottom of the window change (the prompt
#      and the query typed in it, which a centered modal would leave alone); minibuffer-light.png shows
#      it. C-g closes it and the bottom rows are as they were.
#   2. One Dark: the same, saved as minibuffer-dark.png.
#   3. settings.json never mentions `minibuffer`.
# What no file shows (fonts and colors of the strip) is saved as screenshots for the orchestrator to read.
# The scenario leaves the app quit and restores settings.json; it never writes final.png.

if [ -n "${stock:-}" ]; then
  say "smoke: scenario 05-minibuffer skipped under BONGOS_STOCK=1: the stock run has no minibuffer"
  return 0
fi

CONFIG_DIR="$HOME/$BONGOS_CONFIG_PATH"
SETTINGS_FILE="$CONFIG_DIR/settings.json"
STRIP_ROWS=40

command -v import >/dev/null || { echo "bongos: ImageMagick is not installed; the scenario needs it" >&2; exit 1; }

fail() {
  echo "bongos: $*" >&2
  exit 1
}

# The md5 of a band of the app window: <$1> pixels high starting <$2> pixels below its top.
band_hash() {
  local WINDOW X Y WIDTH HEIGHT SCREEN
  eval "$(xdo getwindowgeometry --shell "$WIN")"
  DISPLAY="$BONGOS_DISPLAY" import -window root -crop "${WIDTH}x$1+${X}+$((Y + $2))" rgb:- | md5sum
}

# The md5 of the bottom STRIP_ROWS rows of the window, where the strip and the status bar are.
bottom_hash() {
  local WINDOW X Y WIDTH HEIGHT SCREEN
  eval "$(xdo getwindowgeometry --shell "$WIN")"
  band_hash "$STRIP_ROWS" $((HEIGHT - STRIP_ROWS))
}

# The number of distinct colors in the bottom STRIP_ROWS rows of the window.
bottom_colors() {
  local WINDOW X Y WIDTH HEIGHT SCREEN
  eval "$(xdo getwindowgeometry --shell "$WIN")"
  DISPLAY="$BONGOS_DISPLAY" import -window root \
    -crop "${WIDTH}x${STRIP_ROWS}+${X}+$((Y + HEIGHT - STRIP_ROWS))" png:- | identify -format '%k' png:-
}

write_settings() {
  cat >"$SETTINGS_FILE" <<JSON
{
  "base_keymap": "Emacs",
  "theme": "$1"
}
JSON
}

# One round for a theme: the strip is there, M-x opens in it, C-g closes it. <$1> is the theme.
round() {
  local theme="$1" name="$2" closed open colors
  write_settings "$theme"
  sleep 2
  wait_for_settled
  closed=$(bottom_hash)
  colors=$(bottom_colors)
  [ "$colors" -ge 2 ] || fail "$theme: the bottom of the window is one flat color ($colors): no strip with a border"

  act keys alt+x
  act type_text "toggle"
  open=$(bottom_hash)
  [ "$open" != "$closed" ] || fail "$theme: M-x changed nothing at the bottom of the window"
  shot "$name"
  say "smoke: screenshot $RUN/$name.png"

  act keys ctrl+g
  [ "$(bottom_hash)" = "$closed" ] || fail "$theme: C-g did not leave the strip as it was"
}

# ---------------------------------------------------------------------------------------------
step "Minibuffer: defaults, M-x in the strip, One Light and One Dark"
mkdir -p "$RUN/config-backup"
cp "$SETTINGS_FILE" "$RUN/config-backup/settings-05.json"
write_settings "One Light"

launch
wait_for_window
poll 60 "a rendered frame" ui_rendered
wait_for_settled

round "One Light" minibuffer-light
round "One Dark" minibuffer-dark

quit_app
if grep -q minibuffer "$SETTINGS_FILE"; then
  fail "settings.json mentions the minibuffer, but it is on by default: $(cat "$SETTINGS_FILE")"
fi
cp "$RUN/config-backup/settings-05.json" "$SETTINGS_FILE"
