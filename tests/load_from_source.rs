//! Integration tests for loading fonts from a non-filesystem FontSource.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use norad::{DataRequest, Font, FontSink, FontSource, WriteOptions};

/// A simple in-memory source that wraps a HashMap.
struct MemorySource(HashMap<PathBuf, Vec<u8>>);

impl FontSource for MemorySource {
    fn try_read(&self, path: &Path) -> Option<Result<Vec<u8>, io::Error>> {
        self.0.get(path).cloned().map(Ok)
    }
}

/// Build a MemorySource by walking a real UFO directory on disk.
fn source_from_ufo_dir(ufo_path: &str) -> MemorySource {
    let root = Path::new(ufo_path);
    let mut map = HashMap::new();
    walk_dir(root, root, &mut map);
    MemorySource(map)
}

fn walk_dir(root: &Path, dir: &Path, map: &mut HashMap<PathBuf, Vec<u8>>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            walk_dir(root, &path, map);
        } else {
            let rel = path.strip_prefix(root).unwrap().to_path_buf();
            let contents = std::fs::read(&path).unwrap();
            map.insert(rel, contents);
        }
    }
}

#[test]
fn load_from_source_matches_filesystem() {
    let ufo_path = "testdata/MutatorSansLightWide.ufo";
    let source = source_from_ufo_dir(ufo_path);

    let font_fs = Font::load(ufo_path).unwrap();
    let font_src = Font::load_from_source(&DataRequest::all(), &source).unwrap();

    // Core data should match
    assert_eq!(font_fs.meta, font_src.meta);
    assert_eq!(font_fs.font_info, font_src.font_info);
    assert_eq!(font_fs.lib, font_src.lib);
    assert_eq!(font_fs.groups, font_src.groups);
    assert_eq!(font_fs.kerning, font_src.kerning);
    assert_eq!(font_fs.features, font_src.features);

    // Layers and glyphs
    assert_eq!(font_fs.iter_layers().count(), font_src.iter_layers().count());
    assert_eq!(font_fs.glyph_count(), font_src.glyph_count());
    for layer_fs in font_fs.iter_layers() {
        let layer_src = font_src.layers.get(layer_fs.name()).unwrap();
        assert_eq!(layer_fs, layer_src, "layer '{}' mismatch", layer_fs.name());
    }
}

#[test]
fn load_from_source_with_closure() {
    let ufo_path = "testdata/MutatorSansLightWide.ufo";
    let source = source_from_ufo_dir(ufo_path);

    let reader =
        |path: &Path| -> Option<Result<Vec<u8>, io::Error>> { source.0.get(path).cloned().map(Ok) };
    let font = Font::load_from_source(&DataRequest::all(), &reader).unwrap();

    assert_eq!(font.glyph_count(), 48);
}

#[test]
fn load_from_source_data_request_none() {
    let ufo_path = "testdata/MutatorSansLightWide.ufo";
    let source = source_from_ufo_dir(ufo_path);

    let font = Font::load_from_source(&DataRequest::none(), &source).unwrap();

    assert!(font.groups.is_empty());
    assert!(font.kerning.is_empty());
    assert!(font.features.is_empty());
    assert!(font.default_layer().is_empty());
}

#[test]
fn load_from_source_missing_metainfo() {
    let source = MemorySource(HashMap::new());
    let result = Font::load_from_source(&DataRequest::all(), &source);
    assert!(result.is_err());
}

/// A simple in-memory sink that collects files into a BTreeMap.
struct MemorySink(Mutex<std::collections::BTreeMap<PathBuf, Vec<u8>>>);

impl FontSink for MemorySink {
    fn write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        self.0.lock().unwrap().insert(path.to_path_buf(), data.to_vec());
        Ok(())
    }
}

#[test]
fn save_with_sink_round_trips() {
    let ufo_path = "testdata/MutatorSansLightWide.ufo";
    let source = source_from_ufo_dir(ufo_path);
    let font = Font::load_from_source(&DataRequest::all(), &source).unwrap();

    let mut sink = MemorySink(Mutex::new(Default::default()));
    font.save_with_sink(&WriteOptions::default(), &mut sink).unwrap();

    // Every core file should be present in the sink output.
    let files = sink.0.into_inner().unwrap();
    assert!(files.contains_key(Path::new("metainfo.plist")));
    assert!(files.contains_key(Path::new("layercontents.plist")));
    assert!(files.contains_key(Path::new("glyphs/contents.plist")));

    // Reload from the sink output and verify the round-trip.
    let reload_map: HashMap<PathBuf, Vec<u8>> = files.into_iter().collect();
    let reload_source = MemorySource(reload_map);
    let reloaded = Font::load_from_source(&DataRequest::all(), &reload_source).unwrap();

    assert_eq!(font.font_info, reloaded.font_info);
    assert_eq!(font.lib, reloaded.lib);
    assert_eq!(font.groups, reloaded.groups);
    assert_eq!(font.kerning, reloaded.kerning);
    assert_eq!(font.features, reloaded.features);
    assert_eq!(font.glyph_count(), reloaded.glyph_count());
    // Note: metainfo creator is normalized to org.linebender.norad on save
    // when it was already org.linebender.norad, or reset to default otherwise.
    assert_eq!(reloaded.meta.format_version, font.meta.format_version);
}

#[test]
fn structured_feature_files_load_and_expand() {
    let mut entries: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    entries.insert(
        PathBuf::from("metainfo.plist"),
        br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>creator</key>
    <string>org.linebender.norad</string>
    <key>formatVersion</key>
    <integer>3</integer>
</dict>
</plist>"#
            .to_vec(),
    );
    entries.insert(
        PathBuf::from("features.fea"),
        b"languagesystem DFLT dflt;\ninclude( includes/shared.fea );\nfeature liga {\n    sub A A by A;\n} liga;\n".to_vec(),
    );
    entries.insert(PathBuf::from("includes/shared.fea"), b"@shared = [A];\n".to_vec());
    entries.insert(
        PathBuf::from("layercontents.plist"),
        br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<array>
    <array>
        <string>public.default</string>
        <string>glyphs</string>
    </array>
</array>
</plist>"#
            .to_vec(),
    );
    entries.insert(
        PathBuf::from("glyphs/contents.plist"),
        br#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>A</key>
    <string>A_.glif</string>
</dict>
</plist>"#
            .to_vec(),
    );
    entries.insert(
        PathBuf::from("glyphs/A_.glif"),
        br#"<?xml version="1.0" encoding="UTF-8"?>
<glyph name="A" format="2">
  <advance width="500"/>
</glyph>"#
            .to_vec(),
    );

    let source = MemorySource(entries);
    let font = Font::load_from_source(&DataRequest::all(), &source).unwrap();

    // The main feature text should be loaded as-is.
    assert_eq!(
        font.features,
        "languagesystem DFLT dflt;\ninclude( includes/shared.fea );\nfeature liga {\n    sub A A by A;\n} liga;\n"
    );
    // The included file should be in feature_files.
    assert_eq!(
        font.feature_files.get(Path::new("includes/shared.fea")),
        Some(&"@shared = [A];\n".to_string())
    );
    // features_expanded should inline the include.
    assert_eq!(
        font.features_expanded().unwrap(),
        "languagesystem DFLT dflt;\n@shared = [A];\nfeature liga {\n    sub A A by A;\n} liga;\n"
    );

    // Round-trip through a sink.
    let mut sink = MemorySink(Mutex::new(Default::default()));
    font.save_with_sink(&WriteOptions::default(), &mut sink).unwrap();

    // Both features.fea and the included file should be written.
    let files = sink.0.into_inner().unwrap();
    assert!(files.contains_key(Path::new("features.fea")));
    assert!(files.contains_key(Path::new("includes/shared.fea")));

    // Reload and verify.
    let reload_map: HashMap<PathBuf, Vec<u8>> = files.into_iter().collect();
    let reload_source = MemorySource(reload_map);
    let reloaded = Font::load_from_source(&DataRequest::all(), &reload_source).unwrap();
    assert_eq!(reloaded.features, font.features);
    assert_eq!(reloaded.feature_files, font.feature_files);
    assert_eq!(reloaded.features_expanded().unwrap(), font.features_expanded().unwrap());
}
