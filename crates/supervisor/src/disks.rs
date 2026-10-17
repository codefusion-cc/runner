//! How a job's root is put together from the image and the job's own disk.

/// The overlay's options: the image below, the job's disk above.
///
/// The two are file systems of their own (erofs and ext4), and the guest's kernel leaves `xino` off then. A folder's
/// inode number is then made up each time the kernel loads it, so one dropped under memory pressure comes back with
/// another, and a program that compares inode numbers takes it for another folder: a test of codefusion's failed so.
/// `xino=on` gives every file and folder the number it has on its layer, with the layer in the high bits: the same
/// for as long as the job runs.
pub fn overlay_options(lower: &str, upper: &str, work: &str) -> String {
    format!("lowerdir={lower},upperdir={upper},workdir={work},xino=on")
}
