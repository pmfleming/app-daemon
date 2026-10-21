/// Opaque revisions cross JSON into JavaScript; retain exact integer round trips.
pub(crate) const fn json_revision(value: u64) -> u64 {
    value & ((1_u64 << 53) - 1)
}

/// Stable within a build and independent of hash-map iteration order.
pub(crate) fn unordered_revision<T: Ord + std::hash::Hash>(
    values: impl IntoIterator<Item = T>,
) -> u64 {
    let mut values = values.into_iter().collect::<Vec<_>>();
    values.sort_unstable();
    revision(&values)
}

/// Shared hashing for catalog, compositor and action-precondition revisions.
pub(crate) fn revision(value: &(impl std::hash::Hash + ?Sized)) -> u64 {
    use std::hash::Hasher;
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hash);
    hash.finish()
}

pub(crate) fn add_counter(counter: &mut u64, value: u64) {
    *counter = counter.saturating_add(value);
}

pub(crate) fn finite_nonnegative(value: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

pub(crate) fn rate(numerator: f64, denominator: f64, decimals: i32) -> f64 {
    if denominator > 0.0 {
        rounded(numerator / denominator, decimals)
    } else {
        0.0
    }
}

pub(crate) fn rounded(value: f64, decimals: i32) -> f64 {
    let scale = 10_f64.powi(decimals);
    (finite_nonnegative(value) * scale).round() / scale
}

pub(crate) fn merge_label(current: &mut String, next: &str) {
    if next.is_empty() || next == "unavailable" {
        return;
    }
    if current.is_empty() {
        current.push_str(next);
    } else if current != next {
        *current = "mixed".into();
    }
}

pub(crate) fn available_label(value: String) -> String {
    if value.is_empty() {
        "unavailable".into()
    } else {
        value
    }
}
