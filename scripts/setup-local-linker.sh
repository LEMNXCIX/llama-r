#!/usr/bin/env bash
# Local linker shim for environments without a full gcc package.
# Creates ~/.local/bin/cc and library symlinks so rustc/cargo can link.
set -euo pipefail

export HOME="${HOME:-/home/leonardo}"
# Fix Windows-injected HOME when running under some WSL bridges
if [[ "$HOME" != /* ]]; then
  HOME=/home/leonardo
  export HOME
fi

LINKDIR="$HOME/.local/rust-link-libs"
BINDIR="$HOME/.local/bin"
mkdir -p "$LINKDIR" "$BINDIR"

ln -sfn /usr/lib/libgcc_s.so.1 "$LINKDIR/libgcc_s.so"
ln -sfn /usr/lib/libdl.so.2 "$LINKDIR/libdl.so"
ln -sfn /usr/lib/libutil.so.1 "$LINKDIR/libutil.so"
ln -sfn /usr/lib/librt.so.1 "$LINKDIR/librt.so"
ln -sfn /usr/lib/libpthread.so.0 "$LINKDIR/libpthread.so"

cat > "$BINDIR/cc" <<'EOF'
#!/usr/bin/env bash
exec /usr/bin/ld.bfd \
  -L/home/leonardo/.local/rust-link-libs \
  -L/usr/lib \
  -L/lib \
  "$@"
EOF
chmod +x "$BINDIR/cc"

echo "Local linker ready: $BINDIR/cc"
echo "Add to PATH: export PATH=\"$BINDIR:\$PATH\""
