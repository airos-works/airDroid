#!/bin/sh
# airDroid's post-install script (boot/post-install/airdroid.sh).
# Registers the app with the MIME database and the Android package types
# (.apk files, and the bundles .apkm, .xapk and .apks), with airDroid as
# their preferred application unless the user chose another one.
signature=application/x-vnd.airos-airDroid
type=application/vnd.android.package-archive
app=$(findpaths -p "$0" B_FIND_PATH_APPS_DIRECTORY airDroid 2>/dev/null)
[ -n "$app" ] && [ -e "$app" ] && mimeset -a -f "$app"
# (mimeset has made the types the app supports, with nothing in them: what
# is asked here is whether a type has its extensions, not whether it exists)
if ! setmime -dump "$type" 2>/dev/null | grep -q -- -extension; then
	setmime -set "$type" -short "Android app" -long "Android application package" \
		-extension apk
fi
setmime -dump "$type" 2>/dev/null | grep -q -- -preferredAppSig \
	|| setmime -set "$type" -preferredAppSig "$signature"
# app bundles: an app as its base APK and split APKs in one zip file
bundle=application/x-vnd.android.app-bundle
if ! setmime -dump "$bundle" 2>/dev/null | grep -q -- -extension; then
	setmime -set "$bundle" -short "Android app bundle" \
		-long "Android app with its split packages" \
		-extension apkm -extension xapk -extension apks
fi
setmime -dump "$bundle" 2>/dev/null | grep -q -- -preferredAppSig \
	|| setmime -set "$bundle" -preferredAppSig "$signature"
exit 0
