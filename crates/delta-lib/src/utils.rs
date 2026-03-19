use crate::error::{Error, Result};
use crate::host::Host;
use encoding_rs::Encoding;
use glob::Pattern;
use normpath::PathExt;
use serde::de::DeserializeOwned;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn deser_json<T: DeserializeOwned>(file_path: &Path) -> Result<T> {
    let file_path_str = file_path.display().to_string();

    let bytes = fs::read(file_path).map_err(|source| Error::JsonFileRead {
        file: file_path_str.clone(),
        source,
    })?;

    let (encoding, bytes_without_bom) = Encoding::for_bom(&bytes).unwrap_or((encoding_rs::UTF_8, 0));

    let bytes_to_decode = if bytes_without_bom > 0 {
        &bytes[bytes_without_bom..]
    } else {
        &bytes
    };

    let (content, _, error) = encoding.decode(bytes_to_decode);

    if error {
        return Err(Error::JsonFileRead {
            file: file_path_str,
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Unable to decode file: {}", encoding.name()),
            ),
        });
    }

    serde_json::from_str(&content).map_err(|source| Error::JsonFileParse {
        file: file_path_str,
        source,
    })
}

pub fn resolve_includes(host: &mut impl Host, base: &Path, includes: &[String]) -> Vec<PathBuf> {
    let mut files = Vec::new();

    for include_path in includes {
        if let Some(resolved_path) = resolve(base, include_path) {
            files.push(resolved_path);
        } else {
            let _ = writeln!(host.error(), "Warning: Could not resolve include_str! path: {include_path}");
        }
    }

    files
}

pub fn resolve(base: &Path, relative_path: &str) -> Option<PathBuf> {
    let base_dir = base.parent()?;
    let candidate_path = base_dir.join(relative_path);

    candidate_path.normalize().map_or_else(
        |_| candidate_path.exists().then_some(candidate_path),
        |normalized| {
            let path_buf = normalized.into_path_buf();
            path_buf.exists().then_some(path_buf)
        },
    )
}

pub fn resolve_workspace_relative(workspace: &Path, relative_path: &str) -> Option<PathBuf> {
    let candidate_path = workspace.join(relative_path);

    candidate_path.normalize().map_or_else(
        |_| candidate_path.exists().then_some(candidate_path),
        |normalized| {
            let path_buf = normalized.into_path_buf();
            path_buf.exists().then_some(path_buf)
        },
    )
}

pub struct UnrelatedFiles {
    pub unaccounted: Vec<PathBuf>,
    pub tripwire: Vec<PathBuf>,
    pub filtered: Vec<PathBuf>,
}

pub fn find_unrelated(git_root: &Path, excludes: &[PathBuf], exclude_patterns: &[String], tripwire_patterns: &[String]) -> UnrelatedFiles {
    fn visit(
        dir: &Path,
        git_root: &Path,
        excludes: &[PathBuf],
        excludes_processed: &[PathBuf],
        compiled_patterns: &[Pattern],
        compiled_tripwires: &[Pattern],
        result: &mut UnrelatedFiles,
    ) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();

            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && compiled_patterns.iter().any(|pattern| pattern.matches(name))
            {
                if path.is_file()
                    && let Ok(rel) = path.strip_prefix(git_root)
                {
                    result.filtered.push(rel.to_path_buf());
                }
                continue;
            }

            if path.is_dir() {
                visit(
                    &path,
                    git_root,
                    excludes,
                    excludes_processed,
                    compiled_patterns,
                    compiled_tripwires,
                    result,
                );
                continue;
            }

            if !path.is_file() {
                continue;
            }

            let relative_path = match path.strip_prefix(git_root) {
                Ok(rel) => rel.to_path_buf(),
                Err(_) => continue,
            };

            if excludes.contains(&relative_path) {
                continue;
            }

            if relative_path
                .normalize()
                .is_ok_and(|i| excludes_processed.contains(&i.into_path_buf()))
            {
                continue;
            }

            let file_str = relative_path.to_string_lossy();
            if compiled_tripwires.iter().any(|pattern| pattern.matches(&file_str)) {
                result.tripwire.push(relative_path);
            } else {
                result.unaccounted.push(relative_path);
            }
        }
    }

    let excludes_processed: Vec<PathBuf> = excludes
        .iter()
        .filter_map(|p| p.normalize().ok().map(normpath::BasePathBuf::into_path_buf))
        .collect();

    let compiled: Vec<Pattern> = exclude_patterns.iter().filter_map(|pattern| Pattern::new(pattern).ok()).collect();

    let compiled_tripwires: Vec<Pattern> = tripwire_patterns.iter().filter_map(|pattern| Pattern::new(pattern).ok()).collect();

    let mut result = UnrelatedFiles {
        unaccounted: Vec::new(),
        tripwire: Vec::new(),
        filtered: Vec::new(),
    };
    visit(
        git_root,
        git_root,
        excludes,
        &excludes_processed,
        &compiled,
        &compiled_tripwires,
        &mut result,
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::TestHost;

    #[test]
    fn resolve_includes_warns_on_unresolvable_path() {
        let mut host = TestHost::new();
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src").join("lib.rs");
        let result = resolve_includes(&mut host, &base, &["nonexistent_file_xyz.rs".to_string()]);
        assert!(result.is_empty());
        assert!(host.stderr_str().contains("Warning"));
        assert!(host.stderr_str().contains("nonexistent_file_xyz.rs"));
    }

    #[test]
    fn resolve_includes_resolves_existing_path() {
        let mut host = TestHost::new();
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src").join("lib.rs");
        let result = resolve_includes(&mut host, &base, &["host.rs".to_string()]);
        assert_eq!(result.len(), 1);
        assert!(host.stderr_str().is_empty());
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn deser_json_reads_valid_json() {
        let tmp = std::env::temp_dir().join("cargo_delta_test_deser_valid.json");
        fs::write(&tmp, r#"{"key": "value"}"#).unwrap();

        let result: std::collections::HashMap<String, String> = deser_json(&tmp).unwrap();
        assert_eq!(&result["key"], "value");

        let _ = fs::remove_file(&tmp);
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn deser_json_handles_utf8_bom() {
        let tmp = std::env::temp_dir().join("cargo_delta_test_deser_bom.json");
        let mut content = vec![0xEF, 0xBB, 0xBF]; // UTF-8 BOM
        content.extend_from_slice(br#"{"key": "value"}"#);
        fs::write(&tmp, &content).unwrap();

        let result: std::collections::HashMap<String, String> = deser_json(&tmp).unwrap();
        assert_eq!(&result["key"], "value");

        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn deser_json_returns_error_for_missing_file() {
        let result: Result<std::collections::HashMap<String, String>> = deser_json(Path::new("nonexistent_xyz_test.json"));
        let _ = result.unwrap_err();
    }

    #[test]
    #[cfg_attr(miri, ignore)]
    fn deser_json_returns_error_for_invalid_json() {
        let tmp = std::env::temp_dir().join("cargo_delta_test_deser_bad.json");
        fs::write(&tmp, "not json at all").unwrap();

        let result: Result<std::collections::HashMap<String, String>> = deser_json(&tmp);
        let _ = result.unwrap_err();

        let _ = fs::remove_file(&tmp);
    }
}
