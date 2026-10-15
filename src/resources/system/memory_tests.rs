use super::{MemoryUsage, parse_memory_rollup, parse_memory_status, read_process_memory};

#[test]
fn memory_parsers_preserve_units_availability_and_swap_precedence() {
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
