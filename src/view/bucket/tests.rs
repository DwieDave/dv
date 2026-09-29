use proptest::prelude::*;

use super::*;

fn rows(level: &Level) -> Vec<Row> {
    (0..level.len()).map(|i| level.row(i).unwrap()).collect()
}

fn span(row: &Row) -> Range<u64> {
    match row {
        Row::Child(i) => *i..i + 1,
        Row::Bucket(r) => r.clone(),
    }
}

fn check_partition(range: &Range<u64>, level: &Level) -> Result<(), TestCaseError> {
    prop_assert!(level.len() <= BUCKET);
    prop_assert_eq!(level.row(level.len()), None);
    let mut next = range.start;
    let mut samples = vec![0, level.len() / 2, level.len().saturating_sub(1)];
    samples.dedup();
    for i in samples {
        let Some(row) = level.row(i) else { continue };
        let s = span(&row);
        prop_assert!(s.start >= next && s.end <= range.end && !s.is_empty());
        next = s.end;
    }
    Ok(())
}

proptest! {
    #[test]
    fn levels_partition_their_range(n in 0u64..1_000_000_000_000, path in proptest::collection::vec(any::<prop::sample::Index>(), 0..6)) {
        let mut range = 0..n;
        for pick in path {
            let level = Level::of(range.clone());
            check_partition(&range, &level)?;
            if level.is_empty() { break; }
            match level.row(pick.index(usize::try_from(level.len()).unwrap()) as u64).unwrap() {
                Row::Child(_) => break,
                Row::Bucket(inner) => range = inner,
            }
        }
    }

    #[test]
    fn small_ranges_list_children_in_order(n in 0u64..3000) {
        let level = Level::of(0..n);
        let all = rows(&level);
        let covered: Vec<u64> = all.iter().flat_map(span).collect();
        prop_assert_eq!(covered, (0..n).collect::<Vec<_>>());
        prop_assert_eq!(all.iter().all(|r| matches!(r, Row::Child(_))), n <= BUCKET);
    }

    #[test]
    fn buckets_align_to_their_step(n in BUCKET + 1..100_000_000u64) {
        let level = Level::of(0..n);
        for row in rows(&level) {
            if let Row::Bucket(r) = row {
                let size = r.end - r.start;
                prop_assert!(r.start % BUCKET == 0);
                prop_assert!(r.start % size == 0 || r.end == n);
            }
        }
    }
}

#[test]
fn half_a_million_children_become_1024_sized_buckets() {
    let level = Level::of(0..500_000);
    assert_eq!(level.len(), 489);
    assert_eq!(level.row(0), Some(Row::Bucket(0..1024)));
    assert_eq!(level.row(488), Some(Row::Bucket(499_712..500_000)));
}
