use std::collections::BTreeMap;

// radostrace (cephtrace librados-client tracer) output, e.g.:
//   15915  89499  8  2  2  [4,2,3]  W  4096  9023  bench..._object7 [set-alloc-hint write][0, 4096]
// columns: pid client tid pool pg acting WR size latency(us) object[ops]
// The MVP aggregates by pool; other columns are parsed away for now.

#[derive(Clone, Debug)]
pub(crate) struct RadosEvent {
    pub(crate) pool: String,
    pub(crate) write: bool,
    pub(crate) latency_us: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct RadosPoolRow {
    pub(crate) pool: String,
    pub(crate) count: u64,
    pub(crate) avg_us: u64,
    pub(crate) max_us: u64,
    pub(crate) writes: u64,
    pub(crate) reads: u64,
}

pub(crate) fn parse_rados_event(line: &str) -> Option<RadosEvent> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    // pid client tid pool pg acting WR size latency + at least one object token
    if tokens.len() < 10 || !tokens[0].chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let write = match tokens[6] {
        "W" => true,
        "R" => false,
        _ => return None,
    };
    Some(RadosEvent {
        pool: tokens[3].to_owned(),
        write,
        latency_us: tokens[8].parse::<u64>().ok()?,
    })
}

pub(crate) fn rados_pool_rows(events: &[RadosEvent]) -> Vec<RadosPoolRow> {
    let mut stats: BTreeMap<String, (u64, u64, u64, u64, u64)> = BTreeMap::new();
    for event in events {
        let entry = stats.entry(event.pool.clone()).or_default();
        entry.0 += 1;
        entry.1 = entry.1.saturating_add(event.latency_us);
        entry.2 = entry.2.max(event.latency_us);
        if event.write {
            entry.3 += 1;
        } else {
            entry.4 += 1;
        }
    }
    let mut rows: Vec<RadosPoolRow> = stats
        .into_iter()
        .map(|(pool, (count, sum, max, writes, reads))| RadosPoolRow {
            pool,
            count,
            avg_us: sum.checked_div(count).unwrap_or(0),
            max_us: max,
            writes,
            reads,
        })
        .collect();
    rows.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| right.max_us.cmp(&left.max_us))
            .then_with(|| left.pool.cmp(&right.pool))
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_op_lines() {
        let write = parse_rados_event(
            "   15915   89499       8     2   2         [4,2,3]   W     4096     9023     bench_object7 [set-alloc-hint write][0, 4096]",
        )
        .expect("write line parses");
        assert_eq!(write.pool, "2");
        assert!(write.write);
        assert_eq!(write.latency_us, 9023);

        let read = parse_rados_event(
            "   4210   771   3   5   1e   [1,2,3]   R   4096   412   obj [read][0, 4096]",
        )
        .expect("read line parses");
        assert_eq!(read.pool, "5");
        assert!(!read.write);
        assert_eq!(read.latency_us, 412);
    }

    #[test]
    fn ignores_setup_and_header_lines() {
        assert!(parse_rados_event("Found library librados.so.2 at: /snap/...").is_none());
        assert!(
            parse_rados_event(
                "     pid  client  tid  pool  pg  acting  WR  size  latency  object[ops]"
            )
            .is_none()
        );
        assert!(parse_rados_event("fill_map_hprobes: function Objecter::_send_op").is_none());
        assert!(parse_rados_event("").is_none());
    }

    #[test]
    fn aggregates_pool_rows() {
        let events = vec![
            parse_rados_event("1 1 1 2 1 [1,2,3] W 4096 100 o [write][0,4096]").unwrap(),
            parse_rados_event("1 1 2 2 1 [1,2,3] W 4096 300 o [write][0,4096]").unwrap(),
            parse_rados_event("1 1 3 5 1 [1,2,3] R 4096 50 o [read][0,4096]").unwrap(),
        ];
        let rows = rados_pool_rows(&events);
        assert_eq!(rows[0].pool, "2");
        assert_eq!(rows[0].count, 2);
        assert_eq!(rows[0].avg_us, 200);
        assert_eq!(rows[0].max_us, 300);
        assert_eq!(rows[0].writes, 2);
        assert_eq!(rows[0].reads, 0);
        assert_eq!(rows[1].pool, "5");
        assert_eq!(rows[1].reads, 1);
    }
}
