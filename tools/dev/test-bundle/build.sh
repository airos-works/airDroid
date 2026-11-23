#!/bin/bash
# Builds a small app as an Android App Bundle and from it the three kinds of
# bundle airDroid installs: split-test.apks (bundletool's layout),
# split-test.apkm (APKMirror's) and split-test.xapk (APKPure's, with an
# expansion file). The app has code for x86_64, x86 and arm64-v8a, icons in
# four densities and strings in three languages, so that every kind of
# configuration split is there.
#
#   tools/dev/test-bundle/build.sh TOOLS OUT
#
# TOOLS holds jre/ (a Java 17 runtime), bundletool.jar, aapt2 and
# android.jar (a platform's, for aapt2 link).
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
TOOLS=$(cd "$1" && pwd)
mkdir -p "$2"
OUT=$(cd "$2" && pwd)
W="$OUT/work"
rm -rf "$W"
mkdir -p "$W"/{res/values,res/values-de,res/values-fr,module/manifest,module/lib/x86_64,module/lib/x86,module/lib/arm64-v8a}
JAVA="$TOOLS/jre/bin/java"

for lang in "" -de -fr; do
	case "$lang" in "") name="Split Test";; -de) name="Split-Probe";; -fr) name="Essai de split";; esac
	printf '<?xml version="1.0" encoding="utf-8"?>\n<resources><string name="app_name">%s</string></resources>\n' "$name" \
		> "$W/res/values$lang/strings.xml"
done
python3 -I - "$W/res" <<'PY'
import struct, sys, zlib, os
def png(path, size, rgb):
    row = b"\0" + bytes(rgb) * size
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    os.makedirs(os.path.dirname(path), exist_ok=True)
    open(path, "wb").write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(row * size)) + chunk(b"IEND", b""))
for name, size, rgb in (("mdpi", 48, (200, 60, 60)), ("hdpi", 72, (60, 200, 60)), ("xhdpi", 96, (60, 60, 200)), ("xxhdpi", 144, (200, 200, 60))):
    png(f"{sys.argv[1]}/drawable-{name}/icon.png", size, rgb)
PY

# no C library: the same file serves any Linux of that processor type
gcc -shared -fPIC -nostdlib -O2 -Wl,--hash-style=both -Wl,-soname,libmain.so -Wl,-z,max-page-size=4096 \
	-o "$W/module/lib/x86_64/libmain.so" "$HERE/main.c"
# stand-ins: only their names matter to the test
cp "$W/module/lib/x86_64/libmain.so" "$W/module/lib/x86/libmain.so"
cp "$W/module/lib/x86_64/libmain.so" "$W/module/lib/arm64-v8a/libmain.so"

"$TOOLS/aapt2" compile --dir "$W/res" -o "$W/res.zip"
"$TOOLS/aapt2" link --proto-format -o "$W/linked.zip" -I "$TOOLS/android.jar" \
	--manifest "$HERE/AndroidManifest.xml" -R "$W/res.zip" --auto-add-overlay
(cd "$W/module" && unzip -q ../linked.zip && mv AndroidManifest.xml manifest/ && zip -q -r ../base.zip .)

[ -f "$OUT/test.keystore" ] || "$TOOLS/jre/bin/keytool" -genkeypair -keystore "$OUT/test.keystore" -storepass android \
	-keypass android -alias test -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=airDroid split test" 2>/dev/null
"$JAVA" -jar "$TOOLS/bundletool.jar" build-bundle --modules="$W/base.zip" --output="$W/app.aab"
rm -f "$OUT/split-test.apks"
"$JAVA" -jar "$TOOLS/bundletool.jar" build-apks --bundle="$W/app.aab" --output="$OUT/split-test.apks" \
	--aapt2="$TOOLS/aapt2" --ks="$OUT/test.keystore" --ks-pass=pass:android --ks-key-alias=test --key-pass=pass:android

# the same APKs under the other two stores' names
mkdir -p "$W/apks" "$W/apkm" "$W/xapk/Android/obb/works.airos.splittest"
unzip -q "$OUT/split-test.apks" -d "$W/apks"
for apk in "$W"/apks/splits/base-*.apk; do
	q=$(basename "$apk" .apk); q=${q#base-}
	if [ "$q" = master ]; then
		cp "$apk" "$W/apkm/base.apk"
		cp "$apk" "$W/xapk/works.airos.splittest.apk"
	else
		cp "$apk" "$W/apkm/split_config.$q.apk"
		cp "$apk" "$W/xapk/config.$q.apk"
	fi
done
echo '{"apkm_version": 1, "pname": "works.airos.splittest", "versioncode": "1"}' > "$W/apkm/info.json"
echo '{"xapk_version": 2, "package_name": "works.airos.splittest", "version_code": "1"}' > "$W/xapk/manifest.json"
echo "expansion file of the split test" > "$W/xapk/Android/obb/works.airos.splittest/main.1.works.airos.splittest.obb"
rm -f "$OUT/split-test.apkm" "$OUT/split-test.xapk"
(cd "$W/apkm" && zip -q -0 -r "$OUT/split-test.apkm" .)
(cd "$W/xapk" && zip -q -0 -r "$OUT/split-test.xapk" .)
ls -la "$OUT"/split-test.*
