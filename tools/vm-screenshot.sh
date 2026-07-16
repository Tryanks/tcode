#!/usr/bin/env bash

set -euo pipefail

usage() {
  echo "Usage: $0 <path-to-binary> <out-dir> [--dark|--light] [--seed <dir>] [--crop-window] [--app-arg <arg>] [--open-diff-ui] [--open-acp-marketplace-ui]" >&2
  exit 2
}

[[ $# -ge 2 ]] || usage

binary=$1
out_dir=$2
shift 2
appearance=
seed_dir=
crop_window=false
app_args=()
open_diff_ui=false
open_acp_marketplace_ui=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dark|--light)
      [[ -z "$appearance" ]] || usage
      appearance=$1
      shift
      ;;
    --seed)
      [[ -z "$seed_dir" && $# -ge 2 ]] || usage
      seed_dir=$2
      shift 2
      ;;
    --crop-window)
      crop_window=true
      shift
      ;;
    --app-arg)
      [[ $# -ge 2 ]] || usage
      app_args+=("$2")
      shift 2
      ;;
    --open-diff-ui)
      open_diff_ui=true
      shift
      ;;
    --open-acp-marketplace-ui)
      open_acp_marketplace_ui=true
      shift
      ;;
    *) usage ;;
  esac
done

[[ -f "$binary" ]] || { echo "Binary not found: $binary" >&2; exit 1; }
[[ -x "$binary" ]] || { echo "Binary is not executable: $binary" >&2; exit 1; }
if [[ -n "$seed_dir" ]]; then
  [[ -d "$seed_dir" ]] || { echo "Seed directory not found: $seed_dir" >&2; exit 1; }
  [[ -f "$seed_dir/sessions.json" ]] || { echo "Seed directory has no sessions.json: $seed_dir" >&2; exit 1; }
  seed_dir=$(cd "$seed_dir" && pwd -P)
fi

TART=${TART:-/opt/homebrew/bin/tart}
VM_NAME=${TCODE_VM_NAME:-macos}
VM_USER=${TCODE_VM_USER:-admin}
VM_PASSWORD=${TCODE_VM_PASSWORD:-admin}

[[ -x "$TART" ]] || { echo "tart not found at $TART" >&2; exit 1; }
command -v jq >/dev/null || { echo "jq is required" >&2; exit 1; }
command -v sshpass >/dev/null || { echo "sshpass is required (brew install hudochenkov/sshpass/sshpass)" >&2; exit 1; }

mkdir -p "$out_dir"
out_dir=$(cd "$out_dir" && pwd -P)
binary=$(cd "$(dirname "$binary")" && pwd -P)/$(basename "$binary")

vm_state() {
  "$TART" list --format json | jq -r --arg name "$VM_NAME" '.[] | select(.Name == $name) | .State' | head -1
}

state=$(vm_state)
[[ -n "$state" ]] || { echo "Tart VM '$VM_NAME' does not exist" >&2; exit 1; }

if [[ "$state" != "running" ]]; then
  log_file="${TMPDIR:-/tmp}/tcode-tart-${VM_NAME}.log"
  echo "Starting Tart VM '$VM_NAME'..."
  nohup "$TART" run "$VM_NAME" --no-graphics >"$log_file" 2>&1 &
  tart_pid=$!
  disown "$tart_pid" 2>/dev/null || true

  for _ in $(seq 1 60); do
    [[ "$(vm_state)" == "running" ]] && break
    if ! kill -0 "$tart_pid" 2>/dev/null; then
      echo "Tart exited while starting '$VM_NAME':" >&2
      tail -100 "$log_file" >&2 || true
      exit 1
    fi
    sleep 2
  done

  [[ "$(vm_state)" == "running" ]] || { echo "Timed out starting Tart VM '$VM_NAME'" >&2; exit 1; }
fi

echo "Waiting for VM IP address..."
ip=$("$TART" ip "$VM_NAME")
[[ -n "$ip" ]] || { echo "Could not determine VM IP" >&2; exit 1; }

export SSHPASS=$VM_PASSWORD
ssh_opts=(
  -o StrictHostKeyChecking=no
  -o UserKnownHostsFile=/dev/null
  -o LogLevel=ERROR
  -o ConnectTimeout=5
  -o ServerAliveInterval=5
  -o ServerAliveCountMax=3
)
ssh_cmd=(sshpass -e ssh "${ssh_opts[@]}")
scp_cmd=(sshpass -e scp "${ssh_opts[@]}")
remote="$VM_USER@$ip"

connected=false
for _ in $(seq 1 60); do
  if "${ssh_cmd[@]}" "$remote" true 2>/dev/null; then
    connected=true
    break
  fi
  sleep 2
done
[[ "$connected" == true ]] || { echo "Timed out waiting for SSH at $ip" >&2; exit 1; }

uid=$("${ssh_cmd[@]}" "$remote" "id -u '$VM_USER'")
[[ "$uid" =~ ^[0-9]+$ ]] || { echo "Could not determine VM user UID" >&2; exit 1; }

echo "Waiting for the VM GUI login session..."
gui_ready=false
for _ in $(seq 1 60); do
  if "${ssh_cmd[@]}" "$remote" \
    "who | awk '\$1 == \"$VM_USER\" && \$2 == \"console\" { found = 1 } END { exit !found }' && launchctl print 'gui/$uid' >/dev/null 2>&1" \
    2>/dev/null; then
    gui_ready=true
    break
  fi
  sleep 2
done
[[ "$gui_ready" == true ]] || { echo "Timed out waiting for the VM GUI login session" >&2; exit 1; }

remote_dir="/Users/$VM_USER/tcode-vm-screenshot"
label="dev.tcode.vm-screenshot"
plist="/Users/$VM_USER/Library/LaunchAgents/$label.plist"
remote_binary="$remote_dir/tcode"
remote_shot="$remote_dir/screenshot.png"
remote_seed="$remote_dir/seed"

encoded_app_args=
if [[ ${#app_args[@]} -gt 0 ]]; then
  for app_arg in "${app_args[@]}"; do
    encoded=$(printf '%s' "$app_arg" | base64 | tr -d '\r\n')
    [[ -z "$encoded_app_args" ]] || encoded_app_args+=,
    encoded_app_args+=$encoded
  done
fi
encoded_app_args_arg=${encoded_app_args:-_}

cleanup() {
  set +e
  if [[ -n "${ip:-}" && -n "${uid:-}" ]]; then
    "${ssh_cmd[@]}" "$remote" \
      "launchctl bootout gui/$uid/$label >/dev/null 2>&1 || true; pkill -x tcode >/dev/null 2>&1 || true; rm -f '$plist'" \
      >/dev/null 2>&1
  fi
}
trap cleanup EXIT INT TERM

"${ssh_cmd[@]}" "$remote" \
  "launchctl bootout gui/$uid/$label >/dev/null 2>&1 || true; pkill -x tcode >/dev/null 2>&1 || true; mkdir -p '$remote_dir' '/Users/$VM_USER/Library/LaunchAgents'; rm -rf '$remote_seed'; rm -f '$plist' '$remote_shot' '$remote_dir/tcode.log'"

echo "Copying $(basename "$binary") to $VM_NAME..."
"${scp_cmd[@]}" "$binary" "$remote:$remote_binary"
"${ssh_cmd[@]}" "$remote" "chmod +x '$remote_binary'"

if [[ -n "$seed_dir" ]]; then
  echo "Installing seed from $seed_dir..."
  "${scp_cmd[@]}" -r "$seed_dir" "$remote:$remote_seed"
  "${ssh_cmd[@]}" "$remote" /bin/zsh -s -- "$remote_seed" "$VM_USER" <<'REMOTE_SEED'
set -euo pipefail

seed_dir=$1
vm_user=$2
data_dir="/Users/$vm_user/Library/Application Support/tcode"

rm -rf "$data_dir"
mkdir -p "$data_dir" "/Users/$vm_user/tcode-demo"
cp -R "$seed_dir"/. "$data_dir"/
REMOTE_SEED
fi

if [[ -n "$appearance" ]]; then
  dark_mode=false
  [[ "$appearance" == "--dark" ]] && dark_mode=true
  "${ssh_cmd[@]}" "$remote" \
    "osascript -e 'tell application \"System Events\" to tell appearance preferences to set dark mode to $dark_mode'"
  sleep 2
fi

# Enable Do Not Disturb inside the guest before launching tcode. On current
# macOS this state is owned by Control Center rather than a stable defaults key,
# so drive the real Focus UI. If Focus is already visible in the menu bar, DND
# is active and is left alone. Older guests fall back to the legacy preference;
# the notification dismissal immediately before capture remains as a backstop.
"${ssh_cmd[@]}" "$remote" /bin/zsh -s <<'REMOTE_ENABLE_DND'
osascript <<'APPLESCRIPT' >/dev/null 2>&1 || true
tell application "System Events"
  tell process "ControlCenter"
    if not (exists (first menu bar item of menu bar 1 whose description is "Focus")) then
      click (first menu bar item of menu bar 1 whose description is "Control Center")
      delay 0.5
      if (count of windows) > 0 then
        set panel to group 1 of window 1
        set focusTile to missing value
        set greatestY to -1
        repeat with candidate in checkboxes of panel
          try
            set candidateSize to size of candidate
            set candidatePosition to position of candidate
            if item 1 of candidateSize is 140 and item 2 of candidateSize is 64 then
              if item 2 of candidatePosition > greatestY then
                set focusTile to candidate
                set greatestY to item 2 of candidatePosition
              end if
            end if
          end try
        end repeat
        if focusTile is not missing value then
          click focusTile
          delay 0.5
          if (count of windows) > 0 and (count of checkboxes of group 1 of window 1) > 0 then
            click checkbox 1 of group 1 of window 1
            delay 0.5
          end if
        end if
      end if
    end if
    key code 53
  end tell
end tell
APPLESCRIPT
defaults -currentHost write com.apple.notificationcenterui doNotDisturb -bool true >/dev/null 2>&1 || true
REMOTE_ENABLE_DND

seed_mode=unseeded
[[ -n "$seed_dir" ]] && seed_mode=seeded
"${ssh_cmd[@]}" "$remote" /bin/zsh -s -- "$uid" "$remote_dir" "$label" "$plist" "$seed_mode" "$encoded_app_args_arg" <<'REMOTE_LAUNCH'
set -euo pipefail

uid=$1
remote_dir=$2
label=$3
plist=$4
seed_mode=$5
encoded_app_args=$6
[[ "$encoded_app_args" == _ ]] && encoded_app_args=

rm -f "$plist"
plutil -create xml1 "$plist"
plutil -insert Label -string "$label" "$plist"
plutil -insert ProgramArguments -array "$plist"
plutil -insert ProgramArguments.0 -string "$remote_dir/tcode" "$plist"
if [[ "$seed_mode" == "seeded" ]]; then
  plutil -insert ProgramArguments.1 -string "--open-latest" "$plist"
fi
argument_index=2
if [[ "$seed_mode" != "seeded" ]]; then
  argument_index=1
fi
if [[ -n "$encoded_app_args" ]]; then
  for encoded in ${(s:,:)encoded_app_args}; do
    decoded=$(printf '%s' "$encoded" | /usr/bin/base64 -D)
    plutil -insert "ProgramArguments.$argument_index" -string "$decoded" "$plist"
    argument_index=$((argument_index + 1))
  done
fi
plutil -insert WorkingDirectory -string "$remote_dir" "$plist"
plutil -insert RunAtLoad -bool true "$plist"
plutil -insert ProcessType -string Interactive "$plist"
plutil -insert LimitLoadToSessionType -string Aqua "$plist"
plutil -insert StandardOutPath -string "$remote_dir/tcode.log" "$plist"
plutil -insert StandardErrorPath -string "$remote_dir/tcode.log" "$plist"
for _ in {1..20}; do
  if launchctl print "gui/$uid/$label" >/dev/null 2>&1; then
    exit 0
  fi
  if launchctl bootstrap "gui/$uid" "$plist" >/dev/null 2>&1; then
    exit 0
  fi
  sleep 1
done
echo "Failed to bootstrap $label into gui/$uid" >&2
exit 1
REMOTE_LAUNCH

echo "Waiting 6 seconds for tcode to render..."
sleep 6

window_count=0
for _ in $(seq 1 15); do
  window_count=$("${ssh_cmd[@]}" "$remote" /bin/zsh -s <<'REMOTE_VERIFY'
osascript <<'APPLESCRIPT'
tell application "System Events"
  if not (exists process "tcode") then return 0
  tell process "tcode"
    set frontmost to true
    if (count of windows) > 0 then
      try
        set size of window 1 to {984, 720}
        set position of window 1 to {20, 24}
      end try
    end if
    return count of windows
  end tell
end tell
APPLESCRIPT
REMOTE_VERIFY
  )

  window_count=$(echo "$window_count" | tr -d '[:space:]')
  [[ "$window_count" =~ ^[1-9][0-9]*$ ]] && break
  sleep 1
done

if [[ ! "$window_count" =~ ^[1-9][0-9]*$ ]]; then
  echo "tcode did not create a visible window (window count: ${window_count:-unknown})" >&2
  "${ssh_cmd[@]}" "$remote" "tail -100 '$remote_dir/tcode.log'" >&2 || true
  exit 1
fi

if [[ "$open_diff_ui" == true ]]; then
  diff_result=$(
    "${ssh_cmd[@]}" "$remote" /bin/zsh -s <<'REMOTE_OPEN_DIFF'
osascript <<'APPLESCRIPT'
tell application "System Events"
  tell process "tcode"
    set frontmost to true
    set windowPosition to position of window 1
    set windowSize to size of window 1
  end tell
end tell
return (item 1 of windowPosition as text) & "," & (item 2 of windowPosition as text) & "," & (item 1 of windowSize as text)
APPLESCRIPT
REMOTE_OPEN_DIFF
  )
  IFS=, read -r window_x window_y window_width <<<"$diff_result"
  click_x=$((window_x + window_width - 23))
  click_y=$((window_y + 26))
  "${ssh_cmd[@]}" "$remote" /usr/bin/swift - "$click_x" "$click_y" <<'REMOTE_CLICK_DIFF'
import CoreGraphics
import Darwin
let args = CommandLine.arguments
let point = CGPoint(x: Double(args[1])!, y: Double(args[2])!)
CGWarpMouseCursorPosition(point)
usleep(100_000)
CGEvent(mouseEventSource: nil, mouseType: .leftMouseDown, mouseCursorPosition: point, mouseButton: .left)?.post(tap: .cghidEventTap)
usleep(100_000)
CGEvent(mouseEventSource: nil, mouseType: .leftMouseUp, mouseCursorPosition: point, mouseButton: .left)?.post(tap: .cghidEventTap)
REMOTE_CLICK_DIFF
  sleep 1
fi

if [[ "$open_acp_marketplace_ui" == true ]]; then
  geometry=$("${ssh_cmd[@]}" "$remote" /bin/zsh -s <<'REMOTE_ACP_GEOMETRY'
osascript <<'APPLESCRIPT'
tell application "System Events" to tell process "tcode"
  set p to position of window 1
  set s to size of window 1
  return (item 1 of p as text) & "," & (item 2 of p as text) & "," & (item 2 of s as text)
end tell
APPLESCRIPT
REMOTE_ACP_GEOMETRY
  )
  IFS=, read -r window_x window_y window_height <<<"$geometry"
  settings_x=$((window_x + 120))
  settings_y=$((window_y + window_height - 20))
  providers_x=$((window_x + 110))
  providers_y=$((window_y + 104))
  add_agent_x=$((window_x + 860))
  add_agent_y=$((window_y + 84))
  "${ssh_cmd[@]}" "$remote" /usr/bin/swift - "$settings_x" "$settings_y" "$providers_x" "$providers_y" "$add_agent_x" "$add_agent_y" <<'REMOTE_CLICK_ACP'
import CoreGraphics
import Darwin
let a = CommandLine.arguments
func click(_ x: String, _ y: String) {
    let point = CGPoint(x: Double(x)!, y: Double(y)!)
    CGWarpMouseCursorPosition(point)
    usleep(120_000)
    CGEvent(mouseEventSource: nil, mouseType: .leftMouseDown, mouseCursorPosition: point, mouseButton: .left)?.post(tap: .cghidEventTap)
    usleep(100_000)
    CGEvent(mouseEventSource: nil, mouseType: .leftMouseUp, mouseCursorPosition: point, mouseButton: .left)?.post(tap: .cghidEventTap)
    usleep(700_000)
}
click(a[1], a[2])
click(a[3], a[4])
click(a[5], a[6])
REMOTE_CLICK_ACP
  sleep 2
fi

if [[ -n "$seed_dir" && "$open_diff_ui" == false && "$open_acp_marketplace_ui" == false ]]; then
  expanded=$(
    "${ssh_cmd[@]}" "$remote" /bin/zsh -s <<'REMOTE_EXPAND_WORKLOG'
osascript <<'APPLESCRIPT'
tell application "System Events"
  tell process "tcode"
    set frontmost to true
  end tell
end tell
APPLESCRIPT
/usr/bin/swift -e '
import CoreGraphics
import Darwin
func click(_ point: CGPoint) {
    CGWarpMouseCursorPosition(point)
    usleep(100_000)
    CGEvent(mouseEventSource: nil, mouseType: .leftMouseDown, mouseCursorPosition: point, mouseButton: .left)?.post(tap: .cghidEventTap)
    usleep(100_000)
    CGEvent(mouseEventSource: nil, mouseType: .leftMouseUp, mouseCursorPosition: point, mouseButton: .left)?.post(tap: .cghidEventTap)
}
click(CGPoint(x: 370, y: 200))
usleep(500_000)
CGEvent(scrollWheelEvent2Source: nil, units: .pixel, wheelCount: 1, wheel1: 80, wheel2: 0, wheel3: 0)?.post(tap: .cghidEventTap)
' >/dev/null
echo expanded
REMOTE_EXPAND_WORKLOG
  )
  expanded=$(echo "$expanded" | tr -d '[:space:]')
  [[ "$expanded" == "expanded" ]] || {
    echo "Could not expand the seeded session Work Log (result: ${expanded:-unknown})" >&2
    exit 1
  }
  sleep 1
fi

# LaunchAgents can trigger a one-time macOS background-activity banner. Dismiss
# it when present so it does not cover the application under test.
"${ssh_cmd[@]}" "$remote" /bin/zsh -s <<'REMOTE_DISMISS_NOTIFICATION'
osascript <<'APPLESCRIPT' >/dev/null 2>&1 || true
tell application "System Events"
  tell process "NotificationCenter"
    if exists window "Notification Center" then
      try
        repeat while exists window "Notification Center"
          set notificationGroup to group 1 of group 1 of scroll area 1 of group 1 of group 1 of window "Notification Center"
          perform action 2 of notificationGroup
          delay 0.2
        end repeat
      end try
    end if
  end tell
end tell
APPLESCRIPT
REMOTE_DISMISS_NOTIFICATION
sleep 0.8

mode=${appearance#--}
[[ -n "$mode" ]] || mode=current
timestamp=$(date -u +%Y%m%dT%H%M%SZ)
output="$out_dir/tcode-${mode}-${timestamp}-$$.png"

"${ssh_cmd[@]}" "$remote" "/usr/sbin/screencapture -x '$remote_shot' && test -s '$remote_shot'"
if [[ "$crop_window" == true ]]; then
  crop_geometry=$(
    "${ssh_cmd[@]}" "$remote" /bin/zsh -s <<'REMOTE_CROP_GEOMETRY'
osascript <<'APPLESCRIPT'
tell application "System Events" to tell process "tcode"
  set p to position of window 1
  set s to size of window 1
end tell
tell application "Finder" to set desktopBounds to bounds of window of desktop
return (item 1 of p as text) & "," & (item 2 of p as text) & "," & (item 1 of s as text) & "," & (item 2 of s as text) & "," & (item 3 of desktopBounds as text) & "," & (item 4 of desktopBounds as text)
APPLESCRIPT
REMOTE_CROP_GEOMETRY
  )
  IFS=, read -r crop_x crop_y crop_width crop_height desktop_width desktop_height <<<"$crop_geometry"
  read -r pixel_width pixel_height < <(
    "${ssh_cmd[@]}" "$remote" "/usr/bin/sips -g pixelWidth -g pixelHeight '$remote_shot'" |
      awk '/pixelWidth:/ { w=$2 } /pixelHeight:/ { h=$2 } END { print w, h }'
  )
  scale_x=$((pixel_width / desktop_width))
  scale_y=$((pixel_height / desktop_height))
  [[ "$scale_x" -ge 1 && "$scale_x" == "$scale_y" ]] || {
    echo "Could not determine a uniform backing scale (image ${pixel_width}x${pixel_height}, desktop ${desktop_width}x${desktop_height})" >&2
    exit 1
  }
  crop_x=$((crop_x * scale_x))
  crop_y=$((crop_y * scale_y))
  crop_width=$((crop_width * scale_x))
  crop_height=$((crop_height * scale_y))
  "${ssh_cmd[@]}" "$remote" "/usr/bin/sips -c '$crop_height' '$crop_width' --cropOffset '$crop_y' '$crop_x' '$remote_shot' >/dev/null"
fi
"${scp_cmd[@]}" "$remote:$remote_shot" "$output"

if ! file "$output" | grep -q 'PNG image data'; then
  echo "Captured file is not a PNG: $output" >&2
  exit 1
fi

echo "Captured $output ($window_count tcode window(s))"
