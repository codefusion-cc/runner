//! Prints what the Runner reads from this Linux host, to compare with `free -b`, `/proc/vmstat`,
//! `/proc/swaps`, `/proc/pressure`, the thermal zones and `df`. It only reads and prints, and prints no name of
//! the host.
//!
//! `cargo run -p cf-host-linux --example linux-probe [the state folder]`

use std::path::Path;
use std::process::ExitCode;

use cf_budget::{Budget, GIB, Host, HostError, Settings, Size};
use cf_host_linux::{LinuxHost, battery, boot_time, fast_cpus, load, model, reading};

fn main() -> ExitCode {
    let disk = std::env::args().nth(1).unwrap_or_else(|| "/".into());
    match probe(&disk) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("cannot read this host: {error}");
            ExitCode::FAILURE
        }
    }
}

fn probe(disk: &str) -> Result<(), HostError> {
    let root = Path::new("/");
    let host = LinuxHost::new(Path::new(disk))?;
    let facts = host.facts()?;
    let now = host.now();
    let raw = host.raw()?;
    let reading = reading(now, &raw)?;
    let memory = &raw.memory;
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
        memory.available_bytes,
        gib(memory.available_bytes)
    );
    println!(
        "SwapTotal / SwapFree       {} / {} (used {})",
        memory.swap_total_bytes, memory.swap_free_bytes, reading.swap_used_bytes
    );
    println!("pswpout                    {}", raw.swapouts);
    match &raw.swaps {
        Some(swaps) => {
            for swap in swaps {
                let kind = if swap.zram { "zram" } else { "disk" };
                println!("/proc/swaps {kind}           used {}", swap.used_bytes);
            }
        }
        None => println!("/proc/swaps                not read, every pswpout counts"),
    }
    println!("swap-outs the budget sees  {}", reading.swapouts);
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

    let settings = Settings::for_headless(facts, u32::try_from(fast.len()).unwrap_or(0));
    let Ok(mut budget) = Budget::new(settings.clone()) else {
        println!("headless: the settings do not fit this host");
        return Ok(());
    };
    budget.observe(now, Some(reading));
    let limits = budget.status(now).limits;
    println!(
        "headless: cap {:.2} GiB, floor {:.2} GiB, {} CPUs, vCPUs {:?}; above the floor now: {:.2} GiB",
        gib(limits.cap_bytes),
        gib(limits.floor_bytes),
        limits.cpus,
        settings.vcpus,
        gib(reading.available_bytes.saturating_sub(limits.floor_bytes)),
    );
    for size in Size::ALL {
        let fits = match settings.can_run(size) {
            Ok(()) => format!("{} side by side", settings.side_by_side(&settings.away, size)),
            Err(refusal) => format!("never ({refusal:?})"),
        };
        println!("  {}: {fits}", size.label());
    }
    Ok(())
}

#[allow(clippy::cast_precision_loss)]
fn gib(bytes: u64) -> f64 {
    bytes as f64 / GIB as f64
}
