#!/bin/bash
# Start Zerem's Claude session in a Ghostty window after login/reboot.
#
# CHANGE 2026-11-22: the 4 sisters (Zero, Zeed, Zelot, Zeno) no longer launch
# here — pTerminal now hosts them and restores their tabs itself on login
# (see deploy/linux/pterminal.desktop + launch-profiles.json). Only ZEREM
# stays in Ghostty. Keep this file for Zerem; pTerminal handles the rest.
#
# Usage: ~/zerem-team/start-all.sh   (also run by ~/.config/autostart/zerem-team.desktop)

CH="plugin:discord@claude-plugins-official"
NOTE="[auto-start] เครื่องเพิ่งรีสตาร์ท ระบบเปิด session ให้อัตโนมัติ: เช็ค memory ว่ามีอะไรต้องเปิดใหม่ (background job, monitor, cron ของ session) แล้วทำงานค้างต่อ"
LOG="$HOME/zerem-team/start-all.log"

# Skip any session that is already running, so running the script twice doesn't duplicate bots.
# A session counts as running if a "claude --channels" process has that working directory.
running() { for p in $(pgrep -u "$USER" -f "claude --channels"); do [ "$(readlink /proc/$p/cwd)" = "$1" ] && return 0; done; return 1; }

launch() { # title cwd command
  echo "$(date '+%F %T') start $1" >> "$LOG"
  ghostty --title="$1" --working-directory="$2" -e bash -lc "$3; echo; echo '[$1 exited] press Enter'; read" &
  sleep 4
}

sleep 20   # let the desktop, network and keyring come up first

# Zerem only, in $HOME (her project dir), with Chrome.
# The `running` guard also skips her if pTerminal (or anything) already started
# a "claude --channels" in $HOME, so this never double-launches a bot.
running "$HOME" || \
  launch "Zerem"  "$HOME"                  "claude --channels $CH --continue --chrome \"$NOTE\""
