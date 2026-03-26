use anyhow::{Context, Result, bail};
use std::mem;
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Foundation::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Power::*;
use windows::core::PCWSTR;

/// Metadata about an EMI device channel.
#[derive(Debug, Clone)]
pub struct EmiDevice {
    pub path: String,
    pub oem: String,
    pub model: String,
    pub hw_revision: u16,
    pub channel_name: String,
    pub is_pac: bool,
}

/// A single energy measurement from an EMI device.
#[derive(Debug, Clone, Copy)]
pub struct EmiMeasurement {
    /// Absolute energy in picowatt-hours (monotonically increasing).
    pub energy_pwh: u64,
    /// Absolute time in 100-nanosecond intervals.
    pub time_100ns: u64,
}

impl EmiMeasurement {
    /// Compute power in watts between two measurements.
    pub fn power_watts(&self, prev: &EmiMeasurement) -> f64 {
        let de = self.energy_pwh.wrapping_sub(prev.energy_pwh) as f64;
        let dt = self.time_100ns.wrapping_sub(prev.time_100ns) as f64;
        if dt == 0.0 {
            return 0.0;
        }
        // pWh -> Wh = 1e-12, 100ns -> hours = 1/(3.6e10)
        // power = dE_pWh / dt_100ns * (3.6e10 / 1e12) = dE/dt * 3.6e-2
        // Alternatively: dE_pWh * 3600e-12 / (dt_100ns * 1e-7) = dE/dt * 3.6e-2
        de / dt * 3.6e-2
    }
}

/// Enumerate all EMI device paths on the system.
pub fn enumerate_emi_paths() -> Result<Vec<String>> {
    let mut paths = Vec::new();

    unsafe {
        let dev_info = SetupDiGetClassDevsW(
            Some(&GUID_DEVICE_ENERGY_METER),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
        .context("SetupDiGetClassDevsW failed")?;

        let mut index: u32 = 0;
        loop {
            let mut iface_data = SP_DEVICE_INTERFACE_DATA {
                cbSize: mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
                ..Default::default()
            };

            let result = SetupDiEnumDeviceInterfaces(
                dev_info,
                None,
                &GUID_DEVICE_ENERGY_METER,
                index,
                &mut iface_data,
            );

            if result.is_err() {
                break; // No more devices
            }

            // Get required buffer size
            let mut required_size: u32 = 0;
            let _ = SetupDiGetDeviceInterfaceDetailW(
                dev_info,
                &iface_data,
                None,
                0,
                Some(&mut required_size),
                None,
            );

            if required_size == 0 {
                index += 1;
                continue;
            }

            // Allocate buffer and set cbSize
            let mut buf: Vec<u8> = vec![0u8; required_size as usize];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            // cbSize must be the size of the fixed part of the struct (on x64: 8)
            (*detail).cbSize = mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;

            if SetupDiGetDeviceInterfaceDetailW(
                dev_info,
                &iface_data,
                Some(detail),
                required_size,
                None,
                None,
            )
            .is_err()
            {
                index += 1;
                continue;
            }

            // Extract device path from the variable-length DevicePath field
            let path_ptr = &(*detail).DevicePath as *const u16;
            let path_len = (required_size as usize
                - std::mem::offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath))
                / 2;
            let path_slice = std::slice::from_raw_parts(path_ptr, path_len);
            let path = String::from_utf16_lossy(path_slice)
                .trim_end_matches('\0')
                .to_string();

            paths.push(path);
            index += 1;
        }

        let _ = SetupDiDestroyDeviceInfoList(dev_info);
    }

    Ok(paths)
}

/// Open an EMI device by path and return a handle.
fn open_emi_device(path: &str) -> Result<HANDLE> {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0x80), // FILE_ATTRIBUTE_NORMAL
            None,
        )
        .context("Failed to open EMI device")
    }
}

/// Read EMI version from a device handle.
fn read_version(handle: HANDLE) -> Result<u16> {
    let mut version = EMI_VERSION::default();
    let mut bytes_returned: u32 = 0;
    unsafe {
        DeviceIoControl(
            handle,
            IOCTL_EMI_GET_VERSION,
            None,
            0,
            Some(&mut version as *mut _ as *mut _),
            mem::size_of::<EMI_VERSION>() as u32,
            Some(&mut bytes_returned),
            None,
        )
        .context("IOCTL_EMI_GET_VERSION failed")?;
    }
    Ok(version.EmiVersion)
}

/// Read EMI metadata from a device handle.
fn read_metadata(handle: HANDLE) -> Result<(String, String, u16, String)> {
    let mut meta_size = EMI_METADATA_SIZE::default();
    let mut bytes_returned: u32 = 0;

    unsafe {
        DeviceIoControl(
            handle,
            IOCTL_EMI_GET_METADATA_SIZE,
            None,
            0,
            Some(&mut meta_size as *mut _ as *mut _),
            mem::size_of::<EMI_METADATA_SIZE>() as u32,
            Some(&mut bytes_returned),
            None,
        )
        .context("IOCTL_EMI_GET_METADATA_SIZE failed")?;

        let size = meta_size.MetadataSize as usize;
        let mut buf: Vec<u8> = vec![0u8; size];

        DeviceIoControl(
            handle,
            IOCTL_EMI_GET_METADATA,
            None,
            0,
            Some(buf.as_mut_ptr() as *mut _),
            size as u32,
            Some(&mut bytes_returned),
            None,
        )
        .context("IOCTL_EMI_GET_METADATA failed")?;

        // Parse V1 metadata manually from raw bytes:
        // offset 0:  MeasurementUnit (u32)
        // offset 4:  HardwareOEM [u16; 16] (32 bytes)
        // offset 36: HardwareModel [u16; 16] (32 bytes)
        // offset 68: HardwareRevision (u16)
        // offset 70: MeteredHardwareNameSize (u16)
        // offset 72: MeteredHardwareName [u16; N]

        let oem_slice = std::slice::from_raw_parts(buf.as_ptr().add(4) as *const u16, 16);
        let oem = String::from_utf16_lossy(oem_slice)
            .trim_end_matches('\0')
            .to_string();

        let model_slice = std::slice::from_raw_parts(buf.as_ptr().add(36) as *const u16, 16);
        let model = String::from_utf16_lossy(model_slice)
            .trim_end_matches('\0')
            .to_string();

        let hw_rev = *(buf.as_ptr().add(68) as *const u16);
        let name_size = *(buf.as_ptr().add(70) as *const u16) as usize;
        let name_chars = name_size / 2;

        let name = if name_chars > 0 && 72 + name_size <= buf.len() {
            let name_slice =
                std::slice::from_raw_parts(buf.as_ptr().add(72) as *const u16, name_chars);
            String::from_utf16_lossy(name_slice)
                .trim_end_matches('\0')
                .to_string()
        } else {
            String::new()
        };

        Ok((oem, model, hw_rev, name))
    }
}

/// Read a single measurement from an EMI device handle.
fn read_measurement_raw(handle: HANDLE) -> Result<EmiMeasurement> {
    let mut data = EMI_CHANNEL_MEASUREMENT_DATA::default();
    let mut bytes_returned: u32 = 0;
    unsafe {
        DeviceIoControl(
            handle,
            IOCTL_EMI_GET_MEASUREMENT,
            None,
            0,
            Some(&mut data as *mut _ as *mut _),
            mem::size_of::<EMI_CHANNEL_MEASUREMENT_DATA>() as u32,
            Some(&mut bytes_returned),
            None,
        )
        .context("IOCTL_EMI_GET_MEASUREMENT failed")?;
    }
    Ok(EmiMeasurement {
        energy_pwh: data.AbsoluteEnergy,
        time_100ns: data.AbsoluteTime,
    })
}

/// Discover all EMI devices, read their metadata, and return device info.
pub fn discover_devices() -> Result<Vec<EmiDevice>> {
    let paths = enumerate_emi_paths()?;
    let mut devices = Vec::new();

    for path in &paths {
        let handle = match open_emi_device(path) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("Warning: could not open {}: {}", path, e);
                continue;
            }
        };

        let version = read_version(handle)?;
        if version < 1 {
            unsafe {
                let _ = CloseHandle(handle);
            }
            bail!("Unsupported EMI version {} for {}", version, path);
        }

        let (oem, model, hw_rev, channel_name) = read_metadata(handle)?;
        let is_pac = path.to_lowercase().contains("mchp1940");

        unsafe {
            let _ = CloseHandle(handle);
        }

        devices.push(EmiDevice {
            path: path.clone(),
            oem,
            model,
            hw_revision: hw_rev,
            channel_name,
            is_pac,
        });
    }

    Ok(devices)
}

/// Take a single measurement from a device path.
pub fn measure(path: &str) -> Result<EmiMeasurement> {
    let handle = open_emi_device(path)?;
    let m = read_measurement_raw(handle)?;
    unsafe {
        let _ = CloseHandle(handle);
    }
    Ok(m)
}

/// Take two measurements separated by the given duration and compute power.
#[allow(dead_code)]
pub fn measure_power(path: &str, interval: std::time::Duration) -> Result<(EmiMeasurement, f64)> {
    let handle = open_emi_device(path)?;
    let m1 = read_measurement_raw(handle)?;
    std::thread::sleep(interval);
    let m2 = read_measurement_raw(handle)?;
    unsafe {
        let _ = CloseHandle(handle);
    }
    let watts = m2.power_watts(&m1);
    Ok((m2, watts))
}
