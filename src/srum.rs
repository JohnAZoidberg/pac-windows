use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem;
use std::path::Path;
use windows::Win32::Storage::Jet::*;
use windows::Win32::Storage::StructuredStorage::JET_TABLEID;

// FFI for JetSetSystemParameterW (not in windows crate)
#[link(name = "esent")]
unsafe extern "system" {
    fn JetSetSystemParameterW(
        pinstance: *mut JET_INSTANCE,
        sesid: JET_SESID,
        paramid: u32,
        lparam: usize,
        szparam: *const u16,
    ) -> i32;
}

// Constants not exposed by the windows crate
const JET_MOVE_FIRST: i32 = -2147483648i32; // 0x80000000 as i32
const JET_MOVE_NEXT: i32 = 1;
const JET_COL_INFO_LIST: u32 = 1;
const JET_PARAM_DATABASE_PAGE_SIZE: u32 = 64;
const JET_PARAM_RECOVERY: u32 = 34;

/// SRUM table GUIDs for energy data.
const ENERGY_USAGE_TABLE: &str = "{FEE4E14F-02A9-4550-B5CE-5FA2DA202E37}";
const ENERGY_USAGE_LT_TABLE: &str = "{FEE4E14F-02A9-4550-B5CE-5FA2DA202E37}LT";
const ENERGY_ESTIMATOR_TABLE: &str = "{DA73FB89-2BEA-4DDC-86B8-6E048C6DA477}";
const ID_MAP_TABLE: &str = "SruDbIdMapTable";

/// Check a JET error code, returning an error if negative.
fn jet_check(err: i32, op: &str) -> Result<()> {
    if err < 0 {
        bail!("{} failed with JET error {}", op, err);
    }
    Ok(())
}

/// A row from the energy usage table.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct EnergyRecord {
    pub timestamp: f64, // OLE Automation date
    pub app_id: String,
    pub user_id: String,
    pub columns: HashMap<String, ColumnValue>,
}

#[derive(Debug, Clone)]
pub enum ColumnValue {
    Long(i32),
    LongLong(i64),
    UnsignedLong(u32),
    Short(i16),
    Float(f64),
    DateTime(f64),
    Text(String),
    Binary(Vec<u8>),
    Null,
}

impl std::fmt::Display for ColumnValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ColumnValue::Long(v) => write!(f, "{}", v),
            ColumnValue::LongLong(v) => write!(f, "{}", v),
            ColumnValue::UnsignedLong(v) => write!(f, "{}", v),
            ColumnValue::Short(v) => write!(f, "{}", v),
            ColumnValue::Float(v) => write!(f, "{:.4}", v),
            ColumnValue::DateTime(v) => {
                // OLE date to something readable
                let days_since_epoch = *v - 25569.0; // OLE epoch (1899-12-30) to Unix epoch
                let secs = (days_since_epoch * 86400.0) as i64;
                let dt = chrono::DateTime::from_timestamp(secs, 0).unwrap_or_default();
                write!(f, "{}", dt.format("%Y-%m-%d %H:%M:%S"))
            }
            ColumnValue::Text(v) => write!(f, "{}", v),
            ColumnValue::Binary(v) => write!(f, "{:02x?}", v),
            ColumnValue::Null => write!(f, "NULL"),
        }
    }
}

/// Column info from JET_COLUMNLIST enumeration.
struct ColumnInfo {
    name: String,
    id: u32,
    col_type: u32,
}

/// SRUM database handle wrapping JET session state.
pub struct SrumDatabase {
    instance: JET_INSTANCE,
    sesid: JET_SESID,
    dbid: u32,
    id_map: HashMap<u32, String>,
}

/// Read the ESE database page size from the file header (offset 0xEC).
fn read_ese_page_size(path: &Path) -> Result<usize> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).context("Could not open database file")?;
    let mut header = [0u8; 0xF0];
    f.read_exact(&mut header)
        .context("Could not read database header")?;
    let page_size = u32::from_le_bytes([header[0xEC], header[0xED], header[0xEE], header[0xEF]]);
    if page_size == 0 || !page_size.is_power_of_two() || !(4096..=32768).contains(&page_size) {
        bail!("Unexpected ESE page size {} in database header", page_size);
    }
    Ok(page_size as usize)
}

impl SrumDatabase {
    /// Open a SRUM database file (must be a copy, not the live locked file).
    pub fn open(db_path: &Path) -> Result<Self> {
        let db_path_str = db_path.to_str().context("Invalid path")?;
        let db_wide: Vec<u16> = db_path_str
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        // Read page size from ESE database header (offset 0xEC, 4 bytes LE)
        let page_size = read_ese_page_size(db_path)?;
        eprintln!("Detected ESE page size: {} bytes", page_size);

        unsafe {
            let mut instance = JET_INSTANCE(0);

            // Create instance
            let instance_name: Vec<u16> = "pac_srum\0".encode_utf16().collect();
            jet_check(
                JetCreateInstanceW(&mut instance, Some(instance_name.as_ptr())),
                "JetCreateInstanceW",
            )?;

            // Set page size (must be set on the instance before JetInit)
            jet_check(
                JetSetSystemParameterW(
                    &mut instance,
                    JET_SESID(0),
                    JET_PARAM_DATABASE_PAGE_SIZE,
                    page_size,
                    std::ptr::null(),
                ),
                "JetSetSystemParameter(PageSize)",
            )?;

            // Disable recovery (we're read-only on a copy)
            let off: Vec<u16> = "0\0".encode_utf16().collect();
            jet_check(
                JetSetSystemParameterW(
                    &mut instance,
                    JET_SESID(0),
                    JET_PARAM_RECOVERY,
                    0,
                    off.as_ptr(),
                ),
                "JetSetSystemParameter(Recovery)",
            )?;

            // Initialize
            jet_check(JetInit(Some(&mut instance)), "JetInit")?;

            // Begin session
            let mut sesid = JET_SESID(0);
            jet_check(
                JetBeginSessionW(instance, &mut sesid, None, None),
                "JetBeginSessionW",
            )?;

            // Attach database read-only
            jet_check(
                JetAttachDatabaseW(sesid, db_wide.as_ptr(), JET_bitDbReadOnly),
                "JetAttachDatabaseW",
            )?;

            // Open database
            let mut dbid: u32 = 0;
            jet_check(
                JetOpenDatabaseW(sesid, db_wide.as_ptr(), None, &mut dbid, JET_bitDbReadOnly),
                "JetOpenDatabaseW",
            )?;

            let mut db = SrumDatabase {
                instance,
                sesid,
                dbid,
                id_map: HashMap::new(),
            };
            db.load_id_map()?;
            Ok(db)
        }
    }

    /// Load the SruDbIdMapTable to resolve numeric IDs to app names/SIDs.
    fn load_id_map(&mut self) -> Result<()> {
        let table_name: Vec<u16> = ID_MAP_TABLE
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut tableid = JET_TABLEID(0);

        unsafe {
            let err = JetOpenTableW(
                self.sesid,
                self.dbid,
                table_name.as_ptr(),
                None,
                0,
                JET_bitDbReadOnly,
                &mut tableid,
            );
            if err < 0 {
                eprintln!("Warning: Could not open SruDbIdMapTable (err {})", err);
                return Ok(());
            }

            let columns = self.get_column_info(tableid)?;
            let id_type_col = columns.iter().find(|c| c.name == "IdType");
            let id_index_col = columns.iter().find(|c| c.name == "IdIndex");
            let id_blob_col = columns.iter().find(|c| c.name == "IdBlob");

            if id_index_col.is_none() || id_blob_col.is_none() {
                let _ = JetCloseTable(self.sesid, tableid);
                return Ok(());
            }

            let id_index_id = id_index_col.unwrap().id;
            let id_blob_id = id_blob_col.unwrap().id;
            let id_type_id = id_type_col.map(|c| c.id);

            // Move to first record
            let mut err = JetMove(self.sesid, tableid, JET_MOVE_FIRST, 0);
            while err >= 0 {
                let index = self.retrieve_u32(tableid, id_index_id).unwrap_or(0);
                let blob = self.retrieve_bytes(tableid, id_blob_id);
                let id_type = id_type_id
                    .and_then(|id| self.retrieve_u32(tableid, id))
                    .unwrap_or(0);

                let value = match &blob {
                    Some(b) if id_type == 3 || is_binary_sid(b) => binary_sid_to_string(b),
                    Some(b) => bytes_to_string(b),
                    None => String::new(),
                };

                if !value.is_empty() {
                    self.id_map.insert(index, value);
                }

                err = JetMove(self.sesid, tableid, JET_MOVE_NEXT, 0);
            }

            let _ = JetCloseTable(self.sesid, tableid);
        }
        eprintln!("Loaded {} ID map entries", self.id_map.len());
        Ok(())
    }

    /// Get column info for a table using JetGetTableColumnInfoW with JET_ColInfoList.
    fn get_column_info(&self, tableid: JET_TABLEID) -> Result<Vec<ColumnInfo>> {
        let mut columns = Vec::new();

        unsafe {
            let mut column_list = JET_COLUMNLIST {
                cbStruct: mem::size_of::<JET_COLUMNLIST>() as u32,
                ..Default::default()
            };

            let err = JetGetTableColumnInfoW(
                self.sesid,
                tableid,
                None,
                &mut column_list as *mut _ as *mut c_void,
                mem::size_of::<JET_COLUMNLIST>() as u32,
                JET_COL_INFO_LIST,
            );
            if err < 0 {
                bail!("JetGetTableColumnInfoW failed: {}", err);
            }

            let list_tableid = column_list.tableid;
            let col_name_id = column_list.columnidcolumnname;
            let col_id_id = column_list.columnidcolumnid;
            let col_type_id = column_list.columnidcoltyp;

            let mut move_err = JetMove(self.sesid, list_tableid, JET_MOVE_FIRST, 0);
            while move_err >= 0 {
                let name = self
                    .retrieve_string(list_tableid, col_name_id)
                    .unwrap_or_default();
                let id = self.retrieve_u32(list_tableid, col_id_id).unwrap_or(0);
                let col_type = self.retrieve_u32(list_tableid, col_type_id).unwrap_or(0);

                if !name.is_empty() {
                    columns.push(ColumnInfo { name, id, col_type });
                }
                move_err = JetMove(self.sesid, list_tableid, JET_MOVE_NEXT, 0);
            }

            let _ = JetCloseTable(self.sesid, list_tableid);
        }

        Ok(columns)
    }

    /// Retrieve a u32 value from a column.
    fn retrieve_u32(&self, tableid: JET_TABLEID, column_id: u32) -> Option<u32> {
        let mut value: u32 = 0;
        let mut actual: u32 = 0;
        unsafe {
            let err = JetRetrieveColumn(
                self.sesid,
                tableid,
                column_id,
                Some(&mut value as *mut _ as *mut c_void),
                4,
                Some(&mut actual),
                0,
                None,
            );
            if err >= 0 && actual == 4 {
                Some(value)
            } else {
                None
            }
        }
    }

    /// Retrieve a string from a column, auto-detecting UTF-16 vs UTF-8.
    fn retrieve_string(&self, tableid: JET_TABLEID, column_id: u32) -> Option<String> {
        let mut buf = [0u8; 512];
        let mut actual: u32 = 0;
        unsafe {
            let err = JetRetrieveColumn(
                self.sesid,
                tableid,
                column_id,
                Some(buf.as_mut_ptr() as *mut c_void),
                buf.len() as u32,
                Some(&mut actual),
                0,
                None,
            );
            if err >= 0 && actual > 0 {
                let data = &buf[..actual as usize];
                Some(bytes_to_string(data))
            } else {
                None
            }
        }
    }

    /// Retrieve raw bytes from a column.
    fn retrieve_bytes(&self, tableid: JET_TABLEID, column_id: u32) -> Option<Vec<u8>> {
        let mut buf = [0u8; 4096];
        let mut actual: u32 = 0;
        unsafe {
            let err = JetRetrieveColumn(
                self.sesid,
                tableid,
                column_id,
                Some(buf.as_mut_ptr() as *mut c_void),
                buf.len() as u32,
                Some(&mut actual),
                0,
                None,
            );
            if err >= 0 && actual > 0 {
                Some(buf[..actual as usize].to_vec())
            } else {
                None
            }
        }
    }

    /// Retrieve a column value based on its JET column type.
    fn retrieve_column_value(&self, tableid: JET_TABLEID, col: &ColumnInfo) -> ColumnValue {
        match col.col_type {
            // JET_coltypBit = 1
            1 => self
                .retrieve_u32(tableid, col.id)
                .map(|v| ColumnValue::Long(v as i32))
                .unwrap_or(ColumnValue::Null),
            // JET_coltypUnsignedByte = 2
            2 => self
                .retrieve_u32(tableid, col.id)
                .map(|v| ColumnValue::Long(v as i32))
                .unwrap_or(ColumnValue::Null),
            // JET_coltypShort = 3
            3 => {
                let mut val: i16 = 0;
                let mut actual: u32 = 0;
                unsafe {
                    let err = JetRetrieveColumn(
                        self.sesid,
                        tableid,
                        col.id,
                        Some(&mut val as *mut _ as *mut c_void),
                        2,
                        Some(&mut actual),
                        0,
                        None,
                    );
                    if err >= 0 && actual == 2 {
                        ColumnValue::Short(val)
                    } else {
                        ColumnValue::Null
                    }
                }
            }
            // JET_coltypLong = 4
            4 => {
                let mut val: i32 = 0;
                let mut actual: u32 = 0;
                unsafe {
                    let err = JetRetrieveColumn(
                        self.sesid,
                        tableid,
                        col.id,
                        Some(&mut val as *mut _ as *mut c_void),
                        4,
                        Some(&mut actual),
                        0,
                        None,
                    );
                    if err >= 0 && actual == 4 {
                        ColumnValue::Long(val)
                    } else {
                        ColumnValue::Null
                    }
                }
            }
            // JET_coltypCurrency = 5, JET_coltypIEEESingle = 6, JET_coltypIEEEDouble = 7
            5 | 7 => {
                let mut val: f64 = 0.0;
                let mut actual: u32 = 0;
                unsafe {
                    let err = JetRetrieveColumn(
                        self.sesid,
                        tableid,
                        col.id,
                        Some(&mut val as *mut _ as *mut c_void),
                        8,
                        Some(&mut actual),
                        0,
                        None,
                    );
                    if err >= 0 && actual == 8 {
                        ColumnValue::Float(val)
                    } else {
                        ColumnValue::Null
                    }
                }
            }
            6 => {
                let mut val: f32 = 0.0;
                let mut actual: u32 = 0;
                unsafe {
                    let err = JetRetrieveColumn(
                        self.sesid,
                        tableid,
                        col.id,
                        Some(&mut val as *mut _ as *mut c_void),
                        4,
                        Some(&mut actual),
                        0,
                        None,
                    );
                    if err >= 0 && actual == 4 {
                        ColumnValue::Float(val as f64)
                    } else {
                        ColumnValue::Null
                    }
                }
            }
            // JET_coltypDateTime = 8
            8 => {
                let mut val: f64 = 0.0;
                let mut actual: u32 = 0;
                unsafe {
                    let err = JetRetrieveColumn(
                        self.sesid,
                        tableid,
                        col.id,
                        Some(&mut val as *mut _ as *mut c_void),
                        8,
                        Some(&mut actual),
                        0,
                        None,
                    );
                    if err >= 0 && actual == 8 {
                        ColumnValue::DateTime(val)
                    } else {
                        ColumnValue::Null
                    }
                }
            }
            // JET_coltypBinary = 9, JET_coltypText = 10, JET_coltypLongBinary = 11, JET_coltypLongText = 12
            10 | 12 => self
                .retrieve_string(tableid, col.id)
                .map(ColumnValue::Text)
                .unwrap_or(ColumnValue::Null),
            9 | 11 => self
                .retrieve_bytes(tableid, col.id)
                .map(ColumnValue::Binary)
                .unwrap_or(ColumnValue::Null),
            // JET_coltypUnsignedLong = 14
            14 => self
                .retrieve_u32(tableid, col.id)
                .map(ColumnValue::UnsignedLong)
                .unwrap_or(ColumnValue::Null),
            // JET_coltypLongLong = 15, JET_coltypUnsignedShort = 17, JET_coltypGUID = 16
            15 => {
                let mut val: i64 = 0;
                let mut actual: u32 = 0;
                unsafe {
                    let err = JetRetrieveColumn(
                        self.sesid,
                        tableid,
                        col.id,
                        Some(&mut val as *mut _ as *mut c_void),
                        8,
                        Some(&mut actual),
                        0,
                        None,
                    );
                    if err >= 0 && actual == 8 {
                        ColumnValue::LongLong(val)
                    } else {
                        ColumnValue::Null
                    }
                }
            }
            16 => self
                .retrieve_bytes(tableid, col.id)
                .map(ColumnValue::Binary)
                .unwrap_or(ColumnValue::Null),
            _ => self
                .retrieve_bytes(tableid, col.id)
                .map(ColumnValue::Binary)
                .unwrap_or(ColumnValue::Null),
        }
    }

    /// Read all records from a named table, resolving AppId/UserId via id_map.
    pub fn read_table(&self, table_guid: &str) -> Result<(Vec<String>, Vec<Vec<ColumnValue>>)> {
        let table_name: Vec<u16> = table_guid
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut tableid = JET_TABLEID(0);

        unsafe {
            jet_check(
                JetOpenTableW(
                    self.sesid,
                    self.dbid,
                    table_name.as_ptr(),
                    None,
                    0,
                    JET_bitDbReadOnly,
                    &mut tableid,
                ),
                &format!("JetOpenTableW({})", table_guid),
            )?;
        }

        let columns = self.get_column_info(tableid)?;
        let col_names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        let mut rows: Vec<Vec<ColumnValue>> = Vec::new();

        unsafe {
            let mut err = JetMove(self.sesid, tableid, JET_MOVE_FIRST, 0);
            while err >= 0 {
                let mut row: Vec<ColumnValue> = Vec::with_capacity(columns.len());
                for col in &columns {
                    let mut val = self.retrieve_column_value(tableid, col);
                    // Resolve AppId and UserId through id_map
                    if col.name == "AppId" || col.name == "UserId" {
                        let numeric_id = match &val {
                            ColumnValue::Long(v) => Some(*v as u32),
                            ColumnValue::UnsignedLong(v) => Some(*v),
                            ColumnValue::Short(v) => Some(*v as u32),
                            ColumnValue::LongLong(v) => Some(*v as u32),
                            _ => None,
                        };
                        if let Some(id) = numeric_id
                            && let Some(resolved) = self.id_map.get(&id)
                        {
                            val = ColumnValue::Text(resolved.clone());
                        }
                    }
                    row.push(val);
                }
                rows.push(row);
                err = JetMove(self.sesid, tableid, JET_MOVE_NEXT, 0);
            }

            let _ = JetCloseTable(self.sesid, tableid);
        }

        Ok((col_names, rows))
    }

    /// List all tables in the database with row counts.
    pub fn list_tables(&self) -> Result<Vec<(String, usize)>> {
        let mut tables = Vec::new();
        let known = [
            ENERGY_USAGE_TABLE,
            ENERGY_USAGE_LT_TABLE,
            ENERGY_ESTIMATOR_TABLE,
            "{B6D82AF1-F780-4E17-8077-6CB9AD8A6FC4}",
            "{D10CA2FE-6FCF-4F6D-848E-B2E99266FA89}",
            "{973F5D5C-1D90-4944-BE8E-24B94231A174}",
            "{DD6636C4-8929-4683-974E-22C046A43763}",
        ];
        for name in &known {
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let mut tableid = JET_TABLEID(0);
            unsafe {
                let err = JetOpenTableW(
                    self.sesid,
                    self.dbid,
                    wide.as_ptr(),
                    None,
                    0,
                    JET_bitDbReadOnly,
                    &mut tableid,
                );
                if err >= 0 {
                    let mut count: usize = 0;
                    let mut move_err = JetMove(self.sesid, tableid, JET_MOVE_FIRST, 0);
                    while move_err >= 0 {
                        count += 1;
                        move_err = JetMove(self.sesid, tableid, JET_MOVE_NEXT, 0);
                    }
                    tables.push((name.to_string(), count));
                    let _ = JetCloseTable(self.sesid, tableid);
                }
            }
        }
        Ok(tables)
    }

    /// Read energy usage data, returning records with resolved names.
    #[allow(dead_code)]
    pub fn read_energy_usage(&self) -> Result<(Vec<String>, Vec<Vec<ColumnValue>>)> {
        self.read_table(ENERGY_USAGE_TABLE)
    }

    /// Read long-term energy usage data.
    #[allow(dead_code)]
    pub fn read_energy_usage_lt(&self) -> Result<(Vec<String>, Vec<Vec<ColumnValue>>)> {
        self.read_table(ENERGY_USAGE_LT_TABLE)
    }

    /// Resolve an ID to its mapped name.
    #[allow(dead_code)]
    pub fn resolve_id(&self, id: u32) -> Option<&str> {
        self.id_map.get(&id).map(|s| s.as_str())
    }
}

impl Drop for SrumDatabase {
    fn drop(&mut self) {
        unsafe {
            let _ = JetEndSession(self.sesid, 0);
            let _ = JetTerm(self.instance);
        }
    }
}

/// Check if a byte slice looks like a binary SID (starts with revision=1, has valid structure).
fn is_binary_sid(data: &[u8]) -> bool {
    if data.len() < 8 {
        return false;
    }
    let revision = data[0];
    let sub_count = data[1] as usize;
    // SID revision is always 1, sub-authority count should be reasonable,
    // and total length should match: 8 + sub_count * 4
    revision == 1 && sub_count <= 15 && data.len() >= 8 + sub_count * 4
}

/// Convert a binary SID to its string representation (S-1-5-21-...).
fn binary_sid_to_string(data: &[u8]) -> String {
    if data.len() < 8 {
        return String::new();
    }
    let revision = data[0];
    let sub_authority_count = data[1] as usize;
    let authority = u64::from(data[2]) << 40
        | u64::from(data[3]) << 32
        | u64::from(data[4]) << 24
        | u64::from(data[5]) << 16
        | u64::from(data[6]) << 8
        | u64::from(data[7]);

    let mut sid = format!("S-{}-{}", revision, authority);
    for i in 0..sub_authority_count {
        let offset = 8 + i * 4;
        if offset + 4 > data.len() {
            break;
        }
        let sub = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        sid.push_str(&format!("-{}", sub));
    }
    sid
}

/// Smart string decode: if the data looks like UTF-16LE (has null bytes in the
/// pattern of UTF-16 ASCII), decode as UTF-16. Otherwise fall back to UTF-8.
fn bytes_to_string(data: &[u8]) -> String {
    // Heuristic: if every other byte is 0x00 and the others are printable ASCII,
    // it's UTF-16LE.
    if data.len() >= 2
        && data.len().is_multiple_of(2)
        && data.chunks_exact(2).all(|c| c[1] == 0 || c[0] != 0)
        && data.iter().step_by(2).any(|&b| b > 0x20 && b < 0x7F)
        && data.iter().skip(1).step_by(2).all(|&b| b == 0)
    {
        let u16s: Vec<u16> = data
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&u16s)
            .trim_end_matches('\0')
            .to_string();
    }
    String::from_utf8_lossy(data)
        .trim_end_matches('\0')
        .to_string()
}

/// Friendly names for known SRUM tables.
pub fn table_friendly_name(guid: &str) -> &str {
    match guid {
        "{FEE4E14F-02A9-4550-B5CE-5FA2DA202E37}" => "Energy Usage",
        "{FEE4E14F-02A9-4550-B5CE-5FA2DA202E37}LT" => "Energy Usage (Long Term)",
        "{DA73FB89-2BEA-4DDC-86B8-6E048C6DA477}" => "Energy Estimator Provider",
        "{B6D82AF1-F780-4E17-8077-6CB9AD8A6FC4}" => "Tagged Energy Provider",
        "{D10CA2FE-6FCF-4F6D-848E-B2E99266FA89}" => "Application Resource Usage",
        "{973F5D5C-1D90-4944-BE8E-24B94231A174}" => "Network Data Usage",
        "{DD6636C4-8929-4683-974E-22C046A43763}" => "Network Connectivity Usage",
        _ => guid,
    }
}
