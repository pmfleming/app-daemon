use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Default)]
pub(super) struct EnergySampler {
    previous_rapl: HashMap<PathBuf, (u64, u64)>,
}

impl EnergySampler {
    pub(super) fn sample(&mut self, seconds: f64) -> EnergySample {
        let battery = read_batteries();
        let rapl_mwh = self.rapl_energy_mwh();
        let (energy_mwh, source) = if rapl_mwh > 0.0 {
            (rapl_mwh, "rapl")
        } else if battery.discharge_watts > 0.0 && seconds > 0.0 {
            (battery.discharge_watts * seconds / 3.6, "battery")
        } else {
            (0.0, "unavailable")
        };
        EnergySample {
            energy_mwh,
            battery_full_mwh: battery.full_mwh,
            source: source.into(),
        }
    }

    fn rapl_energy_mwh(&mut self) -> f64 {
        let current = read_rapl_zones();
        let microjoules = current
            .iter()
            .filter_map(|(path, &(value, maximum))| {
                let &(previous, _) = self.previous_rapl.get(path)?;
                Some(counter_delta(previous, value, maximum))
            })
            .sum::<u64>();
        self.previous_rapl = current;
        microjoules as f64 / 3_600_000.0
    }
}

fn counter_delta(previous: u64, current: u64, maximum: u64) -> u64 {
    if current >= previous {
        current - previous
    } else {
        maximum.saturating_sub(previous).saturating_add(current)
    }
}

#[derive(Debug, Default)]
pub(super) struct EnergySample {
    pub(super) energy_mwh: f64,
    pub(super) battery_full_mwh: f64,
    pub(super) source: String,
}

#[derive(Debug, Default)]
struct BatterySample {
    full_mwh: f64,
    discharge_watts: f64,
}

fn read_rapl_zones() -> HashMap<PathBuf, (u64, u64)> {
    let root = Path::new("/sys/class/powercap");
    let Ok(entries) = fs::read_dir(root) else {
        return HashMap::new();
    };
    let candidates = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join("energy_uj").is_file())
        .collect::<Vec<_>>();
    candidates
        .iter()
        .filter(|path| {
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            !candidates.iter().any(|parent| {
                let parent = parent
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("");
                parent != name && name.starts_with(&format!("{parent}:"))
            })
        })
        .filter_map(|path| {
            Some((
                path.clone(),
                (
                    read_u64(&path.join("energy_uj"))?,
                    read_u64(&path.join("max_energy_range_uj")).unwrap_or(u64::MAX),
                ),
            ))
        })
        .collect()
}

fn read_batteries() -> BatterySample {
    let Ok(entries) = fs::read_dir("/sys/class/power_supply") else {
        return BatterySample::default();
    };
    let mut result = BatterySample::default();
    for path in entries.filter_map(Result::ok).map(|entry| entry.path()) {
        if read_trimmed(&path.join("type")).as_deref() != Some("Battery") {
            continue;
        }
        let voltage_uv = read_u64(&path.join("voltage_now")).unwrap_or(0) as f64;
        let full_mwh = read_u64(&path.join("energy_full"))
            .map(|value| value as f64 / 1000.0)
            .or_else(|| {
                read_u64(&path.join("charge_full"))
                    .map(|charge| charge as f64 * voltage_uv / 1_000_000_000.0)
            })
            .unwrap_or(0.0);
        result.full_mwh += full_mwh;
        if read_trimmed(&path.join("status")).as_deref() != Some("Discharging") {
            continue;
        }
        let watts = read_u64(&path.join("power_now"))
            .map(|value| value as f64 / 1_000_000.0)
            .or_else(|| {
                read_u64(&path.join("current_now"))
                    .map(|current| current as f64 * voltage_uv / 1_000_000_000_000.0)
            })
            .unwrap_or(0.0);
        result.discharge_watts += watts;
    }
    result
}

fn read_u64(path: &Path) -> Option<u64> {
    read_trimmed(path)?.parse().ok()
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn handles_counter_rollover() {
        assert_eq!(super::counter_delta(900, 100, 1_000), 200);
        assert_eq!(super::counter_delta(100, 250, 1_000), 150);
    }
}
