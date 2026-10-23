use shelllist_daemon_core::fixed_decimal;
pub(super) fn energy_label(mwh: f64) -> String {
    if !mwh.is_finite() || mwh < 0.0 {
        return "Unavailable".into();
    }
    if mwh >= 1000.0 {
        format!("{} Wh", fixed_decimal(mwh / 1000.0, 2).unwrap())
    } else if mwh >= 100.0 {
        format!("{} mWh", fixed_decimal(mwh, 0).unwrap())
    } else {
        format!("{} mWh", fixed_decimal(mwh, 1).unwrap())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn units_change_only_at_one_wh_and_unknown_is_not_zero() {
        assert_eq!(energy_label(0.0), "0.0 mWh");
        assert_eq!(energy_label(12.34), "12.3 mWh");
        assert_eq!(energy_label(100.0), "100 mWh");
        assert_eq!(energy_label(999.0), "999 mWh");
        assert_eq!(energy_label(1250.0), "1.25 Wh");
        assert_eq!(energy_label(f64::NAN), "Unavailable");
        assert_eq!(energy_label(-1.0), "Unavailable");
    }
}
