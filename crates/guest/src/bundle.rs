//! App bundles: an app as a base APK and split APKs in one zip file, the way
//! stores hand out apps that were published as Android App Bundles.
//!
//! * `.apkm` (APKMirror): `base.apk`, `split_config.arm64_v8a.apk`,
//!   `split_config.en.apk`, `split_config.xxhdpi.apk`, `info.json`
//! * `.xapk` (APKPure): `<package>.apk`, `config.arm64_v8a.apk`, ...,
//!   `manifest.json`, sometimes `Android/obb/<package>/*.obb`
//! * `.apks`: like `.apkm` (SAI), or bundletool's `splits/base-master.apk`,
//!   `splits/base-xxhdpi.apk`, ...
//!
//! AntiSplit-M and its like merge the splits into one APK, which then has to
//! be signed with another key: the app no longer updates from its store and
//! some apps check their signature. Android installs splits as they are, in
//! one session, so that is what is done here: the base, every feature
//! module, and of the configuration splits the ones this device uses.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};

/// What of the device decides which splits it gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// `ro.product.cpu.abilist`, best first: "x86_64", "x86"
    pub abis: Vec<String>,
    /// the display's density (dots per inch)
    pub density: u32,
    /// languages of the user's locales, best first: "en"
    pub languages: Vec<String>,
}

impl Device {
    /// From Android's properties: the ABI list, `ro.sf.lcd_density` (or what
    /// `wm density` says), and the locale.
    pub fn new(abilist: &str, density: &str, locales: &str) -> Device {
        let abis = abilist.split(',').map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect();
        let density = density
            .rsplit(|c: char| !c.is_ascii_digit())
            .find(|s| !s.is_empty())
            .and_then(|s| s.parse().ok())
            .filter(|&d| d > 0)
            .unwrap_or(160);
        let mut languages: Vec<String> = Vec::new();
        for locale in locales.split(',') {
            let lang = locale.trim().split(['-', '_']).next().unwrap_or("").to_ascii_lowercase();
            if !lang.is_empty() && !languages.contains(&lang) {
                languages.push(lang);
            }
        }
        if languages.is_empty() {
            languages.push("en".into());
        }
        Device { abis, density, languages }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// the module's own code and resources
    Master,
    Abi(String),
    Density(u32),
    Language(String),
    /// a configuration this does not know: installed, to be safe
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Split {
    /// "base", or a feature module's name
    module: String,
    kind: Kind,
}

const ABIS: [&str; 8] = ["armeabi", "armeabi_v7a", "arm64_v8a", "x86", "x86_64", "mips", "mips64", "riscv64"];
const DENSITIES: [(&str, u32); 7] =
    [("ldpi", 120), ("mdpi", 160), ("tvdpi", 213), ("hdpi", 240), ("xhdpi", 320), ("xxhdpi", 480), ("xxxhdpi", 640)];

fn qualifier(q: &str) -> Kind {
    let norm = q.replace('-', "_");
    if ABIS.contains(&norm.as_str()) {
        return Kind::Abi(norm);
    }
    if let Some((_, dpi)) = DENSITIES.iter().find(|(name, _)| *name == q) {
        return Kind::Density(*dpi);
    }
    // "en", "pt_BR", "zh-CN", "fil"; not "nodpi", "anydpi" or the like
    let lang = q.split(['-', '_']).next().unwrap_or("");
    if (2..=3).contains(&lang.len()) && lang.bytes().all(|b| b.is_ascii_lowercase()) && !q.ends_with("dpi") {
        return Kind::Language(lang.to_string());
    }
    Kind::Other
}

/// What the APK at `path` of a bundle is, by its name; `None` for what is
/// not a split to install (bundletool's standalone APKs, for devices
/// without split support).
fn classify(path: &str) -> Option<Split> {
    let (dir, file) = match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    };
    let stem = file.strip_suffix(".apk")?;
    if dir.split('/').any(|d| d == "standalones") || dir.starts_with("Android/") {
        return None;
    }
    // bundletool: "splits/base-master.apk", "splits/maps-xxhdpi.apk"
    if dir.split('/').any(|d| d == "splits") {
        if let Some(i) = stem.rfind('-') {
            let (module, q) = (&stem[..i], &stem[i + 1..]);
            let kind = if q == "master" { Kind::Master } else { qualifier(q) };
            return Some(Split { module: module.to_string(), kind });
        }
    }
    let stem = stem.strip_prefix("split_").unwrap_or(stem);
    if stem == "base" {
        return Some(Split { module: "base".into(), kind: Kind::Master });
    }
    if let Some(q) = stem.strip_prefix("config.") {
        return Some(Split { module: "base".into(), kind: qualifier(q) });
    }
    if let Some(i) = stem.find(".config.") {
        return Some(Split { module: stem[..i].to_string(), kind: qualifier(&stem[i + ".config.".len()..]) });
    }
    Some(Split { module: stem.to_string(), kind: Kind::Master })
}

fn abi_name(abi: &str) -> String {
    abi.replace("_v", "-v")
}

/// The APKs of a bundle to install on `device`, the base first: every
/// module's own APK, and of each module's configuration splits the one for
/// the best processor type the device runs, the one for the nearest screen
/// density and those for the user's languages.
///
/// An error if the bundle has code for other processors only: Android
/// would install it and the app could not start.
pub fn choose(apks: &[String], device: &Device) -> Result<Vec<String>, String> {
    let splits: Vec<(&String, Split)> = apks.iter().filter_map(|p| classify(p).map(|s| (p, s))).collect();
    if splits.is_empty() {
        return Err("There is no app (.apk) in this file.".into());
    }
    let mut modules: Vec<&str> = Vec::new();
    for (_, s) in &splits {
        if !modules.contains(&s.module.as_str()) {
            modules.push(&s.module);
        }
    }
    // the base first: "base", or else the module without a split to its name
    modules.sort_by_key(|m| *m != "base");

    let device_abis: Vec<String> = device.abis.iter().map(|a| a.replace('-', "_")).collect();
    let mut masters = Vec::new();
    let mut configs = Vec::new();
    for module in modules {
        let of = |want: fn(&Kind) -> bool| splits.iter().filter(move |(_, s)| s.module == module && want(&s.kind));
        masters.extend(of(|k| *k == Kind::Master).map(|(p, _)| (*p).clone()));

        let abis: Vec<(&String, &str)> = of(|k| matches!(k, Kind::Abi(_)))
            .map(|(p, s)| (*p, if let Kind::Abi(a) = &s.kind { a.as_str() } else { "" }))
            .collect();
        if !abis.is_empty() {
            let best = device_abis.iter().find_map(|d| abis.iter().find(|(_, a)| a == d));
            match best {
                Some((path, _)) => configs.push((*path).clone()),
                None => {
                    let mut has: Vec<String> = abis.iter().map(|(_, a)| abi_name(a)).collect();
                    has.sort();
                    return Err(format!(
                        "This app has no code for this computer's processor ({}): it is built for {} only.",
                        device.abis.first().map(String::as_str).unwrap_or("?"),
                        has.join(", ")
                    ));
                }
            }
        }

        let mut densities: Vec<(&String, u32)> = of(|k| matches!(k, Kind::Density(_)))
            .map(|(p, s)| (*p, if let Kind::Density(d) = s.kind { d } else { 0 }))
            .collect();
        densities.sort_by_key(|(_, d)| *d);
        // the smallest that is enough, or else the largest there is
        if let Some((path, _)) = densities.iter().find(|(_, d)| *d >= device.density).or(densities.last()) {
            configs.push((*path).clone());
        }

        for (path, s) in of(|k| matches!(k, Kind::Language(_))) {
            if let Kind::Language(lang) = &s.kind {
                if device.languages.contains(lang) {
                    configs.push((*path).clone());
                }
            }
        }
        configs.extend(of(|k| *k == Kind::Other).map(|(p, _)| (*p).clone()));
    }
    if masters.is_empty() {
        return Err("This file has parts of an app but not the app itself (no base APK).".into());
    }
    masters.extend(configs);
    Ok(masters)
}

/// Whether a name from a bundle may be handed to `unzip` to unpack under a
/// directory: it stays in there, and is not taken for an option.
fn safe(name: &str) -> bool {
    !name.starts_with(['/', '-'])
        && !name.split('/').any(|part| part == ".." || part.is_empty())
        && !name.chars().any(|c| c.is_control() || c == '\\')
}

/// The names of a bundle's expansion files ("Android/obb/<package>/..."),
/// which go to Android's shared storage as they are.
pub fn expansion_files(entries: &[String]) -> Vec<String> {
    entries.iter().filter(|e| e.starts_with("Android/obb/") && !e.ends_with('/') && safe(e)).cloned().collect()
}

/// The apps (package names) the expansion files belong to.
pub fn expansion_packages(files: &[String]) -> Vec<String> {
    let mut packages: Vec<String> = Vec::new();
    for file in files {
        let package = file.strip_prefix("Android/obb/").and_then(|rest| rest.split('/').next()).unwrap_or("");
        if !package.is_empty() && !packages.iter().any(|p| p == package) {
            packages.push(package.to_string());
        }
    }
    packages
}

fn u16_at(b: &[u8], at: usize) -> usize {
    u16::from_le_bytes([b[at], b[at + 1]]) as usize
}

fn u32_at(b: &[u8], at: usize) -> u64 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as u64
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

/// The names of the files in a zip archive, from its central directory.
pub fn zip_entries(file: &mut File) -> io::Result<Vec<String>> {
    let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());
    let size = file.seek(SeekFrom::End(0))?;
    // the end record is the last thing in the file, before a comment of up
    // to 64 KiB
    let tail_len = size.min(22 + 65535);
    let mut tail = vec![0u8; tail_len as usize];
    file.seek(SeekFrom::Start(size - tail_len))?;
    file.read_exact(&mut tail)?;
    let end = (0..=tail.len().saturating_sub(22))
        .rev()
        // (its comment may hold the signature too: the real record is the
        // one whose comment ends the file)
        .find(|&i| tail[i..i + 4] == [0x50, 0x4b, 0x05, 0x06] && i + 22 + u16_at(&tail, i + 20) == tail.len())
        .ok_or_else(|| bad("not a zip file"))?;
    let mut count = u16_at(&tail, end + 10) as u64;
    let mut dir_size = u32_at(&tail, end + 12);
    let mut dir_offset = u32_at(&tail, end + 16);
    if count == 0xffff || dir_size == 0xffff_ffff || dir_offset == 0xffff_ffff {
        // zip64: a locator in front of the end record points at the real one
        let locator = end.checked_sub(20).filter(|&l| tail[l..l + 4] == [0x50, 0x4b, 0x06, 0x07]);
        let at = u64_at(&tail, locator.ok_or_else(|| bad("zip64 without a locator"))? + 8);
        let mut record = [0u8; 56];
        file.seek(SeekFrom::Start(at))?;
        file.read_exact(&mut record)?;
        if record[..4] != [0x50, 0x4b, 0x06, 0x06] {
            return Err(bad("zip64 end record not found"));
        }
        count = u64_at(&record, 32);
        dir_size = u64_at(&record, 40);
        dir_offset = u64_at(&record, 48);
    }
    if dir_offset.checked_add(dir_size).is_none_or(|e| e > size) || dir_size > 256 << 20 {
        return Err(bad("zip directory out of bounds"));
    }
    let mut dir = vec![0u8; dir_size as usize];
    file.seek(SeekFrom::Start(dir_offset))?;
    file.read_exact(&mut dir)?;
    let mut names = Vec::new();
    let mut at = 0usize;
    for _ in 0..count {
        if at + 46 > dir.len() || dir[at..at + 4] != [0x50, 0x4b, 0x01, 0x02] {
            return Err(bad("zip directory damaged"));
        }
        let (name, extra, comment) = (u16_at(&dir, at + 28), u16_at(&dir, at + 30), u16_at(&dir, at + 32));
        if at + 46 + name > dir.len() {
            return Err(bad("zip directory damaged"));
        }
        names.push(String::from_utf8_lossy(&dir[at + 46..at + 46 + name]).into_owned());
        at += 46 + name + extra + comment;
    }
    Ok(names)
}

/// What a file to install is, from its contents.
#[derive(Debug, PartialEq, Eq)]
pub enum Package {
    /// an APK: installed as it is
    Apk,
    /// a bundle, with its APKs and all of its entries
    Bundle { apks: Vec<String>, entries: Vec<String> },
}

/// Looks into the file at `path`. An APK has its manifest at the top; a zip
/// without one and with APKs in it is a bundle.
pub fn inspect(path: &str) -> Result<Package, String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut magic = [0u8; 4];
    let read = file.read(&mut magic).map_err(|e| e.to_string())?;
    if read < 4 || magic[..2] != *b"PK" {
        return Err(if path.ends_with(".apkm") {
            "This .apkm file is not a zip file. It may be in APKMirror's old encrypted format: a newly downloaded one is a plain bundle.".into()
        } else {
            "This is not an Android app (neither an APK nor a bundle of APKs).".to_string()
        });
    }
    let entries = zip_entries(&mut file).map_err(|e| e.to_string())?;
    if entries.iter().any(|e| e == "AndroidManifest.xml") {
        return Ok(Package::Apk);
    }
    let apks: Vec<String> = entries.iter().filter(|e| e.ends_with(".apk") && safe(e) && classify(e).is_some()).cloned().collect();
    if apks.is_empty() {
        return Err("There is no app (.apk) in this file.".into());
    }
    Ok(Package::Bundle { apks, entries })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn pc() -> Device {
        Device::new("x86_64,x86", "240", "en-US")
    }

    #[test]
    fn device_from_properties() {
        assert_eq!(pc(), Device { abis: names(&["x86_64", "x86"]), density: 240, languages: names(&["en"]) });
        // `wm density`: the override counts, it comes last
        let d = Device::new("x86_64,x86,arm64-v8a", "Physical density: 160\nOverride density: 213", "pt-BR,en-US,pt-PT");
        assert_eq!((d.density, d.languages), (213, names(&["pt", "en"])));
        assert_eq!(Device::new("", "", "").density, 160);
        assert_eq!(Device::new("", "", "").languages, names(&["en"]));
    }

    #[test]
    fn apkm_gets_the_base_and_this_devices_splits() {
        let apkm = names(&[
            "base.apk",
            "split_config.arm64_v8a.apk",
            "split_config.x86_64.apk",
            "split_config.x86.apk",
            "split_config.en.apk",
            "split_config.de.apk",
            "split_config.mdpi.apk",
            "split_config.hdpi.apk",
            "split_config.xxhdpi.apk",
        ]);
        assert_eq!(
            choose(&apkm, &pc()).unwrap(),
            names(&["base.apk", "split_config.x86_64.apk", "split_config.hdpi.apk", "split_config.en.apk"])
        );
        // the second best processor type, a density above the largest split,
        // a language without a split
        let old = Device::new("x86", "640", "fr-FR");
        assert_eq!(choose(&apkm, &old).unwrap(), names(&["base.apk", "split_config.x86.apk", "split_config.xxhdpi.apk"]));
    }

    #[test]
    fn xapk_and_feature_modules() {
        let xapk = names(&[
            "manifest.json",
            "icon.png",
            "org.example.app.apk",
            "config.x86_64.apk",
            "config.arm64_v8a.apk",
            "config.xhdpi.apk",
            "config.pt.apk",
            "split_maps.apk",
            "split_maps.config.x86_64.apk",
            "split_maps.config.arm64_v8a.apk",
            "Android/obb/org.example.app/main.12.org.example.app.obb",
        ]);
        let device = Device::new("x86_64,x86", "320", "pt-BR,en-US");
        assert_eq!(
            choose(&xapk.iter().filter(|n| n.ends_with(".apk")).cloned().collect::<Vec<_>>(), &device).unwrap(),
            names(&[
                "org.example.app.apk",
                "split_maps.apk",
                "config.x86_64.apk",
                "config.xhdpi.apk",
                "config.pt.apk",
                "split_maps.config.x86_64.apk"
            ])
        );
        assert_eq!(expansion_files(&xapk), names(&["Android/obb/org.example.app/main.12.org.example.app.obb"]));
        assert_eq!(expansion_packages(&expansion_files(&xapk)), names(&["org.example.app"]));
    }

    #[test]
    fn names_that_leave_the_directory_are_left_out() {
        let bad = names(&[
            "Android/obb/../../../data/system/packages.xml",
            "Android/obb/org.example.app/",
            "Android/obb//x.obb",
            "Android/obb/org.example.app/patch.3.obb",
        ]);
        assert_eq!(expansion_files(&bad), names(&["Android/obb/org.example.app/patch.3.obb"]));
        for name in ["../base.apk", "/data/base.apk", "-d.apk", "a/../../b.apk", "a\\b.apk", "a\nb.apk"] {
            assert!(!safe(name), "{name:?}");
        }
        assert!(safe("splits/base-master.apk") && safe("split_config.b+sr+Latn.apk"));
    }

    #[test]
    fn bundletool_layout() {
        let apks = names(&[
            "toc.pb",
            "splits/base-master.apk",
            "splits/base-x86_64.apk",
            "splits/base-armeabi_v7a.apk",
            "splits/base-xxhdpi.apk",
            "splits/base-en.apk",
            "splits/base-ja.apk",
            "splits/extras-master.apk",
            "standalones/standalone-x86_64_hdpi.apk",
        ]);
        let list: Vec<String> = apks.iter().filter(|n| n.ends_with(".apk")).cloned().collect();
        assert_eq!(
            choose(&list, &pc()).unwrap(),
            names(&[
                "splits/base-master.apk",
                "splits/extras-master.apk",
                "splits/base-x86_64.apk",
                "splits/base-xxhdpi.apk",
                "splits/base-en.apk"
            ])
        );
    }

    #[test]
    fn code_for_other_processors_only_is_refused() {
        let arm = names(&["base.apk", "split_config.arm64_v8a.apk", "split_config.armeabi_v7a.apk", "split_config.en.apk"]);
        let e = choose(&arm, &pc()).unwrap_err();
        assert!(e.contains("x86_64") && e.contains("arm64-v8a, armeabi-v7a"), "{e}");
        // with a translator for ARM code the device lists those too
        let translated = Device::new("x86_64,x86,arm64-v8a,armeabi-v7a", "240", "en-US");
        assert_eq!(choose(&arm, &translated).unwrap(), names(&["base.apk", "split_config.arm64_v8a.apk", "split_config.en.apk"]));
        // no processor splits: nothing to refuse here
        assert_eq!(choose(&names(&["base.apk", "split_config.en.apk"]), &pc()).unwrap().len(), 2);
        assert!(choose(&names(&["split_config.en.apk"]), &pc()).is_err());
        assert!(choose(&[], &pc()).is_err());
    }

    #[test]
    fn unknown_configurations_are_kept() {
        let odd = names(&["base.apk", "split_config.nodpi.apk", "split_config.zh_CN.apk", "split_config.b+sr+Latn.apk"]);
        assert_eq!(choose(&odd, &pc()).unwrap(), names(&["base.apk", "split_config.nodpi.apk", "split_config.b+sr+Latn.apk"]));
    }

    /// A zip with stored (empty) files of these names.
    fn zip(names: &[&str], comment: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut dir = Vec::new();
        for name in names {
            let offset = out.len() as u32;
            out.extend_from_slice(&[0x50, 0x4b, 0x03, 0x04]);
            out.extend_from_slice(&[0u8; 22]);
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&[0, 0]);
            out.extend_from_slice(name.as_bytes());
            dir.extend_from_slice(&[0x50, 0x4b, 0x01, 0x02]);
            dir.extend_from_slice(&[0u8; 24]);
            dir.extend_from_slice(&(name.len() as u16).to_le_bytes());
            dir.extend_from_slice(&[0u8; 12]);
            dir.extend_from_slice(&offset.to_le_bytes());
            dir.extend_from_slice(name.as_bytes());
        }
        let dir_offset = out.len() as u32;
        out.extend_from_slice(&dir);
        out.extend_from_slice(&[0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0]);
        out.extend_from_slice(&(names.len() as u16).to_le_bytes());
        out.extend_from_slice(&(names.len() as u16).to_le_bytes());
        out.extend_from_slice(&(dir.len() as u32).to_le_bytes());
        out.extend_from_slice(&dir_offset.to_le_bytes());
        out.extend_from_slice(&(comment.len() as u16).to_le_bytes());
        out.extend_from_slice(comment);
        out
    }

    fn inspect_bytes(name: &str, bytes: &[u8]) -> Result<Package, String> {
        let path = std::env::temp_dir().join(format!("airdroid-bundle-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).unwrap();
        let r = inspect(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        r
    }

    #[test]
    fn apks_and_bundles_are_told_apart() {
        let apk = zip(&["AndroidManifest.xml", "classes.dex", "res/raw/inner.apk"], b"");
        assert_eq!(inspect_bytes("a.apk", &apk).unwrap(), Package::Apk);

        let bundle = zip(&["info.json", "base.apk", "split_config.en.apk", "icon.png"], b"a comment, PK\x05\x06 in it");
        assert_eq!(
            inspect_bytes("b.apkm", &bundle).unwrap(),
            Package::Bundle {
                apks: names(&["base.apk", "split_config.en.apk"]),
                entries: names(&["info.json", "base.apk", "split_config.en.apk", "icon.png"])
            }
        );

        assert!(inspect_bytes("c.zip", &zip(&["readme.txt"], b"")).unwrap_err().contains("no app (.apk)"));
        assert!(inspect_bytes("g.apkm", &zip(&["../base.apk", "-x.apk"], b"")).unwrap_err().contains("no app (.apk)"));
        assert!(inspect_bytes("d.apkm", b"\x02\x00encrypted").unwrap_err().contains("old encrypted format"));
        assert!(inspect_bytes("e.apk", b"plain text").unwrap_err().contains("not an Android app"));
        assert!(inspect_bytes("f.apk", b"PK\x03\x04 and then nothing").is_err());
    }
}
