//! Which self-hosted runner label sets a repository's workflows ask for, and how big a VM each one means.
//!
//! This is a port of the Go manager's `workflows` package; its tests are the specification (see the tests here).
//! Added to it: the `cf-<n>g` size labels, where `self-hosted` alone means 4 GB, and the repository variables a
//! job's `runs-on` can read, so a console that flips `CI_RUNNER` to `cf-8g` gets a scale set of that size.

mod expression;
mod labels;
mod size;

pub use labels::{LabelSet, POOL_LABEL, self_hosted, self_hosted_with};
pub use size::{DEFAULT_SIZE_GB, SIZES_GB, SizeError, is_size_label, size_gb};
