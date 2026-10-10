//! Prints what the Runner reads from this Mac, to compare with `vm_stat`, `memory_pressure`, `sysctl` and
//! Activity Monitor. It only reads and prints.
//!
//! `cargo run -p cf-host-macos --example host-probe [directory on the job disks' volume]`

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    use cf_budget::{Budget, Host, Mode, Settings};
    use cf_host_macos::{MacHost, reading};

    let disk = std::env::args().nth(1).unwrap_or_else(|| "/".into());
    let mut host = match MacHost::new(std::path::Path::new(&disk)) {
        Ok(host) => host,
        Err(error) => {
            eprintln!("cannot read this Mac: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let result = (|| {
        let facts = host.facts()?;
        let now = host.now();
        let raw = host.raw()?;
        let reading = reading(facts, now, &raw)?;
        Ok::<_, cf_budget::HostError>((facts, now, raw, reading))
    })();
    let (facts, now, raw, reading) = match result {
        Ok(all) => all,
        Err(error) => {
            eprintln!("cannot read this Mac: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };

    println!("hw.memsize                 {}", facts.total_bytes);
    println!("hw.ncpu                    {}", facts.cores);
    println!("page size                  {}", raw.page_bytes);
    println!("awake ms / elapsed ms      {} / {}", now.awake.0, now.elapsed.0);
    println!(
        "Pages free                 {}",
        raw.free_pages.saturating_sub(raw.speculative_pages)
    );
    println!("Pages active               {}", raw.active_pages);
    println!("Pages inactive             {}", raw.inactive_pages);
    println!("Pages speculative          {}", raw.speculative_pages);
    println!("Pages wired down           {}", raw.wired_pages);
    println!("Pages purgeable            {}", raw.purgeable_pages);
    println!("File-backed pages          {}", raw.file_backed_pages);
    println!("Anonymous pages            {}", raw.anonymous_pages);
    println!("Pages occupied by compressor {}", raw.compressor_pages);
    println!("Swapouts                   {}", raw.swapouts);
    println!(
        "pressure level             {} ({:?})",
        raw.pressure_level, reading.pressure
    );
    println!(
        "swap used / total bytes    {} / {}",
        raw.swap_used_bytes, raw.swap_total_bytes
    );
    println!(
        "HIDIdleTime ns             {:?} ({} s)",
        raw.idle_nanos,
        reading.idle.0 / 1000
    );
    println!(
        "thermal level              {} ({:?})",
        raw.thermal_level, reading.thermal
    );
    println!(
        "power remaining s          {} (on battery: {})",
        raw.power_remaining_secs, reading.on_battery
    );
    println!("disk free bytes ({disk})   {}", reading.disk_free_bytes);
    println!(
        "available bytes            {} ({:.2} GiB)",
        reading.available_bytes,
        gib(reading.available_bytes)
    );

    for mode in [Mode::Dedicated, Mode::Workstation] {
        // The budget itself says which limits are in force, so this never disagrees with the Runner.
        let Ok(mut budget) = Budget::new(Settings::for_host(facts, mode)) else {
            eprintln!("{mode:?}: the default settings do not fit this Mac");
            continue;
        };
        budget.observe(now, Some(reading));
        let status = budget.status(now);
        println!(
            "{mode:?}: person there: {}, cap {:.2} GiB, floor {:.2} GiB, {} CPUs; above the floor now: {:.2} GiB",
            status.person_there,
            gib(status.limits.cap_bytes),
            gib(status.limits.floor_bytes),
            status.limits.cpus,
            gib(reading.available_bytes.saturating_sub(status.limits.floor_bytes)),
        );
    }
    std::process::ExitCode::SUCCESS
}

#[cfg(target_os = "macos")]
fn gib(bytes: u64) -> f64 {
    bytes as f64 / cf_budget::GIB as f64
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("this probe reads macOS");
}
