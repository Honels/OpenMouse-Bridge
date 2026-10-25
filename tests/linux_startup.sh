set -eu
export OPENMOUSE_INSTALL_SOURCED=1
fixture_parent="$1"
set --
source ./install-linux.sh
root=$(mktemp -d "$fixture_parent/bridge-startup.XXXXXXXX")
trap 'rm -rf "$root"' EXIT
executable='/opt/50%/A "$`B\bridge'
expected='Exec="/opt/50%%/A \\"\\$\\`B\\\\bridge"'
write_autostart "$root/autostart.desktop" "$executable"
write_launcher "$root/launcher.desktop" "$executable" '/opt/icon.png'
for entry in "$root/autostart.desktop" "$root/launcher.desktop"; do
    actual=$(while IFS= read -r line; do case "$line" in Exec=*) printf '%s' "$line";; esac; done < "$entry")
    [ "$actual" = "$expected" ] || { printf 'Incorrect Exec entry: %s\n' "$actual" >&2; exit 1; }
done
if write_autostart "$root/invalid.desktop" $'/opt/invalid\nentry'; then exit 1; fi
if write_launcher "$root/invalid.desktop" '/opt/bridge' $'/opt/invalid\nicon'; then exit 1; fi
[ "$(HOME=/home/fixture XDG_CONFIG_HOME=/xdg/config startup_config_home)" = /xdg/config ]
[ "$(HOME=/home/fixture XDG_CONFIG_HOME=relative startup_config_home)" = /home/fixture/.config ]
