use super::{AllocatedFiles, DiskFileId, DiskScanBudget, allocated_directory_bytes};
use std::{fs, os::unix::fs::MetadataExt};

#[test]
fn disk_walks_count_unique_owned_files_and_never_publish_partial_sizes() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let file = directory.path().join("one");
    fs::write(&file, vec![1; 4096])?;
    fs::hard_link(&file, directory.path().join("two"))?;
    let external = tempfile::tempdir()?;
    fs::write(external.path().join("outside"), vec![1; 8192])?;
    std::os::unix::fs::symlink(external.path(), directory.path().join("link"))?;
    let roots = [
        directory.path().to_owned(),
        directory.path().to_owned(), // Repeated roots must not double-count files.
        directory.path().join("missing"),
        directory.path().join("link"), // Root symlinks must not be followed either.
        file.clone(),                  // A regular file is not an application directory.
    ];
    let total = allocated_directory_bytes(&roots, &mut DiskScanBudget::default());
    let metadata = fs::metadata(file)?;
    assert_eq!(total, Some(metadata.blocks() * 512));
    let mut files = AllocatedFiles::default();
    files
        .scan_directory(directory.path(), &mut DiskScanBudget::default())
        .expect("complete scan");
    let id = DiskFileId {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    files.include(id, u64::MAX);
    assert_eq!(
        Some(files.bytes),
        total,
        "repeat observations cannot replace an inode's size"
    );
    files.include(
        DiskFileId {
            device: id.device.wrapping_add(1),
            ..id
        },
        u64::MAX,
    );
    assert_eq!(
        files.bytes,
        u64::MAX,
        "different devices count independently and totals saturate"
    );
    for mut budget in [
        DiskScanBudget {
            remaining_entries: 2,
            ..Default::default()
        },
        DiskScanBudget {
            deadline: std::time::Instant::now(),
            ..Default::default()
        },
    ] {
        assert!(allocated_directory_bytes(&roots, &mut budget).is_none());
    }
    Ok(())
}
