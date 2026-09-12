use super::*;

#[test]
fn reads_cgroup_controllers_independently_and_distinguishes_idle_from_missing() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    assert!(read_cgroup_counters_at(root).is_none());
    fs::write(root.join("cpu.stat"), "usage_usec 123\n")?;
    let cpu = read_cgroup_counters_at(root).unwrap();
    assert_eq!(cpu.cpu_usage_usec, Some(123));
    assert!(cpu.io.is_none());
    assert!(cpu.memory_bytes.is_none());
    fs::remove_file(root.join("cpu.stat"))?;
    fs::write(root.join("memory.current"), "456\n")?;
    fs::write(root.join("io.stat"), "")?;
    let idle = read_cgroup_counters_at(root).unwrap();
    assert!(idle.cpu_usage_usec.is_none());
    assert_eq!(idle.memory_bytes, Some(456));
    assert_eq!(idle.io.unwrap().read_bytes, 0);
    fs::write(root.join("io.stat"), "8:0 rbytes=invalid\n")?;
    assert!(read_cgroup_counters_at(root).unwrap().io.is_none());
    fs::write(
        root.join("io.stat"),
        "8:0 rbytes=10 wbytes=20 rios=1 wios=2\n",
    )?;
    assert_eq!(
        read_cgroup_counters_at(root)
            .unwrap()
            .io
            .unwrap()
            .write_bytes,
        20
    );
    Ok(())
}
