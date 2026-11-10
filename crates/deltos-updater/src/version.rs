//! Version numbers as they appear in release tags and `Cargo.toml`.

/// The numeric components of a version: `v0.1.10` and `0.1.10-beta.2` both give `[0, 1, 10]`.
/// Everything from the first component that is not a number is ignored, so a pre-release
/// suffix never makes a version look newer than the final one.
pub(crate) fn parse(version: &str) -> Vec<u64> {
    version
        .trim()
        .trim_start_matches('v')
        .split(['.', '-', '+'])
        .map_while(|part| part.parse().ok())
        .collect()
}

/// `a` is strictly newer than `b`, comparing dotted numbers (`0.1.10` is newer than `0.1.9`).
/// Equal versions are not newer, so an update to the running version is never offered, and
/// neither is an older one.
pub fn is_newer(a: &str, b: &str) -> bool {
    let a = parse(a);
    let b = parse(b);
    !a.is_empty() && a > b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_numerically() {
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("0.1.0.1", "0.1.0"));
    }

    #[test]
    fn equal_and_older_are_not_newer() {
        assert!(!is_newer("0.1.1", "0.1.1"));
        assert!(!is_newer("v0.1.1", "0.1.1"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(!is_newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn ignores_pre_release_suffixes() {
        assert_eq!(parse("0.2.0-beta.1"), vec![0, 2, 0]);
        assert_eq!(parse("0.2.0+build.5"), vec![0, 2, 0]);
        assert!(!is_newer("0.2.0-beta.1", "0.2.0"));
    }

    #[test]
    fn rejects_text_that_is_not_a_version() {
        assert_eq!(parse("latest"), Vec::<u64>::new());
        assert!(!is_newer("latest", "0.1.0"));
        assert!(!is_newer("", "0.1.0"));
    }
}
