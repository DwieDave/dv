//! Neutral number formatting shared by the view and the UI.

/// `15.2 MB`-style sizes in decimal units.
#[must_use]
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["kB", "MB", "GB", "TB", "PB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    #[allow(clippy::cast_precision_loss)] // display only, one decimal
    let mut value = bytes as f64 / 1000.0;
    let mut unit = 0;
    while value >= 999.95 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// `1,234,567`-style thousands grouping.
#[must_use]
pub fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn grouping_round_trips(n in any::<u64>()) {
            let shown = grouped(n);
            prop_assert_eq!(shown.replace(',', "").parse::<u64>().unwrap(), n);
            prop_assert!(shown.split(',').skip(1).all(|g| g.len() == 3));
        }
    }

    #[test]
    fn sizes_use_decimal_units() {
        let cases = [
            (0, "0 B"),
            (999, "999 B"),
            (15_200_000, "15.2 MB"),
            (1_000, "1.0 kB"),
            (3_400_000_000, "3.4 GB"),
        ];
        for (bytes, shown) in cases {
            assert_eq!(human_bytes(bytes), shown);
        }
    }
}
