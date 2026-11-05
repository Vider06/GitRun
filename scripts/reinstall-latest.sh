#!/bin/sh
set -eu

binary="${1:-}"
if [ -z "$binary" ] || [ ! -x "$binary" ] || [ -L "$binary" ]; then
  echo "GitRun executable not found or is not a regular executable." >&2
  exit 1
fi

canonical="$(readlink -f "$binary")"
case "$canonical" in
  /usr/bin/gitrun|/usr/local/bin/gitrun) ;;
  *)
    echo "Refusing privilege elevation for a GitRun binary outside trusted system install paths." >&2
    exit 1
    ;;
esac
if [ ! -f "$canonical" ] || [ "$(stat -c %u "$canonical")" != 0 ]; then
  echo "Refusing privilege elevation: GitRun binary is not root-owned." >&2
  exit 1
fi
mode="$(stat -c %a "$canonical")"
case "$mode" in
  *[2367][0-7]|*[0-7][2367])
    echo "Refusing privilege elevation: GitRun binary is writable by group/others." >&2
    exit 1
    ;;
esac

if [ "$(id -u)" -eq 0 ]; then
  exec "$canonical" --no-cat --reinstall-latest-root
fi

trusted_helper() {
  helper="$1"
  [ -x "$helper" ] || return 1
  [ "$(stat -c %u "$helper" 2>/dev/null)" = 0 ] || return 1
  helper_mode="$(stat -c %a "$helper" 2>/dev/null)" || return 1
  case "$helper_mode" in
    *[2367][0-7]|*[0-7][2367]) return 1 ;;
  esac
}

for helper in /usr/bin/sudo /usr/bin/doas; do
  if [ -t 0 ] && trusted_helper "$helper"; then
    exec "$helper" "$canonical" --no-cat --reinstall-latest-root
  fi
done

if trusted_helper /usr/bin/pkexec; then
  exec /usr/bin/pkexec "$canonical" --no-cat --reinstall-latest-root
fi

for helper in /usr/bin/sudo /usr/bin/doas; do
  if trusted_helper "$helper"; then
    echo "GitRun needs administrator access; $helper will request authorization." >&2
    exec "$helper" "$canonical" --no-cat --reinstall-latest-root
  fi
done

echo "GitRun reinstall needs administrator access. Install a trusted sudo, doas or pkexec helper and retry." >&2
exit 1
