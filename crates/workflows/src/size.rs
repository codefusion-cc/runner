use crate::labels::LabelSet;

/// The VM sizes a job can ask for with a `cf-<n>g` label, in GB.
pub const SIZES_GB: [u32; 4] = [2, 4, 6, 8];

/// What `self-hosted` without a size label means, in GB.
pub const DEFAULT_SIZE_GB: u32 = 4;

/// Why a label set has no size.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SizeError {
    /// A `cf-<n>g` label whose number is not one of [`SIZES_GB`].
    #[error("{0} is not a size: the sizes are cf-2g, cf-4g, cf-6g and cf-8g")]
    Unknown(String),
    /// Two different size labels in one request.
    #[error("the request names two sizes, {0} and {1}")]
    Conflict(String, String),
}

/// The digits of a (lowercase) label that has the shape of a size label, `cf-<digits>g`, valid or not.
fn size_shape(label: &str) -> Option<&str> {
    let digits = label.strip_prefix("cf-")?.strip_suffix('g')?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some(digits)
}

/// The size in GB a (lowercase) label names, when it is one of the size labels spelled the one way they are:
/// `cf-2g`, `cf-4g`, `cf-6g`, `cf-8g`. `cf-04g` is not one: two spellings of a size would be two scale sets.
fn size_of(label: &str) -> Option<u32> {
    let n = size_shape(label)?.parse::<u32>().ok()?;
    (SIZES_GB.contains(&n) && label == format!("cf-{n}g")).then_some(n)
}

/// Whether a (lowercase) label is one of the valid size labels: `cf-2g`, `cf-4g`, `cf-6g`, `cf-8g`.
pub fn is_size_label(label: &str) -> bool {
    size_of(label).is_some()
}

/// The VM size in GB that a request asks for: its size label, or [`DEFAULT_SIZE_GB`] when it has none (plain
/// `self-hosted`). A size label that is not a valid size, or two different ones, is an error: the request is
/// refused rather than guessed, so a typo does not run in a VM of the wrong size.
pub fn size_gb(set: &LabelSet) -> Result<u32, SizeError> {
    let mut found: Option<(&str, u32)> = None;
    for label in set.labels() {
        if size_shape(label).is_none() {
            continue;
        }
        let n = size_of(label).ok_or_else(|| SizeError::Unknown(label.clone()))?;
        match found {
            Some((first, _)) if first != label => {
                return Err(SizeError::Conflict(first.to_string(), label.clone()));
            }
            _ => found = Some((label, n)),
        }
    }
    Ok(found.map_or(DEFAULT_SIZE_GB, |(_, n)| n))
}
