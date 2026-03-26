# pac-windows

A Rust CLI tool for reading real-time power measurements from Microchip PAC1954 energy monitors on Windows, and analyzing historical energy data from the SRUM database.

## What it does

This tool interfaces with the **Windows Energy Meter Interface (EMI)** to read hardware power measurements from PAC1954 4-channel power monitor ICs. It also reads the **SRUM (System Resource Usage Monitor)** database to extract historical energy consumption data, including power usage during sleep/standby.

### Power rails monitored

| Rail | Description |
|------|-------------|
| `+18.2VB` | Main 18.2V bus (total platform) |
| `CPU_+18.2V_VCCCORE` | CPU core voltage |
| `CPU_+18.2VB_VCCSA` | CPU system agent |
| `CPU_+18.2VB_VCCGT` | CPU integrated graphics |
| `CPU_+18.2_VCCLPECORE` | CPU low-power E-cores |
| `CPU_+5V_BULK_DRAM` | DRAM |
| `DISPLAY_+18.2VB_EDP` | Display panel (eDP) |
| `DISPLAY_+3VS_EDP` | Display 3.3V rail |

## Usage

### List detected EMI devices

```
pac-windows list            # PAC1954 channels only
pac-windows list --all      # Include RAPL and other EMI devices
```

### Single measurement snapshot

```
pac-windows dump            # Table format
pac-windows dump --json     # JSON output
pac-windows dump --all      # Include RAPL
```

### Live power monitoring

```
pac-windows monitor                          # Default: 1s interval, all PAC channels
pac-windows monitor --interval 500           # 500ms sampling
pac-windows monitor --csv                    # CSV output (for piping/logging)
pac-windows monitor --filter vcccore,dram    # Filter to specific rails
pac-windows monitor --count 10               # Stop after 10 samples
pac-windows monitor --all                    # Include RAPL
```

### SRUM database analysis

The SRUM database stores historical energy data per application and power rail. To read it, first copy the live database (requires admin):

```
esentutl /y /vss C:\Windows\System32\sru\SRUDB.dat /d srudb_copy.dat
```

Then query it:

```
pac-windows srum --db srudb_copy.dat --table list           # List available tables
pac-windows srum --db srudb_copy.dat --table energy          # Energy Usage table
pac-windows srum --db srudb_copy.dat --table energy-lt       # Energy Usage (Long Term)
pac-windows srum --db srudb_copy.dat --table estimator       # Energy Estimator Provider
pac-windows srum --db srudb_copy.dat --table energy --csv    # CSV output
pac-windows srum --db srudb_copy.dat --table energy -n 20    # First 20 rows
```

## How it works

### EMI (live monitoring)

The Microchip PAC1954 driver (`PAC194x_5x.sys`) exposes each power channel as a Windows EMI device. The tool:

1. Enumerates EMI devices via `SetupDiGetClassDevs` with `GUID_DEVICE_ENERGY_METER`
2. Opens each device with `CreateFileW` (no admin required)
3. Reads metadata (channel name, OEM, model) via `IOCTL_EMI_GET_METADATA`
4. Reads measurements via `IOCTL_EMI_GET_MEASUREMENT` which returns:
   - `AbsoluteEnergy` (picowatt-hours, monotonically increasing)
   - `AbsoluteTime` (100-nanosecond intervals)
5. Computes instantaneous power from energy deltas between samples

### SRUM (historical analysis)

Windows continuously logs energy meter data to the SRUM database (`C:\Windows\System32\sru\SRUDB.dat`), an ESE (Extensible Storage Engine) database. The tool reads it using the JET API (`esent.dll`) and extracts:

- Per-application energy usage with timestamps
- Energy meter readings per power rail during sleep sessions
- Long-term energy usage trends

This is the same data source that `powercfg /sleepstudy` uses to generate its report.

## Comparison with existing tools

| Feature | pac-windows | powercfg /sleepstudy | Intel SystemMeter | Performance Monitor |
|---------|------------|---------------------|-------------------|-------------------|
| Real-time power (watts) | Yes | No | Yes | Limited |
| Per-rail breakdown | Yes | Yes (in report) | Yes | No |
| Sleep power analysis | Yes (via SRUM) | Yes (HTML report) | No | No |
| Scriptable output (CSV/JSON) | Yes | No (HTML only) | No | Yes (but complex) |
| No admin required (live) | Yes | N/A | Unknown | Yes |
| Historical data | Yes (SRUM) | Yes | No | No |
| Custom sampling interval | Yes | N/A | Fixed | Yes |
| Filter by rail | Yes | No | No | Manual |

### Key advantages

- **Scriptable**: CSV and JSON output for easy integration with data analysis pipelines
- **Focused**: Shows exactly the PAC1954 hardware measurements without UI overhead
- **Flexible**: Filter by rail, custom intervals, sample counts
- **Sleep analysis**: Direct access to SRUM energy data that `powercfg /sleepstudy` formats into HTML

## Building

Requires Rust with the MSVC toolchain on Windows:

```
rustup default stable-x86_64-pc-windows-msvc
cargo build --release
```

The binary will be at `target/release/pac-windows.exe`.

## Requirements

- Windows 10/11 with Microchip PAC1954 driver installed (`PAC194x_5x.sys`)
- MSVC Rust toolchain (for building)
- Admin privileges only needed for SRUM database access (live EMI monitoring works without admin)
