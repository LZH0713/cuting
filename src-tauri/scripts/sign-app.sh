#!/bin/sh
# Sign Cuting.app with the Apple Development certificate.
#
# Run after every `npm run tauri build`:
#   npm run sign
#
# Two things can make codesign reject the bundle with "resource fork, Finder
# information, or similar detritus not allowed":
# - com.apple.FinderInfo on the .app folder itself (Finder adds it while
#   showing the folder), removed here before signing;
# - on macOS 15.0, codesign sometimes cannot sign bundles in
#   File-Provider-managed directories at all. In that case the bundle is
#   copied to a clean directory through a tar pipe (which drops the
#   attributes), signed there, and swapped back in place.
set -e
APP="${1:-src-tauri/target/release/bundle/macos/Cuting.app}"
# codesign matches identities by substring, so a partial name works.
IDENTITY="${CUTING_SIGN_IDENTITY:-Apple Development}"
[ -d "$APP" ] || { echo "no app bundle at $APP (build first)"; exit 1; }

xattr -cr "$APP" 2>/dev/null || true
xattr -d com.apple.FinderInfo "$APP" 2>/dev/null || true
rm -rf "$APP/Contents/_CodeSignature"

if codesign --force --deep --sign "$IDENTITY" "$APP" 2>/dev/null && codesign --verify --strict "$APP"; then
  echo "signed $APP"
  exit 0
fi

# In-place signing failed: sign a clean copy and swap it in.
STAGE=$(mktemp -d "${TMPDIR:-/tmp}/cuting-sign.XXXXXX")
trap 'rm -rf "$STAGE"' EXIT
BASENAME=$(basename "$APP")
(cd "$(dirname "$APP")" && tar -cf - "$BASENAME") | (cd "$STAGE" && tar -xf -)
STAGED_APP="$STAGE/$BASENAME"
xattr -cr "$STAGED_APP" 2>/dev/null || true
xattr -d com.apple.FinderInfo "$STAGED_APP" 2>/dev/null || true
rm -rf "$STAGED_APP/Contents/_CodeSignature"
codesign --force --deep --sign "$IDENTITY" "$STAGED_APP"
codesign --verify --strict "$STAGED_APP"
rm -rf "$APP"
mv "$STAGED_APP" "$APP"
echo "signed $APP (via a clean copy; the original directory could not be signed in place)"
