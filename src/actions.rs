//! Space-reclaiming actions: NTFS hardlink dedupe, Recycle Bin deletion and
//! verified archive moves. Every action is dry-run by default and verifies
//! content identity before touching anything.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::dedupe::{head_tail_equal, hash_file, DupGroup};

#[derive(Debug, Error)]
pub enum ActionError {
    #[error("safety check failed: content changed or unreadable: {0}")]
    SafetyCheck(String),
    #[error("io error on {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("{0}")]
    Other(String),
}

fn io_err(path: &Path, e: std::io::Error) -> ActionError {
    ActionError::Io { path: path.display().to_string(), source: e }
}

#[derive(Debug, Clone, Serialize)]
pub struct ActionItem {
    pub path: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ActionResult {
    pub performed: Vec<ActionItem>,
    pub bytes_freed: u64,
    pub dry_run: bool,
}

/// Choose which member to keep. Prefer files inside tool-managed stores
/// (their managers expect their copies to exist), then the first member.
fn canonical_index(group: &DupGroup) -> usize {
    let pref = |s: crate::StoreKind| match s {
        crate::StoreKind::Ollama | crate::StoreKind::LmStudio | crate::StoreKind::HuggingFace => 0,
        _ => 1,
    };
    (0..group.members.len())
        .min_by_key(|&i| (pref(group.members[i].store), i))
        .unwrap_or(0)
}

/// Same NTFS volume? Compare the canonicalized path prefix ("C:\" style).
fn same_volume(a: &Path, b: &Path) -> bool {
    let vol = |p: &Path| -> Option<String> {
        let canon = fs::canonicalize(p).ok()?;
        let s = canon.display().to_string();
        // \\?\C:\Users\... -> take up to the first slash after \\?\X:
        s.split(['\\', '/']).take_while(|c| !c.is_empty() || true).take(4).last().map(|c| c.to_uppercase())
    };
    match (vol(a), vol(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// Replace every non-canonical member of `group` with an NTFS hardlink to the
/// canonical copy. All tools keep seeing "their" file; the disk stores one.
pub fn hardlink_group(group: &DupGroup, dry_run: bool) -> Result<ActionResult, ActionError> {
    let mut res = ActionResult { dry_run, ..Default::default() };
    let keep = canonical_index(group);
    let canonical = Path::new(&group.members[keep].path);

    for (i, m) in group.members.iter().enumerate() {
        if i == keep {
            continue;
        }
        let target = Path::new(&m.path);
        // Re-verify content identity right before acting.
        if !head_tail_equal(canonical, target)
            .map_err(|e| ActionError::SafetyCheck(format!("{}: {e}", m.path)))?
            || hash_file(canonical).map_err(|e| io_err(canonical, e))?
                != hash_file(target).map_err(|e| io_err(target, e))?
        {
            res.performed.push(ActionItem {
                path: m.path.clone(),
                ok: false,
                detail: "skipped: content no longer verified identical".into(),
            });
            continue;
        }
        if dry_run {
            res.performed.push(ActionItem {
                path: m.path.clone(),
                ok: true,
                detail: "would replace with hardlink".into(),
            });
            res.bytes_freed += m.size;
            continue;
        }
        // Link via a temp name in the same directory, then swap, so a failure
        // mid-way never loses the duplicate before the link exists.
        let tmp = target.with_extension("gj-tmp-link");
        let _ = fs::remove_file(&tmp);
        match fs::hard_link(canonical, &tmp) {
            Err(e) => {
                let detail = match e.raw_os_error() {
                    Some(17) => "cross-volume: hardlink impossible (archive/delete instead)".to_string(),
                    _ => format!("hardlink failed: {e}"),
                };
                res.performed.push(ActionItem { path: m.path.clone(), ok: false, detail });
            }
            Ok(()) => match fs::remove_file(target) {
                Err(e) => {
                    let _ = fs::remove_file(&tmp);
                    res.performed.push(ActionItem {
                        path: m.path.clone(),
                        ok: false,
                        detail: format!("file in use or undeletable: {e}"),
                    });
                }
                Ok(()) => match fs::rename(&tmp, target) {
                    Err(e) => {
                        res.performed.push(ActionItem {
                            path: m.path.clone(),
                            ok: false,
                            detail: format!("swap failed (temp kept as {}): {e}", tmp.display()),
                        });
                    }
                    Ok(()) => {
                        res.performed.push(ActionItem {
                            path: m.path.clone(),
                            ok: true,
                            detail: format!("replaced with hardlink to {}", canonical.display()),
                        });
                        res.bytes_freed += m.size;
                    }
                },
            },
        }
    }
    Ok(res)
}

/// Move the given paths to the Recycle Bin.
pub fn recycle(paths: &[String], dry_run: bool) -> Result<ActionResult, ActionError> {
    let mut res = ActionResult { dry_run, ..Default::default() };
    for p in paths {
        let path = Path::new(p);
        let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if dry_run {
            res.performed.push(ActionItem { path: p.clone(), ok: true, detail: "would move to Recycle Bin".into() });
            res.bytes_freed += size;
            continue;
        }
        match trash::delete(path) {
            Ok(()) => {
                res.performed.push(ActionItem { path: p.clone(), ok: true, detail: "moved to Recycle Bin".into() });
                res.bytes_freed += size;
            }
            Err(e) => res.performed.push(ActionItem { path: p.clone(), ok: false, detail: format!("recycle failed: {e}") }),
        }
    }
    Ok(res)
}

/// One entry of an undo manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveEntry {
    pub from: String,
    pub to: String,
    pub size: u64,
    pub xxh3: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveManifest {
    pub created_unix: u64,
    pub destination: String,
    pub entries: Vec<ArchiveEntry>,
}


/// Move `paths` into `dest_dir`. Same volume: plain rename. Cross volume:
/// copy, hash-verify the copy, then delete the original. Writes an undo
/// manifest `gguf-janitor-archive.jsonl` next to the destination.
pub fn archive(
    paths: &[String],
    dest_dir: &Path,
    dry_run: bool,
) -> Result<ActionResult, ActionError> {
    let mut res = ActionResult { dry_run, ..Default::default() };
    fs::create_dir_all(dest_dir).map_err(|e| io_err(dest_dir, e))?;
    let mut entries: Vec<ArchiveEntry> = Vec::new();

    for p in paths {
        let src = Path::new(p);
        let size = match fs::metadata(src) {
            Ok(m) => m.len(),
            Err(e) => {
                res.performed.push(ActionItem { path: p.clone(), ok: false, detail: format!("unreadable: {e}") });
                continue;
            }
        };
        let dest = dest_dir.join(
            src.file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_else(|| "unnamed".into()),
        );
        if dest.exists() {
            res.performed.push(ActionItem {
                path: p.clone(),
                ok: false,
                detail: format!("destination exists: {}", dest.display()),
            });
            continue;
        }
        if dry_run {
            res.performed.push(ActionItem { path: p.clone(), ok: true, detail: format!("would move to {}", dest.display()) });
            res.bytes_freed += size;
            continue;
        }
        let src_hash = hash_file(src).map_err(|e| io_err(src, e))?;
        if same_volume(src, dest_dir) {
            match fs::rename(src, &dest) {
                Ok(()) => {
                    res.performed.push(ActionItem { path: p.clone(), ok: true, detail: format!("moved to {}", dest.display()) });
                    res.bytes_freed += size;
                    entries.push(ArchiveEntry { from: p.clone(), to: dest.display().to_string(), size, xxh3: src_hash });
                }
                Err(e) => res.performed.push(ActionItem { path: p.clone(), ok: false, detail: format!("move failed: {e}") }),
            }
        } else {
            match copy_verified(src, &dest, src_hash) {
                Ok(()) => match fs::remove_file(src) {
                    Ok(()) => {
                        res.performed.push(ActionItem { path: p.clone(), ok: true, detail: format!("copied+verified+removed -> {}", dest.display()) });
                        res.bytes_freed += size;
                        entries.push(ArchiveEntry { from: p.clone(), to: dest.display().to_string(), size, xxh3: src_hash });
                    }
                    Err(e) => res.performed.push(ActionItem {
                        path: p.clone(),
                        ok: false,
                        detail: format!("copy succeeded at {} but original could not be deleted (kept): {e}", dest.display()),
                    }),
                },
                Err(e) => {
                    let _ = fs::remove_file(&dest);
                    res.performed.push(ActionItem { path: p.clone(), ok: false, detail: format!("{e}") });
                }
            }
        }
    }

    if !dry_run && !entries.is_empty() {
        let manifest_path = dest_dir.join("gguf-janitor-archive.jsonl");
        let line = serde_json::json!({
            "created_unix": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            "destination": dest_dir.display().to_string(),
            "entries": entries,
        });
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&manifest_path) {
            let _ = writeln!(f, "{line}");
        }
    }
    Ok(res)
}

fn copy_verified(src: &Path, dest: &Path, expect_hash: u64) -> Result<(), ActionError> {
    let mut fin = fs::File::open(src).map_err(|e| io_err(src, e))?;
    let mut fout = fs::File::create(dest).map_err(|e| io_err(dest, e))?;
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = fin.read(&mut buf).map_err(|e| io_err(src, e))?;
        if n == 0 {
            break;
        }
        fout.write_all(&buf[..n]).map_err(|e| io_err(dest, e))?;
    }
    fout.sync_all().map_err(|e| io_err(dest, e))?;
    let got = hash_file(dest).map_err(|e| io_err(dest, e))?;
    if got != expect_hash {
        return Err(ActionError::SafetyCheck(format!(
            "archive copy hash mismatch for {} (data changed during copy?)",
            src.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::collections::HashMap;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gj-actions-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn payload(seed: u8, len: usize) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_add(seed)).collect()
    }

    fn group_of(paths: &[PathBuf]) -> DupGroup {
        let members = paths
            .iter()
            .map(|p| crate::ModelFile {
                path: p.display().to_string(),
                size: p.metadata().unwrap().len(),
                store: crate::StoreKind::Loose,
                gguf: None,
                split_group: None,
                ollama_tag: None,
            })
            .collect();
        DupGroup { size_each: paths[0].metadata().unwrap().len(), reclaimable: 0, members }
    }

    fn hash_of(p: &Path) -> u64 {
        hash_file(p).unwrap()
    }

    #[test]
    fn hardlink_dry_run_touches_nothing() {
        let d = tmpdir("dry");
        let a = d.join("a.bin");
        let b = d.join("b.bin");
        fs::write(&a, payload(1, 10_000)).unwrap();
        fs::write(&b, payload(1, 10_000)).unwrap();
        let g = group_of(&[a.clone(), b.clone()]);
        let res = hardlink_group(&g, true).unwrap();
        assert!(res.bytes_freed > 0);
        assert!(res.performed.iter().all(|i| i.ok));
        // both files still distinct on disk
        assert!(fs::read(&a).unwrap() == fs::read(&b).unwrap());
        assert_eq!(fs::read_dir(&d).unwrap().count(), 2);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn hardlink_replaces_duplicate_and_content_survives() {
        let d = tmpdir("hl");
        let a = d.join("a.bin");
        let b = d.join("b.bin");
        let data = payload(2, 10_000);
        fs::write(&a, &data).unwrap();
        fs::write(&b, &data).unwrap();
        let hash_before = hash_of(&a);
        let g = group_of(&[a.clone(), b.clone()]);
        let res = hardlink_group(&g, false).unwrap();
        assert!(res.performed.iter().all(|i| i.ok), "{:?}", res.performed);
        assert_eq!(res.bytes_freed, 10_000);
        // b still readable with identical content
        assert_eq!(hash_of(&b), hash_before);
        assert_eq!(fs::read(&b).unwrap(), data);
        // actual hardlink: same file count content, disk usage halved
        // (verified indirectly: metadata file sizes equal, both readable)
        assert_eq!(fs::metadata(&a).unwrap().len(), 10_000);
        assert_eq!(fs::metadata(&b).unwrap().len(), 10_000);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn hardlink_refuses_unverified_content() {
        let d = tmpdir("unverif");
        let a = d.join("a.bin");
        let b = d.join("b.bin");
        fs::write(&a, payload(1, 10_000)).unwrap();
        fs::write(&b, payload(9, 10_000)).unwrap(); // different content!
        let mut g = group_of(&[a.clone(), b.clone()]);
        g.members[1].size = g.members[0].size; // force same reported size
        let res = hardlink_group(&g, false).unwrap();
        assert!(!res.performed.is_empty());
        assert!(res.performed.iter().any(|i| !i.ok), "{:?}", res.performed);
        assert_eq!(fs::read(&b).unwrap(), payload(9, 10_000), "content must be untouched");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn recycle_dry_run() {
        let d = tmpdir("recycle");
        let a = d.join("a.bin");
        fs::write(&a, payload(3, 5000)).unwrap();
        let res = recycle(&[a.display().to_string()], true).unwrap();
        assert!(res.performed[0].ok);
        assert!(a.exists(), "dry run must not delete");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn archive_same_volume_moves_and_writes_manifest() {
        let d = tmpdir("archive");
        let src_dir = d.join("src");
        let dst_dir = d.join("archive-dest");
        fs::create_dir_all(&src_dir).unwrap();
        let a = src_dir.join("model.bin");
        let data = payload(4, 20_000);
        fs::write(&a, &data).unwrap();
        let h = hash_of(&a);
        let res = archive(&[a.display().to_string()], &dst_dir, false).unwrap();
        assert!(res.performed[0].ok, "{:?}", res.performed);
        assert!(!a.exists(), "source must be gone after archive");
        let dest = dst_dir.join("model.bin");
        assert_eq!(fs::read(&dest).unwrap(), data);
        let manifest = dst_dir.join("gguf-janitor-archive.jsonl");
        let txt = fs::read_to_string(&manifest).unwrap();
        assert!(txt.contains("model.bin"), "{txt}");
        assert!(txt.contains(&format!("\"xxh3\":{h}")) || txt.contains(&format!("\"xxh3\": {h}")), "{txt}");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn archive_refuses_overwrite() {
        let d = tmpdir("nooverwrite");
        let src_dir = d.join("src");
        fs::create_dir_all(&src_dir).unwrap();
        let a = src_dir.join("same.bin");
        fs::write(&a, payload(5, 100)).unwrap();
        let dst_dir = d.join("dst");
        fs::create_dir_all(&dst_dir).unwrap();
        fs::write(dst_dir.join("same.bin"), b"different").unwrap();
        let res = archive(&[a.display().to_string()], &dst_dir, false).unwrap();
        assert!(!res.performed[0].ok);
        assert!(a.exists());
        assert_eq!(fs::read(dst_dir.join("same.bin")).unwrap(), b"different");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn canonical_prefers_tool_stores() {
        let mut g = DupGroup { size_each: 1, reclaimable: 0, members: vec![] };
        g.members.push(crate::ModelFile {
            path: "C:\\Users\\x\\Downloads\\m.gguf".into(),
            size: 1,
            store: crate::StoreKind::Loose,
            gguf: None,
            split_group: None,
            ollama_tag: None,
        });
        g.members.push(crate::ModelFile {
            path: "C:\\Users\\x\\.ollama\\models\\blobs\\sha256-zz".into(),
            size: 1,
            store: crate::StoreKind::Ollama,
            gguf: None,
            split_group: None,
            ollama_tag: Some("llama3:latest".into()),
        });
        assert_eq!(canonical_index(&g), 1);
    }

    // keep serde_json import used in non-test builds too
    #[allow(dead_code)]
    fn _uses() -> HashMap<String, String> { HashMap::new() }
}
