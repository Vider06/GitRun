#!/usr/bin/env bash
set -Eeuo pipefail

binary="${1:-}"
if [[ -z "$binary" || ! -x "$binary" || -L "$binary" ]]; then
  echo "GitRun executable not found or is not a regular executable." >&2
  exit 1
fi

canonical="$(readlink -f -- "$binary")"
case "$canonical" in
  /usr/bin/gitrun|/usr/local/bin/gitrun) ;;
  *)
    echo "Refusing privilege elevation for a GitRun binary outside the trusted system install paths." >&2
    exit 1
    ;;
esac
[[ -f "$canonical" && "$(stat -c %u -- "$canonical")" == 0 ]] || {
  echo "Refusing privilege elevation: GitRun binary is not a root-owned regular file." >&2
  exit 1
}
mode="$(stat -c %a -- "$canonical")"
(( (8#$mode & 022) == 0 )) || {
  echo "Refusing privilege elevation: GitRun binary is writable by group/others." >&2
  exit 1
}

if [[ "$(id -u)" -eq 0 ]]; then
  exec "$canonical" --reinstall-latest-root
fi

if [[ -t 0 && -x /usr/bin/sudo ]]; then
  exec /usr/bin/sudo -- "$canonical" --reinstall-latest-root
fi

if [[ -x /usr/bin/pkexec ]]; then
  exec /usr/bin/pkexec "$canonical" --reinstall-latest-root
fi

if [[ -x /usr/bin/sudo ]]; then
  echo "GitRun needs administrator access; sudo will request authorization." >&2
  exec /usr/bin/sudo -- "$canonical" --reinstall-latest-root
fi

echo "GitRun reinstall needs administrator access. Install sudo or pkexec and retry." >&2
exit 1
