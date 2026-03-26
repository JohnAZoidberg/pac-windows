mod emi;
mod srum;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "pac-windows", about = "PAC1954 power monitor tool for Windows")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List all EMI energy meter devices and their metadata
    List {
        /// Show all EMI devices including RAPL (default: PAC only)
        #[arg(long)]
        all: bool,
    },
    /// Take a single measurement snapshot from all channels
    Dump {
        /// Show all EMI devices including RAPL
        #[arg(long)]
        all: bool,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Live power monitoring with periodic sampling
    Monitor {
        /// Sampling interval in milliseconds
        #[arg(long, default_value = "1000")]
        interval: u64,
        /// Show all EMI devices including RAPL
        #[arg(long)]
        all: bool,
        /// Output as CSV (one line per sample)
        #[arg(long)]
        csv: bool,
        /// Filter to specific rail names (comma-separated substrings)
        #[arg(long)]
        filter: Option<String>,
        /// Number of samples to take (default: unlimited)
        #[arg(long, short)]
        count: Option<u64>,
    },
    /// Read energy data from SRUM database (requires a copy of SRUDB.dat)
    Srum {
        /// Path to SRUDB.dat copy (use: esentutl /y /vss C:\Windows\System32\sru\SRUDB.dat /d srudb_copy.dat)
        #[arg(long)]
        db: PathBuf,
        /// Which table to read: energy, energy-lt, estimator, or "list" to list tables
        #[arg(long, default_value = "energy")]
        table: String,
        /// Output as CSV
        #[arg(long)]
        csv: bool,
        /// Maximum number of rows to display
        #[arg(long, short = 'n')]
        limit: Option<usize>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::List { all } => cmd_list(all),
        Commands::Dump { all, json } => cmd_dump(all, json),
        Commands::Monitor {
            interval,
            all,
            csv,
            filter,
            count,
        } => cmd_monitor(interval, all, csv, filter, count),
        Commands::Srum {
            db,
            table,
            csv,
            limit,
        } => cmd_srum(db, table, csv, limit),
    }
}

fn cmd_list(show_all: bool) -> Result<()> {
    let devices = emi::discover_devices()?;
    let devices: Vec<_> = if show_all {
        devices
    } else {
        devices.into_iter().filter(|d| d.is_pac).collect()
    };

    if devices.is_empty() {
        println!("No EMI devices found.");
        return Ok(());
    }

    println!(
        "{:<35} {:<8} {:<10} {:<5} PATH",
        "CHANNEL", "OEM", "MODEL", "REV"
    );
    println!("{}", "-".repeat(100));
    for d in &devices {
        println!(
            "{:<35} {:<8} {:<10} {:<5} {}",
            d.channel_name, d.oem, d.model, d.hw_revision, d.path
        );
    }
    println!("\n{} device(s) found.", devices.len());
    Ok(())
}

fn cmd_dump(show_all: bool, json: bool) -> Result<()> {
    let devices = emi::discover_devices()?;
    let devices: Vec<_> = if show_all {
        devices
    } else {
        devices.into_iter().filter(|d| d.is_pac).collect()
    };

    if json {
        println!("[");
        for (i, d) in devices.iter().enumerate() {
            let m = emi::measure(&d.path)?;
            let energy_wh = m.energy_pwh as f64 / 1e12;
            println!(
                "  {{\"channel\": \"{}\", \"energy_pwh\": {}, \"energy_wh\": {:.6}, \"time_100ns\": {}}}{}",
                d.channel_name,
                m.energy_pwh,
                energy_wh,
                m.time_100ns,
                if i < devices.len() - 1 { "," } else { "" }
            );
        }
        println!("]");
    } else {
        println!(
            "{:<35} {:>18} {:>14} {:>20}",
            "CHANNEL", "ENERGY (pWh)", "ENERGY (Wh)", "TIME (100ns)"
        );
        println!("{}", "-".repeat(90));
        for d in &devices {
            let m = emi::measure(&d.path)?;
            let energy_wh = m.energy_pwh as f64 / 1e12;
            println!(
                "{:<35} {:>18} {:>14.6} {:>20}",
                d.channel_name, m.energy_pwh, energy_wh, m.time_100ns
            );
        }
    }
    Ok(())
}

fn cmd_monitor(
    interval_ms: u64,
    show_all: bool,
    csv: bool,
    filter: Option<String>,
    count: Option<u64>,
) -> Result<()> {
    let devices = emi::discover_devices()?;
    let mut devices: Vec<_> = if show_all {
        devices
    } else {
        devices.into_iter().filter(|d| d.is_pac).collect()
    };

    // Apply filter
    if let Some(ref f) = filter {
        let patterns: Vec<&str> = f.split(',').map(|s| s.trim()).collect();
        devices.retain(|d| {
            patterns
                .iter()
                .any(|p| d.channel_name.to_lowercase().contains(&p.to_lowercase()))
        });
    }

    if devices.is_empty() {
        println!("No matching EMI devices found.");
        return Ok(());
    }

    let interval = Duration::from_millis(interval_ms);

    // Print header
    if csv {
        print!("timestamp_ms");
        for d in &devices {
            print!(",{}_watts", d.channel_name.trim());
        }
        println!();
    } else {
        println!(
            "Monitoring {} channel(s) every {}ms. Press Ctrl+C to stop.\n",
            devices.len(),
            interval_ms
        );
    }

    // Take initial measurements
    let mut prev: Vec<emi::EmiMeasurement> = devices
        .iter()
        .map(|d| emi::measure(&d.path))
        .collect::<Result<Vec<_>>>()?;

    let start = std::time::Instant::now();
    let mut samples: u64 = 0;

    loop {
        std::thread::sleep(interval);

        let now: Vec<emi::EmiMeasurement> = devices
            .iter()
            .map(|d| emi::measure(&d.path))
            .collect::<Result<Vec<_>>>()?;

        if csv {
            print!("{}", start.elapsed().as_millis());
            for (i, m) in now.iter().enumerate() {
                let watts = m.power_watts(&prev[i]);
                print!(",{:.4}", watts);
            }
            println!();
        } else {
            let elapsed = start.elapsed().as_secs_f64();
            let mut total_watts = 0.0;
            println!("[{:.1}s]", elapsed);
            for (i, d) in devices.iter().enumerate() {
                let watts = now[i].power_watts(&prev[i]);
                total_watts += watts;
                println!("  {:<35} {:>8.3} W", d.channel_name.trim(), watts);
            }
            if devices.len() > 1 {
                println!("  {:<35} {:>8.3} W", "TOTAL", total_watts);
            }
            println!();
        }

        prev = now;
        samples += 1;
        if let Some(max) = count
            && samples >= max
        {
            break;
        }
    }

    Ok(())
}

fn cmd_srum(db_path: PathBuf, table: String, csv: bool, limit: Option<usize>) -> Result<()> {
    println!("Opening SRUM database: {}", db_path.display());
    let db = srum::SrumDatabase::open(&db_path)?;

    if table == "list" {
        println!("\nAvailable tables:");
        for t in db.list_tables()? {
            println!("  {} ({})", t, srum::table_friendly_name(&t));
        }
        return Ok(());
    }

    let table_guid = match table.as_str() {
        "energy" => "{FEE4E14F-02A9-4550-B5CE-5FA2DA202E37}",
        "energy-lt" => "{FEE4E14F-02A9-4550-B5CE-5FA2DA202E37}LT",
        "estimator" => "{DA73FB89-2BEA-4DDC-86B8-6E048C6DA477}",
        "tagged" => "{B6D82AF1-F780-4E17-8077-6CB9AD8A6FC4}",
        other if other.starts_with('{') => other,
        other => anyhow::bail!(
            "Unknown table '{}'. Use: energy, energy-lt, estimator, tagged, or a raw GUID",
            other
        ),
    };

    println!(
        "Reading table: {} ({})\n",
        table_guid,
        srum::table_friendly_name(table_guid)
    );

    let (col_names, rows) = db.read_table(table_guid)?;
    let row_count = if let Some(n) = limit {
        n.min(rows.len())
    } else {
        rows.len()
    };

    if csv {
        // CSV output
        println!("{}", col_names.join(","));
        for row in rows.iter().take(row_count) {
            let vals: Vec<String> = row.iter().map(|v| format!("{}", v)).collect();
            println!("{}", vals.join(","));
        }
    } else {
        // Table output - show column names and values
        println!("{} rows total (showing {})\n", rows.len(), row_count);

        for (row_idx, row) in rows.iter().take(row_count).enumerate() {
            println!("--- Row {} ---", row_idx + 1);
            for (col_idx, val) in row.iter().enumerate() {
                if matches!(val, srum::ColumnValue::Null) {
                    continue; // Skip NULL columns for readability
                }
                println!("  {:<30} {}", col_names[col_idx], val);
            }
            println!();
        }
    }

    println!("Total: {} rows", rows.len());
    Ok(())
}
