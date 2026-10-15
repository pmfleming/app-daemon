use super::{
    MemoryUsage, parse_memory_rollup, parse_memory_status, parse_process_stat, read_process_memory,
};

#[test]
fn procfs_inputs_preserve_identity_counters_units_and_availability() {
    let stat = "42 (name with ) spaces) S 7 0 0 0 0 0 0 0 17 0 120 30 0 0 0 0 5 0 99 0 0";
    let identity = crate::process::parse_process(stat).unwrap();
    let process = parse_process_stat(stat).unwrap();
    assert_eq!((identity.parent, identity.start), (7, 99));
    assert_eq!((process.parent_pid, process.start_ticks), (7, 99));
    assert_eq!(
        (
            process.total_ticks,
            process.major_faults,
            process.thread_count
        ),
        (150, 17, 5)
    );
    assert!(crate::process::parse_process(&stat.replace(") S", ") Z")).is_none());
    for invalid in [
        "malformed",
        "42 (name) S 7",
        "42 (name) S 7 0 0 0 0 0 0 0 17 0 120 30 0 0 0 0 5 0",
    ] {
        assert!(parse_process_stat(invalid).is_none());
        assert!(crate::process::parse_process(invalid).is_none());
    }
    assert_eq!(
        parse_process_stat(&stat.replace("120 30", &format!("{} 30", u64::MAX)))
            .unwrap()
            .total_ticks,
        u64::MAX
    );
    assert!(parse_process_stat(&stat.replace("120 30", "invalid 30")).is_none());

    let overflow = format!(
        "Rss: {} kB\nPrivate_Clean: {} kB\nPrivate_Dirty: 1 kB\nSwap: 9 kB\n",
        u64::MAX,
        u64::MAX,
    );
    for (parse, input, expected) in [
        (
            parse_memory_rollup as fn(&str) -> MemoryUsage,
            "Rss: 8 kB\nPss: 4 kB\nPrivate_Clean: 1 kB\nPrivate_Dirty: 2 kB\nPrivate_Hugetlb: 3 kB\nSwapPss: 5 kB\nSwap: 9 kB\n",
            (8192, 4096, 6144, 5120, true, true),
        ),
        (
            parse_memory_rollup,
            "Rss: bad\nPss: 0 kB\nSwapPss: 0 kB\nSwap: 9 kB",
            (0, 0, 0, 0, false, true),
        ),
        (
            parse_memory_rollup,
            overflow.as_str(),
            (u64::MAX, 0, u64::MAX, 9216, true, false),
        ),
        (parse_memory_rollup, "", (0, 0, 0, 0, false, false)),
        (
            parse_memory_status,
            "VmRSS: 7 kB\nVmSwap: 11 kB",
            (7168, 0, 0, 11264, true, false),
        ),
        (
            parse_memory_status,
            "VmRSS: 0 kB\nVmSwap: invalid",
            (0, 0, 0, 0, true, false),
        ),
        (
            parse_memory_status,
            "VmRSS: -1 kB\nmalformed",
            (0, 0, 0, 0, false, false),
        ),
    ] {
        let usage = parse(input);
        assert_eq!(
            (
                usage.rss_bytes,
                usage.pss_bytes,
                usage.private_bytes,
                usage.swap_bytes,
                usage.rss_available,
                usage.pss_available
            ),
            expected,
            "{input:?}",
        );
    }
    let missing = read_process_memory(u32::MAX);
    assert!(!missing.rss_available && !missing.pss_available);
}
