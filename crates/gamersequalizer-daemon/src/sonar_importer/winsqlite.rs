use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::w;

pub const SQLITE_OK: c_int = 0;
pub const SQLITE_ROW: c_int = 100;
pub const SQLITE_DONE: c_int = 101;
pub const SQLITE_OPEN_READONLY: c_int = 0x00000001;
const SQLITE_OPEN_READWRITE: c_int = 0x00000002;
const SQLITE_OPEN_CREATE: c_int = 0x00000004;
/// How long a read waits while the owner (Sonar, GOG Galaxy) holds a write lock.
const BUSY_TIMEOUT_MS: c_int = 500;

type FnSqlite3OpenV2 = unsafe extern "C" fn(*const c_char, *mut *mut c_void, c_int, *const c_char) -> c_int;
type FnSqlite3PrepareV2 =
  unsafe extern "C" fn(*mut c_void, *const c_char, c_int, *mut *mut c_void, *mut *const c_char) -> c_int;
type FnSqlite3Step = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnSqlite3ColumnText = unsafe extern "C" fn(*mut c_void, c_int) -> *const c_char;
type FnSqlite3ColumnInt = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type FnSqlite3Finalize = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnSqlite3CloseV2 = unsafe extern "C" fn(*mut c_void) -> c_int;
type FnSqlite3BusyTimeout = unsafe extern "C" fn(*mut c_void, c_int) -> c_int;
type FnSqlite3Exec =
  unsafe extern "C" fn(*mut c_void, *const c_char, *const c_void, *mut c_void, *mut *mut c_char) -> c_int;
type FnSqlite3Errmsg = unsafe extern "C" fn(*mut c_void) -> *const c_char;

pub struct WinSqliteLib {
  fn_open_v2: FnSqlite3OpenV2,
  fn_prepare_v2: FnSqlite3PrepareV2,
  fn_step: FnSqlite3Step,
  fn_column_text: FnSqlite3ColumnText,
  fn_column_int: FnSqlite3ColumnInt,
  fn_finalize: FnSqlite3Finalize,
  fn_close_v2: FnSqlite3CloseV2,
  fn_busy_timeout: FnSqlite3BusyTimeout,
  fn_exec: FnSqlite3Exec,
  fn_errmsg: FnSqlite3Errmsg,
}

impl WinSqliteLib {
  pub fn load() -> Result<Self, String> {
    unsafe {
      let h_mod = LoadLibraryW(w!("winsqlite3.dll")).map_err(|e| format!("Failed to load winsqlite3.dll: {}", e))?;

      macro_rules! get_fn {
        ($name:literal, $type:ty) => {{
          let sym =
            GetProcAddress(h_mod, windows::core::s!($name)).ok_or_else(|| format!("Symbol not found: {}", $name))?;
          std::mem::transmute::<unsafe extern "system" fn() -> isize, $type>(sym)
        }};
      }

      Ok(Self {
        fn_open_v2: get_fn!("sqlite3_open_v2", FnSqlite3OpenV2),
        fn_prepare_v2: get_fn!("sqlite3_prepare_v2", FnSqlite3PrepareV2),
        fn_step: get_fn!("sqlite3_step", FnSqlite3Step),
        fn_column_text: get_fn!("sqlite3_column_text", FnSqlite3ColumnText),
        fn_column_int: get_fn!("sqlite3_column_int", FnSqlite3ColumnInt),
        fn_finalize: get_fn!("sqlite3_finalize", FnSqlite3Finalize),
        fn_close_v2: get_fn!("sqlite3_close_v2", FnSqlite3CloseV2),
        fn_busy_timeout: get_fn!("sqlite3_busy_timeout", FnSqlite3BusyTimeout),
        fn_exec: get_fn!("sqlite3_exec", FnSqlite3Exec),
        fn_errmsg: get_fn!("sqlite3_errmsg", FnSqlite3Errmsg),
      })
    }
  }

  pub fn open_readonly(&self, path: &Path) -> Result<WinSqliteConn<'_>, String> {
    let path_str = path.to_str().ok_or_else(|| "Invalid UTF-8 path".to_string())?;
    self.open(path_str, SQLITE_OPEN_READONLY)
  }

  /// An empty database in memory, to run SQL scripts in.
  pub fn open_memory(&self) -> Result<WinSqliteConn<'_>, String> {
    self.open(":memory:", SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE)
  }

  fn open(&self, path: &str, flags: c_int) -> Result<WinSqliteConn<'_>, String> {
    let c_path = CString::new(path).map_err(|e| e.to_string())?;
    let mut db_ptr: *mut c_void = std::ptr::null_mut();
    unsafe {
      let rc = (self.fn_open_v2)(c_path.as_ptr(), &mut db_ptr, flags, std::ptr::null());
      if rc != SQLITE_OK {
        // A failed open still allocates a handle that must be closed.
        let _ = (self.fn_close_v2)(db_ptr);
        return Err(format!("sqlite3_open_v2 returned error code {}", rc));
      }
      // Without it a read fails at once (SQLITE_BUSY) while the owner writes.
      (self.fn_busy_timeout)(db_ptr, BUSY_TIMEOUT_MS);
    }

    Ok(WinSqliteConn { lib: self, db: db_ptr })
  }
}

pub struct WinSqliteConn<'a> {
  lib: &'a WinSqliteLib,
  db: *mut c_void,
}

/// (name, vad, is_preset, data JSON, game_ids "1_730,8_635").
pub type ConfigRow = (String, u32, bool, String, String);

impl<'a> WinSqliteConn<'a> {
  /// Runs every statement of `sql` (no results).
  pub fn exec(&self, sql: &str) -> Result<(), String> {
    let sql = CString::new(sql).map_err(|e| e.to_string())?;
    // SAFETY: no callback; the error text is copied before the next call on this connection.
    unsafe {
      let rc = (self.lib.fn_exec)(self.db, sql.as_ptr(), std::ptr::null(), std::ptr::null_mut(), std::ptr::null_mut());
      if rc == SQLITE_OK {
        return Ok(());
      }
      let msg = (self.lib.fn_errmsg)(self.db);
      let msg = if msg.is_null() { String::new() } else { CStr::from_ptr(msg).to_string_lossy().into_owned() };
      Err(format!("sqlite3_exec returned error code {rc}: {msg}"))
    }
  }

  /// (name, vad, is_preset, data, game_ids). Older Sonar databases have no `game_ids` column.
  pub fn query_configs(&self) -> Result<Vec<ConfigRow>, String> {
    self
      .query_configs_sql(
        "SELECT name, vad, is_preset, data, game_ids FROM configs WHERE data IS NOT NULL ORDER BY vad, name",
      )
      .or_else(|_| {
        self.query_configs_sql(
          "SELECT name, vad, is_preset, data, '' FROM configs WHERE data IS NOT NULL ORDER BY vad, name",
        )
      })
  }

  /// Every row of `sql` as its first `columns` columns of text (NULL = "").
  pub fn query_text(&self, sql: &str, columns: c_int) -> Result<Vec<Vec<String>>, String> {
    let sql = CString::new(sql).map_err(|e| e.to_string())?;
    let mut stmt: *mut c_void = std::ptr::null_mut();
    // SAFETY: `stmt` is finalized on every path; column pointers are copied before the next step.
    unsafe {
      let rc = (self.lib.fn_prepare_v2)(self.db, sql.as_ptr(), -1, &mut stmt, std::ptr::null_mut());
      if rc != SQLITE_OK {
        return Err(format!("sqlite3_prepare_v2 returned error code {rc}"));
      }
      let mut rows = Vec::new();
      loop {
        match (self.lib.fn_step)(stmt) {
          SQLITE_ROW => rows.push(
            (0..columns)
              .map(|i| {
                let p = (self.lib.fn_column_text)(stmt, i);
                if p.is_null() { String::new() } else { CStr::from_ptr(p).to_string_lossy().into_owned() }
              })
              .collect(),
          ),
          SQLITE_DONE => break,
          rc => {
            let _ = (self.lib.fn_finalize)(stmt);
            return Err(format!("sqlite3_step failed with error code {rc}"));
          }
        }
      }
      let _ = (self.lib.fn_finalize)(stmt);
      Ok(rows)
    }
  }

  fn query_configs_sql(&self, sql: &str) -> Result<Vec<ConfigRow>, String> {
    let sql = CString::new(sql).map_err(|e| e.to_string())?;

    let mut stmt: *mut c_void = std::ptr::null_mut();
    unsafe {
      let rc = (self.lib.fn_prepare_v2)(self.db, sql.as_ptr(), -1, &mut stmt, std::ptr::null_mut());
      if rc != SQLITE_OK {
        return Err(format!("sqlite3_prepare_v2 returned error code {}", rc));
      }

      let mut results = Vec::new();
      loop {
        let step_rc = (self.lib.fn_step)(stmt);
        if step_rc == SQLITE_ROW {
          let name_ptr = (self.lib.fn_column_text)(stmt, 0);
          let vad = (self.lib.fn_column_int)(stmt, 1) as u32;
          let is_preset_int = (self.lib.fn_column_int)(stmt, 2);
          let data_ptr = (self.lib.fn_column_text)(stmt, 3);
          let ids_ptr = (self.lib.fn_column_text)(stmt, 4);

          let name =
            if !name_ptr.is_null() { CStr::from_ptr(name_ptr).to_string_lossy().into_owned() } else { String::new() };

          let data =
            if !data_ptr.is_null() { CStr::from_ptr(data_ptr).to_string_lossy().into_owned() } else { String::new() };

          let game_ids =
            if !ids_ptr.is_null() { CStr::from_ptr(ids_ptr).to_string_lossy().into_owned() } else { String::new() };

          results.push((name, vad, is_preset_int != 0, data, game_ids));
        } else if step_rc == SQLITE_DONE {
          break;
        } else {
          let _ = (self.lib.fn_finalize)(stmt);
          return Err(format!("sqlite3_step failed with error code {}", step_rc));
        }
      }

      let _ = (self.lib.fn_finalize)(stmt);
      Ok(results)
    }
  }
}

impl<'a> Drop for WinSqliteConn<'a> {
  fn drop(&mut self) {
    if !self.db.is_null() {
      unsafe {
        let _ = (self.lib.fn_close_v2)(self.db);
      }
    }
  }
}
