use cf_supervisor::disks::overlay_options;

/// The kernel reads the options as a comma-separated list: each of the job's root.
fn options(text: &str) -> Vec<&str> {
    text.split(',').collect()
}

#[test]
fn the_root_keeps_its_inode_numbers_and_its_layers() {
    let text = overlay_options("/mnt/image", "/mnt/scratch/upper", "/mnt/scratch/work");
    let mut listed = options(&text);
    listed.sort_unstable();
    assert_eq!(
        listed,
        [
            "lowerdir=/mnt/image",
            "upperdir=/mnt/scratch/upper",
            "workdir=/mnt/scratch/work",
            "xino=on",
        ]
    );
}
