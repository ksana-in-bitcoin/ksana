#!/bin/sh
# The rustc of the playground's builds: lib/build.mjs runs cargo with RUSTC set to this script and KSANA_RUSTC to the
# toolchain's rustc. Cargo hashes `rustc -vV`, its host line included, into the metadata of build scripts and proc
# macros, and through them into the crates built for wasm32, so a module's symbol names, and with them the order of its
# functions, would follow the architecture of the machine that builds it (D-119). Cargo is told one host on every
# machine instead. What runs on the machine is still built for it: cargo passes no --target for build scripts and proc
# macros and asks rustc for their cfg itself, and a build script's probe for that host (proc-macro2's compiles with
# --target $TARGET) gets the machine's own.
host=x86_64-unknown-linux-gnu

if [ "$#" -eq 1 ] && [ "$1" = -vV ]; then
  info=$("$KSANA_RUSTC" -vV) || exit
  printf '%s\n' "$info" | sed "s/^host: .*/host: $host/"
  exit
fi

case " $* " in
*" $host "* | *"=$host "*)
  own=$("$KSANA_RUSTC" -vV | sed -n 's/^host: //p')
  previous=
  for arg do
    shift
    if [ "$previous" = --target ] && [ "$arg" = "$host" ]; then
      set -- "$@" "$own"
    elif [ "$arg" = "--target=$host" ]; then
      set -- "$@" "--target=$own"
    else
      set -- "$@" "$arg"
    fi
    previous=$arg
  done
  ;;
esac

# On macOS the toolchain's rust-lld finds libLLVM.dylib only through DYLD_FALLBACK_LIBRARY_PATH: its rpaths name
# lib/rustlib/<host>/lib and the directory the toolchain was built in. Rustup's proxy sets the variable for cargo, but
# macOS drops every DYLD_ variable when it starts this script's /bin/sh, so the toolchain's lib directory is named again
# for the rustc that runs the linker, followed by dyld's own fallbacks as rustup adds them.
if [ "$(uname -s)" = Darwin ]; then
  DYLD_FALLBACK_LIBRARY_PATH="${KSANA_RUSTC%/*}/../lib:${DYLD_FALLBACK_LIBRARY_PATH:-$HOME/lib:/usr/local/lib:/usr/lib}"
  export DYLD_FALLBACK_LIBRARY_PATH
fi
exec "$KSANA_RUSTC" "$@"
