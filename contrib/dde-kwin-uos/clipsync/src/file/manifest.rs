//! 递归遍历一组顶层路径，产出扁平化 manifest（跨平台纯逻辑）。

use crate::proto::{Entry, Manifest, MAX_MANIFEST_ENTRIES};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct BuiltOffer {
    pub manifest: Manifest,
    /// file_id → 绝对路径（仅非目录条目）
    pub paths: HashMap<u32, PathBuf>,
}

const MAX_DEPTH: usize = 64;

/// 输入是用户复制的一组顶层路径（文件或目录），产出扁平清单。
pub fn build_manifest(session_id: u32, roots: &[PathBuf]) -> BuiltOffer {
    let mut entries = Vec::new();
    let mut paths = HashMap::new();
    let mut next_id: u32 = 0;
    for root in roots {
        // relpath 以顶层名开头：复制 /x/myfolder → "myfolder/..."; 复制 /x/a.txt → "a.txt"
        let base = root.parent().unwrap_or(Path::new("/"));
        walk(root, base, &mut entries, &mut paths, &mut next_id, 0);
    }
    BuiltOffer {
        manifest: Manifest {
            session_id,
            entries,
        },
        paths,
    }
}

fn walk(
    p: &Path,
    base: &Path,
    entries: &mut Vec<Entry>,
    paths: &mut HashMap<u32, PathBuf>,
    next_id: &mut u32,
    depth: usize,
) {
    if entries.len() >= MAX_MANIFEST_ENTRIES || depth > MAX_DEPTH {
        return;
    }
    let meta = match std::fs::symlink_metadata(p) {
        Ok(m) => m,
        Err(_) => return,
    };
    if meta.file_type().is_symlink() {
        return; // 不跟随 symlink，防环
    }
    let relpath = match p.strip_prefix(base) {
        Ok(r) => r.to_string_lossy().replace('\\', "/"),
        Err(_) => return,
    };
    if meta.is_dir() {
        let id = *next_id;
        *next_id += 1;
        entries.push(Entry {
            file_id: id,
            relpath,
            size: 0,
            is_dir: true,
        });
        if let Ok(rd) = std::fs::read_dir(p) {
            let mut kids: Vec<_> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
            kids.sort();
            for k in kids {
                walk(&k, base, entries, paths, next_id, depth + 1);
            }
        }
    } else if meta.is_file() {
        let id = *next_id;
        *next_id += 1;
        entries.push(Entry {
            file_id: id,
            relpath,
            size: meta.len(),
            is_dir: false,
        });
        paths.insert(id, p.to_path_buf());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn walk_dir_produces_relpaths_and_dir_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("myfolder");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), b"hello").unwrap();
        fs::write(root.join("sub/b.bin"), b"xy").unwrap();
        fs::create_dir(root.join("empty")).unwrap();

        let built = build_manifest(99, &[root.clone()]);
        let by_rel: std::collections::HashMap<_, _> = built
            .manifest
            .entries
            .iter()
            .map(|e| (e.relpath.clone(), e.clone()))
            .collect();

        assert!(by_rel["myfolder"].is_dir);
        assert!(by_rel["myfolder/empty"].is_dir); // 空目录也有条目
        assert_eq!(by_rel["myfolder/a.txt"].size, 5);
        assert_eq!(by_rel["myfolder/sub/b.bin"].size, 2);
        // file_id → 绝对路径 映射覆盖所有非目录条目
        for e in built.manifest.entries.iter().filter(|e| !e.is_dir) {
            assert!(built.paths.get(&e.file_id).is_some());
        }
    }
}
