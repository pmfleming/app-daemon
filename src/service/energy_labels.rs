//! Pure energy overview projection; state locking stays in the service.
use crate::{
    catalog::Catalog,
    history::EnergyTotal,
    metrics::{merged_labels, rate, rounded},
    model::{ApplicationEnergyOverview, ApplicationEnergySummary},
};
use shelllist_daemon_core::fixed_decimal;

pub(super) fn overview(
    mut totals: Vec<EnergyTotal>,
    catalog: &Catalog,
    since_ms: u64,
    until_ms: u64,
    limit: usize,
) -> ApplicationEnergyOverview {
    totals.sort_by(|left, right| {
        right
            .energy_mwh
            .total_cmp(&left.energy_mwh)
            .then(left.target_id.cmp(&right.target_id))
    });
    let total_energy_mwh = totals.iter().map(|value| value.energy_mwh).sum::<f64>();
    let energy_source = merged_labels(totals.iter().map(|value| value.energy_source.as_str()));
    let energy_confidence =
        merged_labels(totals.iter().map(|value| value.energy_confidence.as_str()));
    let applications = totals
        .into_iter()
        .take(limit.clamp(1, 100))
        .map(|value| {
            let identity = catalog.by_id(&value.target_id);
            ApplicationEnergySummary {
                energy_label: energy_label(value.energy_mwh),
                name: identity
                    .map(|entry| entry.name.clone())
                    .unwrap_or_else(|| value.target_id.clone()),
                icon: identity.map(|entry| entry.icon.clone()).unwrap_or_default(),
                share: rate(value.energy_mwh, total_energy_mwh, 4),
                target_id: value.target_id,
                energy_mwh: value.energy_mwh,
            }
        })
        .collect();
    ApplicationEnergyOverview {
        total_energy_label: energy_label(total_energy_mwh),
        since_ms,
        until_ms,
        total_energy_mwh: rounded(total_energy_mwh, 4),
        energy_source,
        energy_confidence,
        applications,
    }
}

fn energy_label(mwh: f64) -> String {
    if !mwh.is_finite() || mwh < 0.0 {
        return "Unavailable".into();
    }
    let (value, decimals, unit) = if mwh >= 1000.0 {
        (mwh / 1000.0, 2, "Wh")
    } else {
        (mwh, u8::from(mwh < 100.0), "mWh")
    };
    fixed_decimal(value, decimals)
        .map_or_else(|| "Unavailable".into(), |value| format!("{value} {unit}"))
}
#[cfg(test)]
mod tests {
    use super::energy_label;
    #[test]
    fn units_change_only_at_one_wh_and_unknown_is_not_zero() {
        for (value, expected) in [
            (0.0, "0.0 mWh"),
            (12.34, "12.3 mWh"),
            (99.99, "100.0 mWh"),
            (100.0, "100 mWh"),
            (999.0, "999 mWh"),
            (1000.0, "1.00 Wh"),
            (1250.0, "1.25 Wh"),
            (f64::NAN, "Unavailable"),
            (f64::INFINITY, "Unavailable"),
            (f64::NEG_INFINITY, "Unavailable"),
            (-1.0, "Unavailable"),
        ] {
            assert_eq!(energy_label(value), expected, "{value}");
        }
    }
}
