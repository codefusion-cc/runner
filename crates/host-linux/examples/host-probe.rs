//! Prints what the Runner reads from this Linux host, to compare with `free -b`, `/proc/vmstat`, `/proc/pressure`,
//! the thermal zones and `df`. It only reads and prints, and prints no name of the host.
//!
//! `cargo run -p cf-host-linux --example host-probe [the state folder]`

use std::path::Path;

use cf_budget::{Budget, Host, HostError, Settings};
use cf_host_linux::{LinuxHost, battery, boot_time, fast_cpus, load, model, reading};

fn main() -> std::process::ExitCode {
    let disk = std::env::args().nth(1).unwrap_or_else(|| "/".into());
    let root = Path::new("/");
    let result = (|| {
        let host = LinuxHost::new(Path::new(&disk))?;
        let facts = host.facts()?;
        let now = host.now();
        let raw = host.raw()?;
        let reading = reading(now, &raw)?;
        Ok::<_, HostError>((host, facts, now, raw, reading))
    })();
    let (host, facts, now, raw, reading) = match result {
        Ok(all) => all,
        Err(error) => {
            eprintln!("cannot read this host: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let fast = fast_cpus(root).unwrap_or_default();

    println!(
        "model                      {}",
        model(root).unwrap_or_else(|| "-".into())
    );
    println!(
        "MemTotal                   {} ({:.2} GiB)",
        facts.total_bytes,
        gib(facts.total_bytes)
    );
    println!("online CPUs                {}", facts.cores);
    println!("fast CPUs                  {fast:?}");
    println!("monotonic ms / boottime ms {} / {}", now.awake.0, now.elapsed.0);
    println!(
        "MemAvailable               {} ({:.2} GiB)",
        raw.available_bytes,
        gib(raw.available_bytes)
    );
    println!(
        "SwapTotal / SwapFree       {} / {} (used {})",
        raw.swap_total_bytes, raw.swap_free_bytes, reading.swap_used_bytes
    );
    println!("pswpout                    {}", raw.swapouts);
    match raw.memory_pressure {
        Some(psi) => println!(
            "PSI memory avg10           some {:.2}% full {:.2}% ({:?})",
            f64::from(psi.some) / 100.0,
            f64::from(psi.full) / 100.0,
            reading.pressure
        ),
        None => println!(
            "PSI memory                 none on this kernel ({:?})",
            reading.pressure
        ),
    }
    for zone in &raw.zones {
        let trips: Vec<String> = zone
            .trips
            .iter()
            .map(|trip| format!("{:?} {:.1}", trip.kind, f64::from(trip.millidegrees) / 1000.0))
            .collect();
        println!(
            "zone {:<21} {:.1} C, trips: {}",
            zone.name,
            f64::from(zone.millidegrees) / 1000.0,
            trips.join(", ")
        );
    }
    println!("thermal                    {:?}", reading.thermal);
    for supply in &raw.supplies {
        println!(
            "supply {:<19} battery {}, online {:?}, status {:?}, capacity {:?}",
            supply.name, supply.battery, supply.online, supply.status, supply.capacity
        );
    }
    println!("on battery                 {}", reading.on_battery);
    println!("battery                    {:?}", battery(root));
    println!(
        "disk free / total ({disk})  {} / {}",
        reading.disk_free_bytes,
        host.disk_total_bytes().unwrap_or(0)
    );
    println!("load                       {:?}", load(root));
    println!("btime                      {:?}", boot_time(root));

    let fast_cores = u32::try_from(fast.len()).unwrap_or(0);
    let settings = Settings::for_headless(facts, fast_cores);
    let Ok(mut budget) = Budget::new(settings.clone()) else {
        eprintln!("the headless settings do not fit this host");
        return std::process::ExitCode::SUCCESS;
    };
    budget.observe(now, Some(reading));
    let status = budget.status(now);
    println!(
        "headless: cap {:.2} GiB, floor {:.2} GiB, {} CPUs, vCPUs {:?}; above the floor now: {:.2} GiB",
        gib(status.limits.cap_bytes),
        gib(status.limits.floor_bytes),
        status.limits.cpus,
        settings.vcpus,
        gib(reading.available_bytes.saturating_sub(status.limits.floor_bytes)),
    );
    for size in cf_budget::Size::ALL {
        println!(
            "  {}: {}",
            size.label(),
            match settings.can_run(size) {
                Ok(()) => format!("{} side by side", settings.side_by_side(&settings.away, size)),
                Err(refusal) => format!("never ({refusal:?})"),
            }
        );
    }
    std::process::ExitCode::SUCCESS
}

#[allow(clippy::cast_precision_loss)]
fn gib(bytes: u64) -> f64 {
    bytes as f64 / cf_budget::GIB as f64
}
