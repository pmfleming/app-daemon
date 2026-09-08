use super::{DiskScanBudget, allocated_directory_bytes};
use std::{fs, os::unix::fs::MetadataExt};

#[test]
fn incomplete_directory_walks_never_publish_partial_sizes() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("one"), vec![1; 4096])?;
    fs::write(directory.path().join("two"), vec![1; 4096])?;
    let roots = [directory.path().to_owned()];
    let mut budget = DiskScanBudget {
        remaining_entries: 2,
        ..Default::default()
    };
    assert!(allocated_directory_bytes(&roots, &mut budget).is_none());
    let mut budget = DiskScanBudget {
        deadline: std::time::Instant::now(),
        ..Default::default()
    };
    assert!(allocated_directory_bytes(&roots, &mut budget).is_none());
    Ok(())
}

#[test]
fn disk_walks_deduplicate_hardlinks_and_do_not_follow_symlinks() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let file = directory.path().join("one");
    fs::write(&file, vec![1; 4096])?;
    fs::hard_link(&file, directory.path().join("two"))?;
    let external = tempfile::tempdir()?;
    fs::write(external.path().join("outside"), vec![1; 8192])?;
    std::os::unix::fs::symlink(external.path(), directory.path().join("link"))?;
    let roots = [directory.path().to_owned()];
    let total = allocated_directory_bytes(&roots, &mut DiskScanBudget::default());
    assert_eq!(total, Some(fs::metadata(file)?.blocks() * 512));
    Ok(())
}
