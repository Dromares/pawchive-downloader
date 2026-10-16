//! Small display helpers.

use pawchive_core::plan::Stat;

/// `1536` -> `"1.5 kB"`; decimal units, one decimal above bytes.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

/// Total-size cell for a type row: exact when every file is sized, otherwise a lower bound with
/// the sizing progress ("≥ 1.2 GB (sizing 340/612)").
pub fn size_cell(stat: &Stat, sizing: Option<(usize, usize)>) -> String {
    if stat.count == 0 {
        return String::new();
    }
    if stat.unsized_files == 0 {
        return human_size(stat.bytes);
    }
    match sizing {
        Some((done, total)) => format!("≥ {} (sizing {done}/{total})", human_size(stat.bytes)),
        None if stat.bytes == 0 => format!("unknown ({} unsized)", stat.unsized_files),
        None => format!("≥ {} ({} unsized)", human_size(stat.bytes), stat.unsized_files),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1_500), "1.5 kB");
        assert_eq!(human_size(199_887_803), "199.9 MB");
        assert_eq!(human_size(1_234_000_000), "1.2 GB");
    }

    #[test]
    fn size_cells() {
        let s = |count, bytes, unsized_files| Stat { count, bytes, unsized_files };
        assert_eq!(size_cell(&s(0, 0, 0), None), "");
        assert_eq!(size_cell(&s(3, 3000, 0), Some((3, 3))), "3.0 kB");
        assert_eq!(size_cell(&s(612, 1_200_000_000, 272), Some((340, 612))), "≥ 1.2 GB (sizing 340/612)");
        assert_eq!(size_cell(&s(5, 0, 5), None), "unknown (5 unsized)");
        assert_eq!(size_cell(&s(5, 2_000, 1), None), "≥ 2.0 kB (1 unsized)");
    }
}
