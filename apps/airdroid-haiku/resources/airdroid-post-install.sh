#!/bin/sh
# airDroid's post-install script (boot/post-install/airdroid.sh).
# Registers the app with the MIME database and the Android package type
# (.apk files), with airDroid as its preferred application unless the user
# chose another one.
signature=application/x-vnd.airos-airDroid
type=application/vnd.android.package-archive
app=$(findpaths -p "$0" B_FIND_PATH_APPS_DIRECTORY airDroid 2>/dev/null)
[ -n "$app" ] && [ -e "$app" ] && mimeset -a -f "$app"
if ! setmime -dump "$type" >/dev/null 2>&1; then
	setmime -set "$type" -short "Android app" -long "Android application package" \
		-extension apk
fi
setmime -dump "$type" 2>/dev/null | grep -q -- -preferredAppSig \
	|| setmime -set "$type" -preferredAppSig "$signature"
exit 0
