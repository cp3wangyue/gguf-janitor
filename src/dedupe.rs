//! Content-duplicate detection for scanned model files.
//!
//! Two-stage: group by exact size, stream-hash (XXH3-64) the members of each
//! size group, then byte-verify head+tail before declaring a duplicate group.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde::Serialize;
use xxhash_rust::xxh3::Xxh3;

use crate::{ModelFile, ScanProgress};

#[derive(Debug, Clone, Serialize)]
pub struct DupGroup {
    /// Size of every member in bytes.
    pub size_each: u64,
    /// Reclaimable size if all but one member are deduplicated.
    pub reclaimable: u64,
    pub members: Vec<ModelFile>,
}

#[derive(Debug, Clone)]
pub struct DedupeOptions {
    /// Files smaller than this are skipped (default 8 MiB).
    pub min_file_size: u64,
    /// Report hashing progress every N bytes.
    pub progress_every: u64,
}

impl Default for DedupeOptions {
    fn default() -> Self {
        DedupeOptions { min_file_size: 8 * 1024 * 1024, progress_every: 64 * 1024 * 1024 }
    }
}

/// Stream XXH3-64 over a file. Returns Err on IO problems (e.g. locked file).
pub fn hash_file(path: &Path) -> std::io::Result<u64> {
    let mut f = File::open(path)?;
    let mut h = Xxh3::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.digest())
}

/// Cheap identity check: first and last 4 KiB must be byte-identical.
pub fn head_tail_equal(a: &Path, b: &Path) -> std::io::Result<bool> {
    const CHUNK: u64 = 4096;
    let fa = File::open(a)?;
    let fb = File::open(b)?;
    let len = fa.metadata()?.len();
    if fb.metadata()?.len() != len {
        return Ok(false);
    }
    let mut ba = Vec::new();
    let mut bb = Vec::new();
    let mut fa = fa;
    let mut fb = fb;
    let head = CHUNK.min(len);
    fa.seek(SeekFrom::Start(0))?;
    fb.seek(SeekFrom::Start(0))?;
    (&mut fa).take(head).read_to_end(&mut ba)?;
    (&mut fb).take(head).read_to_end(&mut bb)?;
    if ba != bb {
        return Ok(false);
    }
    if len > head {
        let tail_start = len - head;
        let mut ta = Vec::new();
        let mut tb = Vec::new();
        fa.seek(SeekFrom::Start(tail_start))?;
        fb.seek(SeekFrom::Start(tail_start))?;
        fa.read_to_end(&mut ta)?;
        fb.read_to_end(&mut tb)?;
        if ta != tb {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Find groups of content-identical files.
///
/// `progress` receives hashing progress; hashing order is deterministic
/// (grouped by descending size) so the biggest wins appear first.
pub fn find_duplicates(
    files: &[ModelFile],
    opts: &DedupeOptions,
    progress: &mut impl FnMut(&ScanProgress),
    should_stop: &impl Fn() -> bool,
) -> anyhow::Result<Vec<DupGroup>> {
    // 1. Group by exact size.
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    for (i, f) in files.iter().enumerate() {
        if f.size >= opts.min_file_size {
            by_size.entry(f.size).or_default().push(i);
        }
    }
    let mut candidate_groups: Vec<(u64, Vec<usize>)> = by_size
        .into_iter()
        .filter(|(_, v)| v.len() > 1)
        .collect();
    candidate_groups.sort_by(|a, b| b.0.cmp(&a.0).then(a.1[0].cmp(&b.1[0])));

    let total_bytes: u64 = candidate_groups.iter().map(|(s, v)| s * v.len() as u64).sum();
    let mut bytes_done: u64 = 0;
    let mut files_done: u64 = 0;
    let files_total = candidate_groups.iter().map(|(_, v)| v.len() as u64).sum();

    let mut hash_cache: HashMap<String, u64> = HashMap::new();
    let mut out = Vec::new();

    for (size, idxs) in candidate_groups {
        if should_stop() {
            break;
        }
        // 2. Hash every member.
        let mut by_hash: HashMap<u64, Vec<usize>> = HashMap::new();
        for &i in &idxs {
            let path = files[i].path.clone();
            let h = if let Some(cached) = hash_cache.get(&path) {
                Some(*cached)
            } else {
                match hash_file(Path::new(&path)) {
                    Ok(hv) => {
                        hash_cache.insert(path, hv);
                        Some(hv)
                    }
                    // Unreadable (locked/disappeared): exclude from grouping.
                    Err(_) => None,
                }
            };
            files_done += 1;
            if let Some(hv) = h {
                by_hash.entry(hv).or_default().push(i);
            }
            progress(&ScanProgress {
                phase: crate::ProgressPhase::Hashing,
                files_done,
                files_total,
                bytes_done,
                bytes_total: total_bytes,
                current: files[i].path.clone(),
            });
        }
        bytes_done += size * idxs.len() as u64;

        // 3. Verify head/tail byte equality before trusting the hash; members
        // that fail verification (improbable 64-bit collision or mid-write
        // file) are excluded rather than trusted.
        for (_, members) in by_hash.into_iter().filter(|(_, m)| m.len() > 1) {
            let mut verified: Vec<usize> = vec![members[0]];
            'outer: for &m in &members[1..] {
                for &v in &verified {
                    if head_tail_equal(Path::new(&files[m].path), Path::new(&files[v].path))? {
                        verified.push(m);
                        continue 'outer;
                    }
                }
                // Matches no verified member: treat as distinct, drop it.
            }
            if verified.len() > 1 {
                let group_members: Vec<ModelFile> =
                    verified.iter().map(|&i| files[i].clone()).collect();
                out.push(DupGroup {
                    size_each: size,
                    reclaimable: size * (verified.len() as u64 - 1),
                    members: group_members,
                });
            }
        }
    }
    out.sort_by(|a, b| b.reclaimable.cmp(&a.reclaimable));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::io::Write;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("gj-dedupe-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(path: &Path, bytes: &[u8]) {
        let mut f = fs::File::create(path).unwrap();
        f.write_all(bytes).unwrap();
    }

    fn mf(path: &Path) -> ModelFile {
        ModelFile {
            path: path.display().to_string(),
            size: path.metadata().unwrap().len(),
            store: crate::StoreKind::Custom,
            gguf: None,
            split_group: None,
            ollama_tag: None,
        }
    }

    fn nop(_: &ScanProgress) {}
    fn stop() -> bool { false }

    #[test]
    fn finds_identical_and_distinguishes_close_files() {
        let d = tmpdir("basic");
        // 12 MiB payloads so they clear the default min size; use a custom
        // min size instead to keep the test fast.
        let a_path = d.join("a.bin");
        let b_path = d.join("b.bin");
        let c_path = d.join("c.bin");
        let payload: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
        write(&a_path, &payload);
        write(&b_path, &payload); // identical content, different name
        let mut c = payload.clone();
        c[500_000] ^= 0xFF; // one byte differs
        write(&c_path, &c);

        let files = vec![mf(&a_path), mf(&b_path), mf(&c_path)];
        let opts = DedupeOptions { min_file_size: 1000, ..Default::default() };
        let groups = find_duplicates(&files, &opts, &mut nop, &stop).unwrap();
        assert_eq!(groups.len(), 1, "{groups:?}");
        let g = &groups[0];
        assert_eq!(g.members.len(), 2);
        assert_eq!(g.size_each, 1_000_000);
        assert_eq!(g.reclaimable, 1_000_000);
        let mut names: Vec<String> = g.members.iter().map(|m| m.path.clone()).collect();
        names.sort();
        assert_eq!(names, vec![a_path.display().to_string(), b_path.display().to_string()]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn head_tail_check_catches_collisions() {
        // Same size, same head+tail, different middle: hash may or may not
        // collide, but head_tail_equal must be true and full content differs.
        let d = tmpdir("collide");
        let x = d.join("x.bin");
        let y = d.join("y.bin");
        let mut p1: Vec<u8> = vec![7u8; 100_000];
        let mut p2 = p1.clone();
        p1[50_000] = 1;
        p2[50_000] = 2;
        write(&x, &p1);
        write(&y, &p2);
        assert!(head_tail_equal(&x, &y).unwrap());
        assert_ne!(hash_file(&x).unwrap(), hash_file(&y).unwrap());
        let files = vec![mf(&x), mf(&y)];
        let opts = DedupeOptions { min_file_size: 1000, ..Default::default() };
        assert!(find_duplicates(&files, &opts, &mut nop, &stop).unwrap().is_empty());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn min_size_filters_small_files() {
        let d = tmpdir("small");
        let a = d.join("a.bin");
        let b = d.join("b.bin");
        write(&a, b"same");
        write(&b, b"same");
        let files = vec![mf(&a), mf(&b)];
        let groups = find_duplicates(&files, &DedupeOptions::default(), &mut nop, &stop).unwrap();
        assert!(groups.is_empty());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn split_shards_never_match_themselves() {
        // A single split group must not be reported as duplicates of each other
        // when shards differ; identical shards (pathological) would be — that
        // is genuinely reclaimable.
        let d = tmpdir("split");
        let s1 = d.join("m-00001-of-00002.gguf");
        let s2 = d.join("m-00002-of-00002.gguf");
        let mut p1: Vec<u8> = (0..50_000u32).map(|i| i as u8).collect();
        let p2 = p1.clone();
        p1[10] = 9;
        write(&s1, &p1);
        write(&s2, &p2);
        let mut f1 = mf(&s1);
        let mut f2 = mf(&s2);
        f1.split_group = Some("m".into());
        f2.split_group = Some("m".into());
        let opts = DedupeOptions { min_file_size: 1000, ..Default::default() };
        let groups = find_duplicates(&[f1, f2], &opts, &mut nop, &stop).unwrap();
        assert!(groups.is_empty());
        let _ = fs::remove_dir_all(&d);
    }
}
