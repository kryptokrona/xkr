#!/usr/bin/env bash
# Checks that every ELF file under the given paths runs on glibc <max> (default 2.35, Ubuntu
# 22.04) and libstdc++ GLIBCXX <max-cxx> (default 3.4.30, the one of Ubuntu 22.04's GCC 12).
#
#   packaging/scripts/check-glibc.sh [--max 2.35] [--max-cxx 3.4.30] <file or dir>...
#
# Reads the version needs of each file (readelf -V); a need marked WEAK is skipped, because the
# loader accepts a missing weak version (Rust's standard library looks newer functions up at run
# time). Prints the highest need of each file and exits 1 when any file needs more.
set -euo pipefail

max=2.35 max_cxx=3.4.30 paths=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --max) max=${2:?}; shift 2 ;;
    --max-cxx) max_cxx=${2:?}; shift 2 ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) paths+=("$1"); shift ;;
  esac
done
[[ ${#paths[@]} -gt 0 ]] || { echo "check-glibc: name a file or directory" >&2; exit 2; }
command -v readelf >/dev/null || { echo "check-glibc: readelf not found (binutils)" >&2; exit 2; }

# newer A B: true when version A is above B.
newer() { [[ "$1" != "$2" && "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -n 1)" = "$1" ]]; }

bad=0 count=0
while IFS= read -r -d '' f; do
  [[ "$(head -c 4 "$f" | od -An -tx1 | tr -d ' \n')" = 7f454c46 ]] || continue
  count=$((count + 1))
  needs=$(readelf -V --wide "$f" 2>/dev/null | sed -n 's/.*Name: \(GLIBC\(XX\)\?_[0-9.]*\) *Flags: none.*/\1/p' | sort -u)
  top_c=$(printf '%s\n' "$needs" | sed -n 's/^GLIBC_//p' | sort -V | tail -n 1)
  top_cxx=$(printf '%s\n' "$needs" | sed -n 's/^GLIBCXX_//p' | sort -V | tail -n 1)
  problem=""
  if [[ -n "$top_c" ]] && newer "$top_c" "$max"; then problem="  needs glibc $top_c > $max"; fi
  if [[ -n "$top_cxx" ]] && newer "$top_cxx" "$max_cxx"; then problem+="  needs GLIBCXX $top_cxx > $max_cxx"; fi
  printf '%-8s %-9s %s%s\n' "${top_c:--}" "${top_cxx:--}" "$f" "$problem"
  [[ -z "$problem" ]] || bad=1
done < <(find "${paths[@]}" -type f -print0 | LC_ALL=C sort -z)

[[ $count -gt 0 ]] || { echo "check-glibc: no ELF files under ${paths[*]}" >&2; exit 2; }
if [[ $bad = 1 ]]; then
  echo "check-glibc: some files need a newer glibc than $max (build them on Ubuntu 22.04)" >&2
  exit 1
fi
echo "check-glibc: $count ELF files run on glibc $max"
