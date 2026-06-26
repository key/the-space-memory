//! Low-level filesystem placement helpers shared by `tsm setup` (cache
//! population) and `tsm init` (workspace scaffolding) — ADR-0008.
//!
//! Each `place_*` call is idempotent and converges `dst` to `mode`: a missing
//! parent is created, and an existing destination (symlink, file, or directory)
//! is replaced. `Copy` placement is two-phase — the new entry is materialized
//! into a temporary sibling and swapped in with an atomic `rename`, so a failed
//! or interrupted copy never destroys the previously working entry.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::LinkMode;

/// Materialize `dst` as a directory reference to `src` per `mode`
/// (`Symlink` → directory symlink, `Copy` → recursive physical copy).
pub fn place_directory(src: &Path, dst: &Path, mode: LinkMode) -> Result<()> {
    place(src, dst, mode, copy_dir_all)
}

/// Materialize `dst` as a file reference to `src` per `mode`
/// (`Symlink` → symlink, `Copy` → physical copy).
pub fn place_file(src: &Path, dst: &Path, mode: LinkMode) -> Result<()> {
    place(src, dst, mode, |s, d| {
        std::fs::copy(s, d)
            .map(|_| ())
            .with_context(|| format!("copying {} -> {}", s.display(), d.display()))
    })
}

/// Shared placement core. `materialize` writes `src`'s content to a fresh path.
/// `Copy` writes into a temp sibling first, then atomically renames it over
/// `dst`; `Symlink` replaces `dst` with a symlink directly.
fn place(
    src: &Path,
    dst: &Path,
    mode: LinkMode,
    materialize: impl Fn(&Path, &Path) -> Result<()>,
) -> Result<()> {
    ensure_parent(dst)?;
    match mode {
        LinkMode::Symlink => {
            remove_path(dst)?;
            std::os::unix::fs::symlink(src, dst)
                .with_context(|| format!("symlinking {} -> {}", dst.display(), src.display()))?;
        }
        LinkMode::Copy => {
            let tmp = temp_sibling(dst);
            remove_path(&tmp)?;
            if let Err(e) = materialize(src, &tmp) {
                // Leave the existing `dst` untouched; just clean the partial temp.
                let _ = remove_path(&tmp);
                return Err(e);
            }
            // Temp is fully materialized — now swap it in. Removing the old entry
            // immediately before the rename keeps the failure window minimal.
            remove_path(dst)?;
            std::fs::rename(&tmp, dst)
                .with_context(|| format!("renaming {} -> {}", tmp.display(), dst.display()))?;
        }
    }
    Ok(())
}

/// Create `dst`'s parent directory (and ancestors) if absent.
fn ensure_parent(dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent dir {}", parent.display()))?;
    }
    Ok(())
}

/// A sibling path of `dst` used as the staging target for atomic `Copy` swaps.
/// Same parent as `dst` so the final `rename` stays on one filesystem (no EXDEV).
fn temp_sibling(dst: &Path) -> PathBuf {
    let parent = dst.parent().unwrap_or_else(|| Path::new("."));
    let mut name = dst
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".tmp.{}", std::process::id()));
    parent.join(name)
}

/// Remove `p` whether it is a symlink (even a dangling one), a regular file,
/// or a directory. A non-existent path is a no-op.
pub fn remove_path(p: &Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(p) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("stat {}", p.display())),
    };
    let file_type = meta.file_type();
    if file_type.is_dir() {
        std::fs::remove_dir_all(p).with_context(|| format!("removing dir {}", p.display()))?;
    } else {
        // Regular files and symlinks (including dangling) go through remove_file;
        // a directory symlink's link is removed, not its target.
        std::fs::remove_file(p).with_context(|| format!("removing {}", p.display()))?;
    }
    Ok(())
}

/// Recursively copy directory `src` into `dst`, creating `dst` and any parents.
/// Symlinks are dereferenced: file symlinks become physical files and directory
/// symlinks become physical directories, so a `copy` of a symlink-based tree
/// (e.g. a HuggingFace snapshot whose files link to blobs) is self-contained.
pub fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("creating dir {}", dst.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("reading dir {}", src.display()))? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        // Follow symlinks so blob-linked snapshot files materialize as bytes.
        let meta = std::fs::metadata(&from).with_context(|| format!("stat {}", from.display()))?;
        if meta.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("copying {} -> {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

/// Total size in bytes of `path`: its own length if a file, else the recursive
/// sum of all regular files beneath it. Symlinks are followed. This is the
/// single size rule shared by `tsm setup` (writing the manifest) and
/// `tsm doctor` (verifying it), so the two never disagree (ADR-0008).
pub fn tree_size(path: &Path) -> Result<u64> {
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if !meta.is_dir() {
        return Ok(meta.len());
    }
    let mut total = 0;
    for entry in
        std::fs::read_dir(path).with_context(|| format!("reading dir {}", path.display()))?
    {
        total += tree_size(&entry?.path())?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn make_dir_with_file(root: &Path, name: &str, contents: &str) -> std::path::PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), contents).unwrap();
        dir
    }

    #[test]
    fn place_directory_symlink_creates_symlink_to_src() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = make_dir_with_file(tmp.path(), "src", "hello");
        let dst = tmp.path().join("dst");

        place_directory(&src, &dst, LinkMode::Symlink).unwrap();

        assert!(dst.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(dst.join("f.txt")).unwrap(), "hello");
        assert_eq!(std::fs::read_link(&dst).unwrap(), src);
    }

    #[test]
    fn place_directory_copy_creates_physical_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = make_dir_with_file(tmp.path(), "src", "hello");
        let dst = tmp.path().join("dst");

        place_directory(&src, &dst, LinkMode::Copy).unwrap();

        let meta = dst.symlink_metadata().unwrap();
        assert!(meta.file_type().is_dir());
        assert!(!meta.file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(dst.join("f.txt")).unwrap(), "hello");
    }

    #[test]
    fn place_directory_is_idempotent_and_replaces_existing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = make_dir_with_file(tmp.path(), "src", "hello");
        let dst = tmp.path().join("dst");

        // First as a copy, then re-place as a symlink — must converge.
        place_directory(&src, &dst, LinkMode::Copy).unwrap();
        place_directory(&src, &dst, LinkMode::Symlink).unwrap();

        assert!(dst.symlink_metadata().unwrap().file_type().is_symlink());
        // Re-running symlink mode again stays green.
        place_directory(&src, &dst, LinkMode::Symlink).unwrap();
        assert_eq!(std::fs::read_to_string(dst.join("f.txt")).unwrap(), "hello");
    }

    #[test]
    fn place_file_symlink_creates_symlink_to_src() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src.db");
        std::fs::write(&src, "data").unwrap();
        let dst = tmp.path().join("dst.db");

        place_file(&src, &dst, LinkMode::Symlink).unwrap();

        assert!(dst.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "data");
    }

    #[test]
    fn place_file_copy_creates_physical_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src.db");
        std::fs::write(&src, "data").unwrap();
        let dst = tmp.path().join("dst.db");

        place_file(&src, &dst, LinkMode::Copy).unwrap();

        let meta = dst.symlink_metadata().unwrap();
        assert!(meta.file_type().is_file());
        assert!(!meta.file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "data");
    }

    #[test]
    fn place_file_replaces_existing_symlink() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src.db");
        std::fs::write(&src, "data").unwrap();
        let dst = tmp.path().join("dst.db");

        place_file(&src, &dst, LinkMode::Symlink).unwrap();
        place_file(&src, &dst, LinkMode::Copy).unwrap();

        assert!(dst.symlink_metadata().unwrap().file_type().is_file());
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "data");
    }

    #[test]
    fn remove_path_handles_dangling_symlink() {
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("missing");
        let link = tmp.path().join("link");
        symlink(&missing, &link).unwrap();

        remove_path(&link).unwrap();

        assert!(link.symlink_metadata().is_err());
    }

    #[test]
    fn remove_path_handles_file_dir_and_absent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let file = tmp.path().join("f");
        std::fs::write(&file, "x").unwrap();
        let dir = make_dir_with_file(tmp.path(), "d", "x");
        let absent = tmp.path().join("absent");

        remove_path(&file).unwrap();
        remove_path(&dir).unwrap();
        remove_path(&absent).unwrap(); // no-op, must not error

        assert!(!file.exists());
        assert!(!dir.exists());
    }

    #[test]
    fn copy_dir_all_copies_nested_tree() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("a.txt"), "a").unwrap();
        std::fs::write(src.join("nested/b.txt"), "b").unwrap();
        let dst = tmp.path().join("out/dst");

        copy_dir_all(&src, &dst).unwrap();

        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "a");
        assert_eq!(
            std::fs::read_to_string(dst.join("nested/b.txt")).unwrap(),
            "b"
        );
    }

    // C1: dst whose parent does not yet exist (the real `init` case for
    // `state_dir/models/ruri-v3-30m`, where `state_dir/models/` is absent).
    #[test]
    fn place_directory_creates_missing_parent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = make_dir_with_file(tmp.path(), "src", "hello");
        let dst = tmp.path().join("a/b/c/dst");

        place_directory(&src, &dst, LinkMode::Symlink).unwrap();
        assert!(dst.symlink_metadata().unwrap().file_type().is_symlink());

        let dst2 = tmp.path().join("x/y/z/dst");
        place_directory(&src, &dst2, LinkMode::Copy).unwrap();
        assert_eq!(
            std::fs::read_to_string(dst2.join("f.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn place_file_creates_missing_parent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src.db");
        std::fs::write(&src, "data").unwrap();
        let dst = tmp.path().join("deep/nested/dst.db");

        place_file(&src, &dst, LinkMode::Copy).unwrap();

        assert!(dst.symlink_metadata().unwrap().file_type().is_file());
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "data");
    }

    // S3: copying a tree whose entries are symlinks (HF snapshot → blob layout)
    // must materialize physical bytes, not re-create the symlinks.
    #[test]
    fn copy_dir_all_dereferences_file_symlinks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let blobs = tmp.path().join("blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(blobs.join("blob"), "real-bytes").unwrap();

        // snapshot/ contains a file-symlink into blobs/ (like a HF snapshot dir).
        let snapshot = tmp.path().join("snapshot");
        std::fs::create_dir_all(&snapshot).unwrap();
        symlink(blobs.join("blob"), snapshot.join("config.json")).unwrap();

        let dst = tmp.path().join("dst");
        copy_dir_all(&snapshot, &dst).unwrap();

        let placed = dst.join("config.json");
        assert!(
            placed.symlink_metadata().unwrap().file_type().is_file(),
            "copied entry must be a physical file, not a symlink"
        );
        assert_eq!(std::fs::read_to_string(&placed).unwrap(), "real-bytes");
    }

    // S3 end-to-end: place_directory(Copy) over a directory whose contents are
    // symlinks yields a self-contained physical directory.
    #[test]
    fn place_directory_copy_dereferences_symlinked_contents() {
        let tmp = tempfile::TempDir::new().unwrap();
        let blobs = tmp.path().join("blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(blobs.join("blob"), "model-bytes").unwrap();
        let snapshot = tmp.path().join("snapshot");
        std::fs::create_dir_all(&snapshot).unwrap();
        symlink(blobs.join("blob"), snapshot.join("model.safetensors")).unwrap();
        let dst = tmp.path().join("out/model");

        place_directory(&snapshot, &dst, LinkMode::Copy).unwrap();

        let placed = dst.join("model.safetensors");
        assert!(placed.symlink_metadata().unwrap().file_type().is_file());
        assert_eq!(std::fs::read_to_string(&placed).unwrap(), "model-bytes");
    }

    // S6: a failed Copy replacement must leave the previously working entry
    // intact (two-phase: temp materialize → atomic rename).
    #[test]
    fn place_directory_copy_failure_preserves_existing_dst() {
        let tmp = tempfile::TempDir::new().unwrap();
        let good_src = make_dir_with_file(tmp.path(), "good", "good-bytes");
        let dst = tmp.path().join("dst");

        // Establish a working entry.
        place_directory(&good_src, &dst, LinkMode::Copy).unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.join("f.txt")).unwrap(),
            "good-bytes"
        );

        // A subsequent copy from a non-existent source must fail without
        // destroying the working dst, and must not leave a temp behind.
        let missing_src = tmp.path().join("does-not-exist");
        let err = place_directory(&missing_src, &dst, LinkMode::Copy);
        assert!(err.is_err());
        assert_eq!(
            std::fs::read_to_string(dst.join("f.txt")).unwrap(),
            "good-bytes",
            "failed copy must preserve the existing entry"
        );
        let leftover = temp_sibling(&dst);
        assert!(
            leftover.symlink_metadata().is_err(),
            "partial temp must be cleaned up on failure"
        );
    }

    // S2: the size rule shared by setup and doctor — recursive sum of regular
    // files, following symlinks; plain file returns its own length.
    #[test]
    fn tree_size_sums_files_recursively_following_symlinks() {
        let tmp = tempfile::TempDir::new().unwrap();

        let file = tmp.path().join("solo.bin");
        std::fs::write(&file, "abc").unwrap();
        assert_eq!(tree_size(&file).unwrap(), 3);

        let blobs = tmp.path().join("blobs");
        std::fs::create_dir_all(&blobs).unwrap();
        std::fs::write(blobs.join("blob"), "xxxxx").unwrap(); // 5 bytes

        let dir = tmp.path().join("dir");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a"), "12").unwrap(); // 2
        std::fs::write(dir.join("sub/b"), "345").unwrap(); // 3
        symlink(blobs.join("blob"), dir.join("link")).unwrap(); // follows → 5

        assert_eq!(tree_size(&dir).unwrap(), 2 + 3 + 5);
    }
}
