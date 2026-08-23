#!/bin/bash
# Status line, two rows:
#   [Opus 4.8] ▓▓▓▓░░░░░░ 43%              [2h13m] ▓▓▓▓░░░░░░ 18%
#   main
# Row 1: context window usage on the left, 5-hour rate limit on the right.
# Row 2: current git branch.

COLOR='\033[38;2;236;236;236m'
RESET='\033[0m'
BAR_WIDTH=10

# Build a BAR_WIDTH bar for a percentage. Sets $bar.
build_bar() {
  local pct=$1 filled empty fill pad
  filled=$(( pct * BAR_WIDTH / 100 ))
  [ "$filled" -gt "$BAR_WIDTH" ] && filled=$BAR_WIDTH
  [ "$filled" -lt 0 ] && filled=0
  empty=$(( BAR_WIDTH - filled ))
  printf -v fill '%*s' "$filled" ''
  printf -v pad '%*s' "$empty" ''
  bar="${fill// /▓}${pad// /░}"
}

input=$(cat)

# --- Row 1 left: model name and context window bar ---
model=$(echo "$input" | jq -r '.model.display_name // empty')
# Defaults to 0 so the bar still draws on a fresh session, before the first
# API response populates context_window.
used=$(echo "$input" | jq -r '.context_window.used_percentage // 0')
left=""
left_cols=0
if [ -n "$model" ]; then
  left="[${model}]"
  left_cols=$(( ${#model} + 2 ))
fi
used_int=$(printf '%.0f' "$used")
build_bar "$used_int"
left="${left} ${bar} ${used_int}%"
# Count columns, not bytes: the block characters are multibyte.
left_cols=$(( left_cols + 1 + BAR_WIDTH + 1 + ${#used_int} + 1 ))

# --- Row 1 right: time until the 5-hour limit resets, and how much is used ---
# Both default to 0 so this side still draws before the first API response.
rl_pct=$(echo "$input" | jq -r '.rate_limits.five_hour.used_percentage // 0')
rl_reset=$(echo "$input" | jq -r '.rate_limits.five_hour.resets_at // 0')
secs=0
if [ "$rl_reset" -gt 0 ]; then
  secs=$(( rl_reset - $(date +%s) ))
  [ "$secs" -lt 0 ] && secs=0
fi
if [ "$secs" -ge 3600 ]; then
  printf -v reset_str '[%dh%dm]' $(( secs / 3600 )) $(( secs % 3600 / 60 ))
else
  printf -v reset_str '[%dm]' $(( secs / 60 ))
fi
rl_int=$(printf '%.0f' "$rl_pct")
build_bar "$rl_int"
right="${reset_str} ${bar} ${rl_int}%"
right_cols=$(( ${#reset_str} + 1 + BAR_WIDTH + 1 + ${#rl_int} + 1 ))

# --- Row 1: pad the two sides apart ---
# COLUMNS is the full terminal width, but the status line row is indented by the
# interface's own spacing, so reserve a few columns or the tail gets truncated.
reserve=4
cols=${COLUMNS:-80}
row1="$left"
if [ -n "$right" ]; then
  pad=$(( cols - left_cols - right_cols - reserve ))
  if [ "$pad" -ge 1 ]; then
    printf -v gap '%*s' "$pad" ''
    row1="${left}${gap}${right}"
  else
    # Too narrow for both: one space between them and let it wrap.
    row1="${left} ${right}"
  fi
fi

# --- Row 2: git branch ---
dir=$(echo "$input" | jq -r '.workspace.current_dir // .cwd // empty')
branch=""
[ -n "$dir" ] && branch=$(git -C "$dir" branch --show-current 2>/dev/null)

printf "%b%s%b" "$COLOR" "$row1" "$RESET"
if [ -n "$branch" ]; then
  printf "\n%b%s%b" "$COLOR" "$branch" "$RESET"
fi

# Must exit 0: any non-zero status makes Claude Code blank the status line.
exit 0