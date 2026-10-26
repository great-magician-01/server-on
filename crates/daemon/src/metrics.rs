use server_on_core::config::ServiceId;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct MetricsCache {
    samples: BTreeMap<ServiceId, CounterSample>,
}

struct CounterSample {
    identity: String,
    cumulative_ns: u64,
    sampled_at: Instant,
}

impl MetricsCache {
    pub fn remove(&mut self, service_id: &ServiceId) {
        self.samples.remove(service_id);
    }

    pub fn cpu_percent(
        &mut self,
        service_id: &ServiceId,
        identity: String,
        cumulative_ns: u64,
        processors: usize,
    ) -> Option<f64> {
        let now = Instant::now();
        let Some(previous) = self.samples.get(service_id) else {
            self.samples.insert(
                service_id.clone(),
                CounterSample {
                    identity,
                    cumulative_ns,
                    sampled_at: now,
                },
            );
            return None;
        };
        if previous.identity != identity || cumulative_ns < previous.cumulative_ns {
            self.samples.insert(
                service_id.clone(),
                CounterSample {
                    identity,
                    cumulative_ns,
                    sampled_at: now,
                },
            );
            return None;
        }
        let elapsed = now.duration_since(previous.sampled_at);
        let delta = cumulative_ns - previous.cumulative_ns;
        self.samples.insert(
            service_id.clone(),
            CounterSample {
                identity,
                cumulative_ns,
                sampled_at: now,
            },
        );
        if elapsed.is_zero() {
            return None;
        }
        let processors = processors.max(1) as f64;
        Some(
            (delta as f64 / 1_000_000_000.0 / elapsed.as_secs_f64() / processors * 100.0)
                .clamp(0.0, 100.0),
        )
    }
}

#[derive(Clone, Debug, Default)]
pub struct RawMetrics {
    pub cpu_time_ns: Option<u64>,
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
    pub process_count: Option<u32>,
    pub message: Option<String>,
}

pub fn parse_systemd_properties(input: &str) -> RawMetrics {
    let mut metrics = RawMetrics::default();
    for line in input.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "CPUUsageNSec" => metrics.cpu_time_ns = parse_u64(value),
            "MemoryCurrent" => metrics.memory_bytes = parse_u64(value),
            "TasksCurrent" => {
                metrics.process_count = parse_u64(value).and_then(|v| u32::try_from(v).ok())
            }
            _ => {}
        }
    }
    metrics
}

pub fn parse_ps_metrics(input: &str, process_group_id: u32) -> RawMetrics {
    let mut metrics = RawMetrics::default();
    let mut cpu_seconds = 0.0;
    let mut memory_kib = 0_u64;
    let mut count = 0_u32;
    for line in input.lines() {
        let columns: Vec<_> = line.split_whitespace().collect();
        if columns.len() < 3 {
            continue;
        }
        let Ok(pgid) = columns[0].parse::<u32>() else {
            continue;
        };
        if pgid != process_group_id {
            continue;
        }
        let Ok(rss) = columns[1].parse::<u64>() else {
            continue;
        };
        let Ok(cpu) = columns[2].parse::<f64>() else {
            continue;
        };
        memory_kib = memory_kib.saturating_add(rss);
        cpu_seconds += cpu;
        count += 1;
    }
    if count > 0 {
        metrics.cpu_time_ns = Some((cpu_seconds * 1_000_000_000.0) as u64);
        metrics.memory_bytes = Some(memory_kib.saturating_mul(1024));
        metrics.process_count = Some(count);
    } else {
        metrics.message = Some("未找到该 WSL 进程组的指标".into());
    }
    metrics
}

fn parse_u64(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() || value == "[not set]" || value == "infinity" || value == "max" {
        return None;
    }
    value.parse().ok()
}

pub fn parse_memory_bytes(input: &str) -> Option<u64> {
    let input = input.trim();
    let split = input
        .char_indices()
        .find(|(_, character)| !character.is_ascii_digit() && *character != '.')
        .map(|(index, _)| index)
        .unwrap_or(input.len());
    let (number, unit) = input.split_at(split);
    let value = number.trim().parse::<f64>().ok()?;
    let multiplier = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "kb" | "kib" | "k" => 1_024.0,
        "mb" | "mib" | "m" => 1_048_576.0,
        "gb" | "gib" | "g" => 1_073_741_824.0,
        "tb" | "tib" | "t" => 1_099_511_627_776.0,
        _ => return None,
    };
    Some((value * multiplier) as u64)
}

pub fn parse_percentage(input: &str) -> Option<f64> {
    input.trim().trim_end_matches('%').trim().parse().ok()
}

pub fn logical_processor_count() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
}

pub fn query_timeout() -> Duration {
    Duration::from_secs(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_delta_is_machine_normalized_and_reset_safe() {
        let mut cache = MetricsCache::default();
        let id = ServiceId::new();
        assert_eq!(cache.cpu_percent(&id, "job-a".into(), 1_000, 1), None);
        std::thread::sleep(Duration::from_millis(20));
        let value = cache
            .cpu_percent(&id, "job-a".into(), 2_000_000, 1)
            .unwrap();
        assert!(value > 0.0);
        assert_eq!(cache.cpu_percent(&id, "job-a".into(), 10, 1), None);
        assert_eq!(cache.cpu_percent(&id, "job-b".into(), 20, 1), None);
    }

    #[test]
    fn parses_systemd_values_and_unset_fields() {
        let metrics = parse_systemd_properties(
            "CPUUsageNSec=2500000000\nMemoryCurrent=1048576\nTasksCurrent=8\n",
        );
        assert_eq!(metrics.cpu_time_ns, Some(2_500_000_000));
        assert_eq!(metrics.memory_bytes, Some(1_048_576));
        assert_eq!(metrics.process_count, Some(8));
        let unset = parse_systemd_properties("CPUUsageNSec=[not set]\nMemoryCurrent=infinity\n");
        assert_eq!(unset.cpu_time_ns, None);
        assert_eq!(unset.memory_bytes, None);
    }

    #[test]
    fn parses_process_group_totals() {
        let metrics = parse_ps_metrics("  12  2048  1.50\n  12  1024  0.50\n  99  4096  9.0\n", 12);
        assert_eq!(metrics.process_count, Some(2));
        assert_eq!(metrics.memory_bytes, Some(3_072 * 1024));
        assert_eq!(metrics.cpu_time_ns, Some(2_000_000_000));
    }

    #[test]
    fn parses_docker_units() {
        assert_eq!(parse_memory_bytes("0B"), Some(0));
        assert_eq!(parse_memory_bytes("1.5MiB"), Some(1_572_864));
        assert_eq!(parse_memory_bytes("2GiB"), Some(2_147_483_648));
        assert_eq!(parse_percentage("12.5%"), Some(12.5));
    }
}
