pub mod db;

// foldhash: SipHash showed up in profiles hashing 50-char store paths.
pub type HashMap<K, V> = std::collections::HashMap<K, V, foldhash::fast::RandomState>;
pub type HashSet<K> = std::collections::HashSet<K, foldhash::fast::RandomState>;

pub fn format_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.2} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.2} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.2} KiB", b / KIB)
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::format_size;

    #[test]
    fn format_size_units() {
        assert_eq!(format_size(0), "0 bytes");
        assert_eq!(format_size(1023), "1023 bytes");
        assert_eq!(format_size(1024), "1.00 KiB");
        assert_eq!(format_size(1536), "1.50 KiB");
        assert_eq!(format_size(1024 * 1024), "1.00 MiB");
        assert_eq!(format_size(5 * 1024 * 1024 + 512 * 1024), "5.50 MiB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.00 GiB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024 / 2), "1.50 GiB");
    }
}
