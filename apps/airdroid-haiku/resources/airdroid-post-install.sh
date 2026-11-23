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
# A file's contents count before its name, and both are zips: without these
# rules (the app sets them at every start too) they are zip archives to
# Tracker until airDroid has run once.
setmime -set "$type" -sniffRule '0.60 [0] ("PK\003\004") [30] ("AndroidManifest.xml" | "META-INF/com/android/build/" | "classes.dex" | "resources.arsc")'
setmime -set "$bundle" -sniffRule '0.55 [0] ("PK\003\004") ([60:8192] "META-INF/APKMIRRO" | [30:130] ".apk" | [30] "split_config." | [30] "toc.pb" | [30] "splits/base-" | [30] "meta.sai_v" | [30] "Android/obb/")'
# Files that were on the disk before these rules keep the type they were
# given then (a zip or a Java archive), and a double click on them does not
# reach airDroid. Those named like an app are identified again; a type that
# somebody chose for a file is left alone.
query -a -e -f '((name=="*.apkm")||(name=="*.xapk")||(name=="*.apks")||(name=="*.apk"))' 2>/dev/null \
| while IFS= read -r file; do
	case "$(catattr -d BEOS:TYPE "$file" 2>/dev/null | tr -d '\0')" in
	application/zip|application/java-archive|application/octet-stream|"")
		mimeset -F "$file" 2>/dev/null ;;
	esac
done
exit 0
