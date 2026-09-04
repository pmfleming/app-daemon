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
