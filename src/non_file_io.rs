//! Structured feature file support and non-filesystem UFO I/O helpers.
//!
//! This module provides:
//!
//! - [`load_feature_files`]: loads `features.fea` and all transitively
//!   `include()`-d feature files from a [`FontSource`], populating the
//!   [`Font::feature_files`][crate::Font::feature_files] map.
//! - [`expand_feature_text`]: expands `include()` directives in a feature
//!   text using a `feature_files` map, producing a single flattened string.
//! - [`save_font_with_sink`]: serializes a [`Font`][crate::Font] to a
//!   [`FontSink`], the write-side counterpart of `FontSource`.
//! - [`normalize_feature_text`]: normalizes CRLF to LF in feature text.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};

use crate::error::{FontLoadError, FontWriteError};
use crate::font::{
    Font, FormatVersion, MetaInfo, DATA_DIR, DEFAULT_METAINFO_CREATOR, FEATURES_FILE,
    FONTINFO_FILE, GROUPS_FILE, IMAGES_DIR, KERNING_FILE, LIB_FILE, METAINFO_FILE,
};
use crate::font_sink::FontSink;
use crate::font_source::FontSource;
use crate::groups::validate_groups;
use crate::layer::LAYER_CONTENTS_FILE;
use crate::shared_types::PUBLIC_OBJECT_LIBS_KEY;
use crate::write::{self, WriteOptions};

type FeatureFilesLoadResult = Option<(String, BTreeMap<PathBuf, String>)>;

/// Load `features.fea` and all transitively included feature files from `source`.
///
/// Returns `None` if `features.fea` does not exist in the source.
/// Returns `Some((main_features, included_files))` where:
/// - `main_features` is the raw content of `features.fea`
/// - `included_files` maps each included file's virtual path to its content
pub(crate) fn load_feature_files(
    source: &dyn FontSource,
    path: &Path,
) -> Result<FeatureFilesLoadResult, FontLoadError> {
    let Some(data) = source.try_read(path) else {
        return Ok(None);
    };
    let data = data.map_err(FontLoadError::FeatureFile)?;
    let contents = String::from_utf8(data).map_err(|e| {
        FontLoadError::FeatureFile(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    })?;

    let normalized_path = normalize_virtual_path(path);
    let mut stack = HashSet::new();
    let mut included_files = BTreeMap::new();
    stack.insert(normalized_path.clone());
    collect_feature_includes(source, &normalized_path, &contents, &mut stack, &mut included_files)?;
    Ok(Some((contents, included_files)))
}

fn collect_feature_includes(
    source: &dyn FontSource,
    current_path: &Path,
    contents: &str,
    stack: &mut HashSet<PathBuf>,
    included_files: &mut BTreeMap<PathBuf, String>,
) -> Result<(), FontLoadError> {
    for line in contents.split_inclusive('\n') {
        if let Some(include_target) = parse_include_target(line) {
            let include_path =
                join_virtual_path(current_path.parent().unwrap_or(Path::new("")), &include_target);
            if stack.contains(&include_path) {
                return Err(FontLoadError::FeatureIncludeCycle { path: include_path });
            }
            if included_files.contains_key(&include_path) {
                continue;
            }

            let include_data = source.try_read(&include_path).ok_or_else(|| {
                FontLoadError::MissingIncludedFeatureFile { path: include_path.clone() }
            })?;
            let include_data = include_data.map_err(FontLoadError::FeatureFile)?;
            let include_contents = String::from_utf8(include_data).map_err(|e| {
                FontLoadError::FeatureFile(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            })?;

            stack.insert(include_path.clone());
            collect_feature_includes(
                source,
                &include_path,
                &include_contents,
                stack,
                included_files,
            )?;
            stack.remove(&include_path);
            included_files.insert(include_path, include_contents);
        }
    }

    Ok(())
}

/// Expand `include()` directives in `features` using the `feature_files` map.
///
/// This produces a single flattened feature text with all includes inlined.
pub(crate) fn expand_feature_text(
    features: &str,
    feature_files: &BTreeMap<PathBuf, String>,
) -> Result<String, FontLoadError> {
    let mut stack = HashSet::new();
    let root_path = normalize_virtual_path(Path::new(FEATURES_FILE));
    stack.insert(root_path.clone());
    expand_feature_text_from_map(&root_path, features, feature_files, &mut stack)
}

fn expand_feature_text_from_map(
    current_path: &Path,
    contents: &str,
    feature_files: &BTreeMap<PathBuf, String>,
    stack: &mut HashSet<PathBuf>,
) -> Result<String, FontLoadError> {
    let mut out = String::new();

    for line in contents.split_inclusive('\n') {
        if let Some(include_target) = parse_include_target(line) {
            let include_path =
                join_virtual_path(current_path.parent().unwrap_or(Path::new("")), &include_target);
            if !stack.insert(include_path.clone()) {
                return Err(FontLoadError::FeatureIncludeCycle { path: include_path });
            }
            let include_contents = feature_files.get(&include_path).ok_or_else(|| {
                FontLoadError::MissingIncludedFeatureFile { path: include_path.clone() }
            })?;
            out.push_str(&expand_feature_text_from_map(
                &include_path,
                include_contents,
                feature_files,
                stack,
            )?);
            stack.remove(&include_path);
        } else {
            out.push_str(line);
        }
    }

    Ok(out)
}

fn parse_include_target(line: &str) -> Option<PathBuf> {
    let trimmed = line.trim();
    if !trimmed.starts_with("include") || !trimmed.ends_with(';') {
        return None;
    }

    let open = trimmed.find('(')?;
    let close = trimmed.rfind(')')?;
    if close < open {
        return None;
    }

    let inner = trimmed[open + 1..close].trim();
    if inner.is_empty() {
        return None;
    }

    let inner = inner.trim_matches(|c| c == '"' || c == '\'');
    if inner.is_empty() {
        return None;
    }

    Some(normalize_virtual_path(Path::new(inner)))
}

fn join_virtual_path(base: &Path, relative: &Path) -> PathBuf {
    if relative.is_absolute() {
        return normalize_virtual_path(relative);
    }
    normalize_virtual_path(&base.join(relative))
}

/// Lexically normalize a UFO virtual path without consulting cwd or the host filesystem.
pub(crate) fn normalize_virtual_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
            Component::RootDir | Component::Prefix(_) => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

/// Normalize CRLF to LF in feature text.
pub(crate) fn normalize_feature_text(contents: &str) -> Cow<'_, str> {
    if contents.as_bytes().contains(&b'\r') {
        Cow::Owned(contents.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(contents)
    }
}

/// Serialize a [`Font`] to a [`FontSink`].
///
/// This is the sink-based counterpart to [`Font::load_from_source`]. It writes
/// all UFO files (metainfo, fontinfo, lib, groups, kerning, features, layers,
/// data, images) to the sink as `(relative_path, bytes)` pairs.
///
/// Structured feature files (stored in `font.feature_files`) are written
/// alongside `features.fea`.
pub(crate) fn save_font_with_sink<S: FontSink>(
    font: &Font,
    options: &WriteOptions,
    sink: &mut S,
) -> Result<(), FontWriteError> {
    if font.meta.format_version != FormatVersion::V3 {
        return Err(FontWriteError::Downgrade);
    }

    if font.lib.contains_key(PUBLIC_OBJECT_LIBS_KEY) {
        return Err(FontWriteError::PreexistingPublicObjectLibsKey);
    }

    validate_groups(&font.groups).map_err(FontWriteError::InvalidGroups)?;
    font.font_info.validate().map_err(FontWriteError::InvalidFontInfo)?;

    for (path, entry) in font.data.iter().chain(font.images.iter()) {
        if let Err(source) = entry {
            return Err(FontWriteError::InvalidStoreEntry { path: path.clone(), source });
        }
    }

    let metainfo_value = if font.meta.creator == Some(DEFAULT_METAINFO_CREATOR.into()) {
        font.meta.clone()
    } else {
        MetaInfo::default()
    };
    write_sink_file(
        sink,
        Path::new(METAINFO_FILE),
        &write::write_xml_to_bytes(&metainfo_value, options)
            .map_err(|source| FontWriteError::CustomFile { name: METAINFO_FILE, source })?,
    )?;

    if !font.font_info.is_empty() {
        write_sink_file(
            sink,
            Path::new(FONTINFO_FILE),
            &write::write_xml_to_bytes(&font.font_info, options)
                .map_err(|source| FontWriteError::CustomFile { name: FONTINFO_FILE, source })?,
        )?;
    }

    let mut lib = font.lib.clone();
    let font_object_libs = font.font_info.dump_object_libs();
    if !font_object_libs.is_empty() {
        lib.insert(PUBLIC_OBJECT_LIBS_KEY.into(), font_object_libs.into());
    }
    if !lib.is_empty() {
        crate::util::recursive_sort_plist_keys(&mut lib);
        write_sink_file(
            sink,
            Path::new(LIB_FILE),
            &write::write_xml_to_bytes(&lib, options)
                .map_err(|source| FontWriteError::CustomFile { name: LIB_FILE, source })?,
        )?;
    }

    if !font.groups.is_empty() {
        write_sink_file(
            sink,
            Path::new(GROUPS_FILE),
            &write::write_xml_to_bytes(&font.groups, options)
                .map_err(|source| FontWriteError::CustomFile { name: GROUPS_FILE, source })?,
        )?;
    }

    if !font.kerning.is_empty() {
        let kerning_serializer = crate::kerning::KerningSerializer { kerning: &font.kerning };
        write_sink_file(
            sink,
            Path::new(KERNING_FILE),
            &write::write_xml_to_bytes(&kerning_serializer, options)
                .map_err(|source| FontWriteError::CustomFile { name: KERNING_FILE, source })?,
        )?;
    }

    if !font.features.is_empty() || !font.feature_files.is_empty() {
        write_sink_file(
            sink,
            Path::new(FEATURES_FILE),
            normalize_feature_text(&font.features).as_bytes(),
        )?;
        for (feature_path, contents) in &font.feature_files {
            write_sink_file(sink, feature_path, normalize_feature_text(contents).as_bytes())?;
        }
    }

    let contents: Vec<(&str, &PathBuf)> =
        font.layers.iter().map(|layer| (layer.name().as_ref(), &layer.path)).collect();
    write_sink_file(
        sink,
        Path::new(LAYER_CONTENTS_FILE),
        &write::write_xml_to_bytes(&contents, options)
            .map_err(|source| FontWriteError::CustomFile { name: LAYER_CONTENTS_FILE, source })?,
    )?;

    for layer in font.layers.iter() {
        let layer_path = Path::new(layer.path());
        layer.save_with_sink(layer_path, options, sink).map_err(|source| {
            FontWriteError::Layer {
                name: layer.name().to_string(),
                path: layer_path.to_path_buf(),
                source: Box::new(source),
            }
        })?;
    }

    if !font.data.is_empty() {
        for (data_path, contents) in font.data.iter() {
            let data = contents.expect("internal error: should have been checked");
            write_sink_file(sink, &Path::new(DATA_DIR).join(data_path), &data[..])?;
        }
    }

    if !font.images.is_empty() {
        for (image_path, contents) in font.images.iter() {
            let data = contents.expect("internal error: should have been checked");
            write_sink_file(sink, &Path::new(IMAGES_DIR).join(image_path), &data[..])?;
        }
    }

    Ok(())
}

fn write_sink_file<S: FontSink>(
    sink: &mut S,
    path: &Path,
    bytes: &[u8],
) -> Result<(), FontWriteError> {
    sink.write(path, bytes).map_err(|source| FontWriteError::Sink {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}
