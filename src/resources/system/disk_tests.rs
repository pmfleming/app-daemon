use super::{AllocatedFiles, DiskFileId, DiskScanBudget, allocated_directory_bytes};
use std::{fs, os::unix::fs::MetadataExt};

#[test]
fn allocated_totals_saturate_and_deduplicate_by_device_and_inode() {
    let mut files = AllocatedFiles::default();
    let first = DiskFileId {
        device: 1,
        inode: 1,
    };
    files.include(first, 10);
    files.include(first, 999);
    assert_eq!(
        files.bytes, 10,
        "only the first observation of an inode counts"
    );
    files.include(
        DiskFileId {
            device: 2,
            inode: 1,
        },
        20,
    );
    assert_eq!(files.bytes, 30, "inode numbers are scoped to their device");
    files.include(
        DiskFileId {
            device: 1,
            inode: 2,
        },
        u64::MAX,
    );
    assert_eq!(files.bytes, u64::MAX);
}

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
    assert_eq!(total, Some(fs::metadata(file)?.blocks() * 512));
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
