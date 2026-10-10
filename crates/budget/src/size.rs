use crate::GIB;

/// The size of a job's VM, named by its `runs-on` label.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Size {
    /// `cf-2g`
    G2,
    /// `cf-4g`, and what `self-hosted` means
    G4,
    /// `cf-6g`
    G6,
    /// `cf-8g`
    G8,
}

impl Size {
    /// Every size, smallest first.
    pub const ALL: [Size; 4] = [Size::G2, Size::G4, Size::G6, Size::G8];

    /// The label of workflows that carried no size before sizes existed. It means [`Size::G4`].
    pub const UNSIZED_LABEL: &'static str = "self-hosted";

    /// The memory the VM is created with. It never grows or shrinks.
    pub const fn vm_bytes(self) -> u64 {
        match self {
            Size::G2 => 2 * GIB,
            Size::G4 => 4 * GIB,
            Size::G6 => 6 * GIB,
            Size::G8 => 8 * GIB,
        }
    }

    /// The label that asks for this size.
    pub const fn label(self) -> &'static str {
        match self {
            Size::G2 => "cf-2g",
            Size::G4 => "cf-4g",
            Size::G6 => "cf-6g",
            Size::G8 => "cf-8g",
        }
    }

    /// Where this size sits in [`Size::ALL`].
    pub const fn index(self) -> usize {
        match self {
            Size::G2 => 0,
            Size::G4 => 1,
            Size::G6 => 2,
            Size::G8 => 3,
        }
    }

    /// The size one label asks for; `None` for a label that names no size. GitHub compares labels without case.
    pub fn from_label(label: &str) -> Option<Size> {
        if label.eq_ignore_ascii_case(Self::UNSIZED_LABEL) {
            return Some(Size::G4);
        }
        Size::ALL
            .into_iter()
            .find(|size| label.eq_ignore_ascii_case(size.label()))
    }

    /// The size a job's labels ask for. A size label wins over `self-hosted` next to it; `None` when no label
    /// names a size, or two labels name different ones (GitHub offers such a job to no scale set of ours).
    pub fn of_labels<'a>(labels: impl IntoIterator<Item = &'a str>) -> Option<Size> {
        let mut named = None;
        let mut unsized_seen = false;
        for label in labels {
            if label.eq_ignore_ascii_case(Self::UNSIZED_LABEL) {
                unsized_seen = true;
            } else if let Some(size) = Size::from_label(label) {
                if named.is_some_and(|other| other != size) {
                    return None;
                }
                named = Some(size);
            }
        }
        named.or(unsized_seen.then_some(Size::G4))
    }
}
