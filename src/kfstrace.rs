use std::collections::BTreeMap;

// kfstrace (cephtrace kernel-client tracer) MDS-mode output, e.g.:
//   03:23:50 4126073  touch  84234  106  0  CREATE  kt_1  1  799μs  10.6ms  OK
// columns: TIME PID COMMAND CLIENT_ID TID MDS OP FILE ATTEMPTS UNSAFE_LAT SAFE_LAT RESULT
// The MVP aggregates by MDS op type; other columns are parsed away for now.

#[derive(Clone, Debug)]
pub(crate) struct KfsEvent {
    pub(crate) op: String,
    pub(crate) safe_lat_us: u64,
    pub(crate) unsafe_lat_us: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct KfsOpRow {
    pub(crate) op: String,
    pub(crate) count: u64,
    pub(crate) avg_us: u64,
    pub(crate) max_us: u64,
    pub(crate) unsafe_count: u64,
}

pub(crate) fn parse_kfs_event(line: &str) -> Option<KfsEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty()
        || trimmed.starts_with("Tracing")
        || trimmed.starts_with("Timeout")
        || trimmed.starts_with("Press")
        || trimmed.starts_with("TIME")
    {
        return None;
    }
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    // 7 leading fields + at least one FILE token + 4 trailing fields
    if tokens.len() < 12 || !tokens[0].contains(':') {
        return None;
    }
    let len = tokens.len();
    Some(KfsEvent {
        op: tokens[6].to_owned(),
        unsafe_lat_us: parse_lat_us(tokens[len - 3]),
        safe_lat_us: parse_lat_us(tokens[len - 2]),
    })
}

fn parse_lat_us(value: &str) -> u64 {
    let value = value.trim();
    if value == "-" || value.is_empty() {
        return 0;
    }
    let split = value
        .find(|ch: char| !ch.is_ascii_digit() && ch != '.')
        .unwrap_or(value.len());
    let (num, unit) = value.split_at(split);
    let Ok(number) = num.parse::<f64>() else {
        return 0;
    };
    let micros = match unit {
        "ms" => number * 1_000.0,
        "s" => number * 1_000_000.0,
        "ns" => number / 1_000.0,
        _ => number, // μs / us / unitless
    };
    micros.round() as u64
}

pub(crate) fn kfs_op_rows(events: &[KfsEvent]) -> Vec<KfsOpRow> {
    let mut stats: BTreeMap<String, (u64, u64, u64, u64)> = BTreeMap::new();
    for event in events {
        let entry = stats.entry(event.op.clone()).or_default();
        entry.0 += 1;
        entry.1 = entry.1.saturating_add(event.safe_lat_us);
        entry.2 = entry.2.max(event.safe_lat_us);
        if event.unsafe_lat_us > 0 {
            entry.3 += 1;
        }
    }
    let mut rows: Vec<KfsOpRow> = stats
        .into_iter()
        .map(|(op, (count, sum, max, unsafe_count))| KfsOpRow {
            op,
            count,
            avg_us: if count > 0 { sum / count } else { 0 },
            max_us: max,
            unsafe_count,
        })
        .collect();
    rows.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| right.max_us.cmp(&left.max_us))
            .then_with(|| left.op.cmp(&right.op))
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_mds_lines() {
        let create = parse_kfs_event(
            "03:23:50 4126073  touch    84234  106  0   CREATE   kt_1   1        799μs      10.6ms     OK",
        )
        .expect("create line parses");
        assert_eq!(create.op, "CREATE");
        assert_eq!(create.unsafe_lat_us, 799);
        assert_eq!(create.safe_lat_us, 10_600);

        let getattr =
            parse_kfs_event("03:23:50 4126074  ls  84234  107  0   GETATTR  ?  1  -  7.4ms  OK")
                .expect("getattr line parses");
        assert_eq!(getattr.op, "GETATTR");
        assert_eq!(getattr.unsafe_lat_us, 0);
        assert_eq!(getattr.safe_lat_us, 7_400);
    }

    #[test]
    fn ignores_headers_and_status_lines() {
        assert!(parse_kfs_event("Tracing Ceph kernel OSD and MDS requests...").is_none());
        assert!(parse_kfs_event("Timeout reached, exiting...").is_none());
        assert!(parse_kfs_event("").is_none());
        assert!(parse_kfs_event("not enough columns").is_none());
    }

    #[test]
    fn aggregates_op_rows_sorted_by_count() {
        let events = vec![
            parse_kfs_event("03:23:50 1 ls 84234 1 0 GETATTR ? 1 - 8ms OK").unwrap(),
            parse_kfs_event("03:23:50 2 ls 84234 2 0 GETATTR ? 1 - 4ms OK").unwrap(),
            parse_kfs_event("03:23:50 3 touch 84234 3 0 CREATE f 1 800μs 12ms OK").unwrap(),
        ];
        let rows = kfs_op_rows(&events);
        assert_eq!(rows[0].op, "GETATTR");
        assert_eq!(rows[0].count, 2);
        assert_eq!(rows[0].avg_us, 6_000);
        assert_eq!(rows[0].max_us, 8_000);
        assert_eq!(rows[0].unsafe_count, 0);
        assert_eq!(rows[1].op, "CREATE");
        assert_eq!(rows[1].unsafe_count, 1);
    }
}
