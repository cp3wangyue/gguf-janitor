//! Discovery of local LLM model stores and their model files.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::gguf::{self, GGUF_MAGIC};
use crate::{ModelFile, ScanProgress, StoreKind, StoreSummary};

#[derive(Debug, Error)]
pub enum DiscoverError {
    #[error("home directory unavailable")]
    NoHome,
}

/// A model store root directory.
#[derive(Debug, Clone)]
pub struct StoreDir {
    pub kind: StoreKind,
    pub root: PathBuf,
}

impl StoreDir {
    fn summary(&self, files: u64, bytes: u64, missing: bool) -> StoreSummary {
        StoreSummary {
            kind: self.kind,
            root: self.root.display().to_string(),
            files,
            bytes,
            missing,
        }
    }
}

fn home() -> Result<PathBuf, DiscoverError> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .ok_or(DiscoverError::NoHome)
}

/// Locate the well-known model stores on this machine.
pub fn known_stores() -> Result<Vec<StoreDir>, DiscoverError> {
    let home = home()?;
    let mut stores = Vec::new();

    // Ollama: OLLAMA_MODELS env override, else ~/.ollama/models
    let ollama_root = std::env::var_os("OLLAMA_MODELS")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".ollama").join("models"));
    stores.push(StoreDir { kind: StoreKind::Ollama, root: ollama_root });

    // LM Studio: ~/.lmstudio/models (v0.3+) and legacy ~/.cache/lm-studio/models
    stores.push(StoreDir { kind: StoreKind::LmStudio, root: home.join(".lmstudio").join("models") });
    stores.push(StoreDir {
        kind: StoreKind::LmStudio,
        root: home.join(".cache").join("lm-studio").join("models"),
    });

    // HuggingFace cache: HF_HUB_CACHE override, else ~/.cache/huggingface/hub
    let hf_root = std::env::var_os("HF_HUB_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".cache").join("huggingface").join("hub"));
    stores.push(StoreDir { kind: StoreKind::HuggingFace, root: hf_root });

    Ok(stores)
}

/// All fixed drives on Windows (for the full-disk loose scan).
pub fn fixed_drives() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for letter in b'A'..=b'Z' {
        let root = PathBuf::from(format!("{}:\\", letter as char));
        if root.is_dir() {
            // Heuristic: only scans that resolve as fixed are cheap to probe for;
            // GetDriveTypeW would be better but this matches DRIVE_FIXED well
            // enough because CD/absent letters are not dirs.
            out.push(root);
        }
    }
    out
}

fn is_gguf(path: &Path) -> bool {
    let mut f = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut magic = [0u8; 4];
    match f.read_exact(&mut magic) {
        Ok(()) => u32::from_le_bytes(magic) == GGUF_MAGIC,
        Err(_) => false,
    }
}

/// Parse "<base>-00001-of-00003" split naming; returns the shard-group key.
fn split_group_key(stem: &str) -> Option<String> {
    // ...-NNNNN-of-MMMMM
    let idx = stem.rfind("-of-")?;
    let (left, right) = stem.split_at(idx);
    let m_str = &right[4..];
    if m_str.len() != 5 || !m_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let dash = left.rfind('-')?;
    let n_str = &left[dash + 1..];
    if n_str.len() != 5 || !n_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(left[..dash].to_string())
}

/// Map ollama blob digest -> human tag by walking ~/.ollama/models/manifests.
fn ollama_tags(models_root: &Path) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let manifests = models_root.join("manifests");
    let Ok(rd) = fs::read_dir(&manifests) else { return map };
    for reg in rd.flatten() {
        let reg_path = reg.path();
        let Ok(ns_rd) = fs::read_dir(&reg_path) else { continue };
        for ns in ns_rd.flatten() {
            // Either <registry>/<user>/<model>/<tag> or <registry>/<library-model>/<tag>;
            // walk recursively and treat the last four components as the tag path.
            walk_manifest_leaves(&ns.path(), &mut Vec::new(), &mut map, &reg_path);
        }
    }
    map
}

fn walk_manifest_leaves(
    dir: &Path,
    stack: &mut Vec<String>,
    map: &mut BTreeMap<String, String>,
    manifests_root: &Path,
) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            stack.push(e.file_name().to_string_lossy().into_owned());
            walk_manifest_leaves(&p, stack, map, manifests_root);
            stack.pop();
        } else if let Ok(txt) = fs::read(&p) {
            // tag path components relative to manifests root
            let mut parts: Vec<String> = p
                .strip_prefix(manifests_root)
                .map(|rel| rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect())
                .unwrap_or_default();
            if parts.len() >= 2 {
                let tag = parts.pop().unwrap();
                let model = parts.pop().unwrap();
                let _ns = parts.pop().unwrap_or_default(); // user/org or "library"
                let name = format!("{model}:{tag}");
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&txt) {
                    if let Some(layers) = v.get("layers").and_then(|l| l.as_array()) {
                        for l in layers {
                            if let Some(d) = l.get("digest").and_then(|d| d.as_str()) {
                                let file = d.replace(':', "-");
                                map.insert(file, name.clone());
                            }
                        }
                    }
                }
            }
        }
    }
}

fn push_file(
    path: &Path,
    kind: StoreKind,
    parse_gguf: bool,
    ollama_tag: Option<String>,
    out: &mut Vec<ModelFile>,
    progress: &mut impl FnMut(&ScanProgress),
) {
    let Ok(md) = fs::metadata(path) else { return };
    if !md.is_file() || md.len() == 0 {
        return;
    }
    progress(&ScanProgress {
        phase: crate::ProgressPhase::Parsing,
        files_done: out.len() as u64,
        files_total: 0,
        bytes_done: 0,
        bytes_total: 0,
        current: path.display().to_string(),
    });
    let gguf_summary = if parse_gguf && is_gguf(path) {
        gguf::parse_header(path).ok().map(|i| i.summarize())
    } else {
        None
    };
    let split_group = path
        .file_stem()
        .and_then(|s| split_group_key(&s.to_string_lossy()));
    // Only treat files as model files when they look like models: .gguf
    // extension, a verified GGUF header (ollama blobs), or a known split name.
    let is_gguf_ext = path.extension().map(|e| e == "gguf").unwrap_or(false);
    let looks_like_model = is_gguf_ext || is_gguf(path) || split_group.is_some();
    if !looks_like_model {
        return;
    }
    out.push(ModelFile {
        path: path.display().to_string(),
        size: md.len(),
        store: kind,
        gguf: gguf_summary,
        split_group,
        ollama_tag,
    });
}

/// Collect model files from the given stores plus extra user directories.
///
/// `parse_gguf` controls header parsing (costs one open+read per file).
pub fn collect_files(
    stores: &[StoreDir],
    extra_dirs: &[PathBuf],
    parse_gguf: bool,
    progress: &mut impl FnMut(&ScanProgress),
) -> Vec<ModelFile> {
    let mut files = Vec::new();
    let ollama_map = stores
        .iter()
        .find(|s| s.kind == StoreKind::Ollama && s.root.is_dir())
        .map(|s| ollama_tags(&s.root))
        .unwrap_or_default();

    for store in stores {
        if !store.root.is_dir() {
            continue;
        }
        match store.kind {
            StoreKind::Ollama => {
                let blobs = store.root.join("blobs");
                let Ok(rd) = fs::read_dir(&blobs) else { continue };
                for e in rd.flatten() {
                    let p = e.path();
                    let name = e.file_name().to_string_lossy().into_owned();
                    if !name.starts_with("sha256-") {
                        continue;
                    }
                    let tag = ollama_map.get(&name).cloned();
                    // Blob files have no extension; verify GGUF magic before use.
                    if is_gguf(&p) {
                        push_file(&p, store.kind, parse_gguf, tag, &mut files, progress);
                    }
                }
            }
            _ => {
                for entry in walkdir::WalkDir::new(&store.root)
                    .follow_links(false)
                    .into_iter()
                    .filter_map(|e| e.ok())
                {
                    if !entry.file_type().is_file() {
                        continue;
                    }
                    let is_gguf_ext = entry.path().extension().map(|e| e == "gguf").unwrap_or(false);
                    if !is_gguf_ext {
                        continue;
                    }
                    push_file(entry.path(), store.kind, parse_gguf, None, &mut files, progress);
                }
            }
        }
    }

    for dir in extra_dirs {
        if !dir.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let is_gguf_ext = entry.path().extension().map(|e| e == "gguf").unwrap_or(false);
            if !is_gguf_ext {
                continue;
            }
            push_file(entry.path(), StoreKind::Custom, parse_gguf, None, &mut files, progress);
        }
    }

    files
}

/// Summaries for stores (including not-found ones, flagged `missing`).
pub fn store_summaries(stores: &[StoreDir], files: &[ModelFile]) -> Vec<StoreSummary> {
    let mut out = Vec::new();
    for s in stores {
        let mine: Vec<&ModelFile> = files.iter().filter(|f| {
            f.path.starts_with(&s.root.display().to_string())
        }).collect();
        let bytes = mine.iter().map(|f| f.size).sum();
        out.push(s.summary(mine.len() as u64, bytes, !s.root.is_dir()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_keys() {
        assert_eq!(
            split_group_key("Qwen2.5-7B-Instruct-Q4_K_M-00001-of-00003"),
            Some("Qwen2.5-7B-Instruct-Q4_K_M".to_string())
        );
        assert_eq!(split_group_key("plain-model"), None);
        assert_eq!(split_group_key("bad-1-of-3"), None);
        assert_eq!(split_group_key("bad-00001-of-3"), None);
    }

    #[test]
    fn fixed_drives_at_least_c() {
        // Test machines always have C:.
        assert!(fixed_drives().iter().any(|d| d.starts_with("C:")));
    }

    #[test]
    fn ollama_tags_parses_manifest() {
        let tmp = std::env::temp_dir().join(format!("gj-test-{}", std::process::id()));
        let manifests = tmp.join("manifests").join("registry.ollama.ai").join("library").join("llama3");
        fs::create_dir_all(&manifests).unwrap();
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "config": {"digest": "sha256:aaa"},
            "layers": [
                {"digest": "sha256:bigblob1", "mediaType": "application/vnd.ollama.image.model", "size": 123},
                {"digest": "sha256:cfg", "mediaType": "application/vnd.ollama.image.params", "size": 5}
            ]
        });
        fs::write(manifests.join("latest"), serde_json::to_vec(&manifest).unwrap()).unwrap();
        let map = ollama_tags(&tmp);
        assert_eq!(map.get("sha256-bigblob1").map(|s| s.as_str()), Some("llama3:latest"));
        assert_eq!(map.get("sha256-cfg").map(|s| s.as_str()), Some("llama3:latest"));
        fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn collect_files_finds_gguf_and_blobs() {
        let tmp = std::env::temp_dir().join(format!("gj-collect-{}", std::process::id()));
        let ollama_models = tmp.join(".ollama").join("models");
        fs::create_dir_all(ollama_models.join("blobs")).unwrap();
        let mut blob = gguf::GGUF_MAGIC.to_le_bytes().to_vec();
        blob.extend_from_slice(&[0u8; 64]); // truncated header is fine: magic check only
        fs::write(ollama_models.join("blobs").join("sha256-abc"), &blob).unwrap();
        fs::write(ollama_models.join("blobs").join("sha256-notgguf"), b"not a model").unwrap();

        let lm = tmp.join(".lmstudio").join("models").join("org").join("repo");
        fs::create_dir_all(&lm).unwrap();
        fs::write(lm.join("model.gguf"), b"whatever").unwrap();
        fs::write(lm.join("notes.txt"), b"skip me").unwrap();

        let stores = vec![
            StoreDir { kind: StoreKind::Ollama, root: ollama_models.clone() },
            StoreDir { kind: StoreKind::LmStudio, root: tmp.join(".lmstudio").join("models") },
        ];
        let mut prog = |_p: &ScanProgress| {};
        let files = collect_files(&stores, &[], false, &mut prog);
        assert_eq!(files.len(), 2, "found: {files:?}");
        assert!(files.iter().any(|f| f.store == StoreKind::Ollama && f.path.ends_with("sha256-abc")));
        assert!(files.iter().any(|f| f.store == StoreKind::LmStudio && f.path.ends_with("model.gguf")));
        fs::remove_dir_all(&tmp).ok();
    }
}
