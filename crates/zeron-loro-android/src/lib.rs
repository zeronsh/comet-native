use std::sync::{Arc, Mutex};

use loro::LoroDoc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LoroAndroidError {
    #[error("loro: {0}")]
    Loro(String),
    #[error("malformed bytes")]
    Malformed,
    #[error("doc closed")]
    Closed,
}

pub struct ZeronLoroDoc {
    inner: Mutex<Option<LoroDoc>>,
}

impl ZeronLoroDoc {
    fn with<R>(&self, f: impl FnOnce(&LoroDoc) -> Result<R, LoroAndroidError>) -> Result<R, LoroAndroidError> {
        let g = self.inner.lock().unwrap();
        let doc = g.as_ref().ok_or(LoroAndroidError::Closed)?;
        f(doc)
    }
}

pub fn create_doc() -> Arc<ZeronLoroDoc> {
    Arc::new(ZeronLoroDoc {
        inner: Mutex::new(Some(LoroDoc::new())),
    })
}

pub fn doc_from_bytes(data: Vec<u8>) -> Result<Arc<ZeronLoroDoc>, LoroAndroidError> {
    let doc = LoroDoc::new();
    doc.import(&data).map_err(|_| LoroAndroidError::Malformed)?;
    Ok(Arc::new(ZeronLoroDoc {
        inner: Mutex::new(Some(doc)),
    }))
}

pub fn doc_export_snapshot(doc: Arc<ZeronLoroDoc>) -> Result<Vec<u8>, LoroAndroidError> {
    doc.with(|d| d.export(loro::ExportMode::Snapshot).map_err(|e| LoroAndroidError::Loro(e.to_string())))
}

pub fn doc_export_updates(
    doc: Arc<ZeronLoroDoc>,
    _from_version: Option<Vec<u8>>,
) -> Result<Vec<u8>, LoroAndroidError> {
    doc.with(|d| d.export(loro::ExportMode::Snapshot).map_err(|e| LoroAndroidError::Loro(e.to_string())))
}

pub fn doc_import_bytes(doc: Arc<ZeronLoroDoc>, data: Vec<u8>) -> Result<(), LoroAndroidError> {
    doc.with(|d| {
        d.import(&data).map_err(|_| LoroAndroidError::Malformed)?;
        Ok(())
    })
}

pub fn doc_get_deep_value(doc: Arc<ZeronLoroDoc>) -> Result<String, LoroAndroidError> {
    doc.with(|d| {
        let v = d.get_deep_value();
        Ok(serde_json::to_string(&v).unwrap_or_else(|_| "{}".into()))
    })
}

pub fn doc_get_frontiers(doc: Arc<ZeronLoroDoc>) -> Result<String, LoroAndroidError> {
    doc.with(|d| {
        let f = d.state_frontiers();
        Ok(format!("{f:?}"))
    })
}

pub fn doc_contains_frontier(
    doc: Arc<ZeronLoroDoc>,
    frontier: Vec<u8>,
) -> Result<bool, LoroAndroidError> {
    doc.with(|_d| {
        let s = String::from_utf8_lossy(&frontier);
        let _v: Result<serde_json::Value, _> = serde_json::from_str(&s);
        Ok(false)
    })
}

pub fn doc_close(doc: Arc<ZeronLoroDoc>) -> Result<(), LoroAndroidError> {
    let mut g = doc.inner.lock().unwrap();
    *g = None;
    Ok(())
}

// ── C ABI (dyn-loaded from Kotlin via JNI `registerNativeMethods`, or raw)
//    These are the single stable entry points the Android `System.loadLibrary`
//    boundary needs. Handles are opaque `*mut ZeronLoroDoc` passed by Kotlin.

#[unsafe(no_mangle)]
pub extern "C" fn zla_create() -> *mut std::ffi::c_void {
    Arc::into_raw(create_doc()) as *mut std::ffi::c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn zla_read(handle: *mut std::ffi::c_void) -> *mut std::ffi::c_char {
    if handle.is_null() { return std::ptr::null_mut() }
    // Borrow without taking the Arc: the pointer is owned by Kotlin until
    // zla_free. Reconstruct a transient Arc from a clone reference is unsafe;
    // instead increment the refcount properly.
    let arc: Arc<ZeronLoroDoc> = {
        let ptr = handle as *const ZeronLoroDoc;
        unsafe { Arc::increment_strong_count(ptr); Arc::from_raw(ptr) }
    };
    let json = doc_get_deep_value(arc).unwrap_or_else(|_| "{}".into());
    std::ffi::CString::new(json).unwrap_or_default().into_raw()
}

#[unsafe(no_mangle)]
pub extern "C" fn zla_import(handle: *mut std::ffi::c_void, data: *const u8, len: usize) -> i32 {
    if handle.is_null() { return 2 }
    let arc: Arc<ZeronLoroDoc> = {
        let ptr = handle as *const ZeronLoroDoc;
        unsafe { Arc::increment_strong_count(ptr); Arc::from_raw(ptr) }
    };
    let slice = unsafe { std::slice::from_raw_parts(data, len) };
    let ok = {
        let g = arc.inner.lock().unwrap();
        match g.as_ref() {
            Some(doc) => doc.import(slice).is_ok(),
            None => false,
        }
    };
    drop(arc);
    if ok { 0 } else { 1 }
}

/// Append a durable command entry to the root `commands` LoroList (schema.rs).
/// `cmd_json` is a complete entry `{id,kind,payload,issuedBy,issuedAt,status,resolution}`.
#[unsafe(no_mangle)]
pub extern "C" fn zla_append_command(handle: *mut std::ffi::c_void, cmd_json: *const std::ffi::c_char) -> i32 {
    if handle.is_null() || cmd_json.is_null() { return 2 }
    let ok = {
        let arc = {
            let ptr = handle as *const ZeronLoroDoc;
            unsafe { Arc::increment_strong_count(ptr); Arc::from_raw(ptr) }
        };
        let raw = unsafe { std::ffi::CStr::from_ptr(cmd_json).to_string_lossy().into_owned() };
        let value = serde_json::from_str::<serde_json::Value>(&raw).unwrap_or(serde_json::json!({}));
        let r = {
            let g = arc.inner.lock().unwrap();
            match g.as_ref() {
                Some(doc) => {
                    let list = doc.get_list("commands");
                    list.insert(list.len(), loro::LoroValue::from(value));
                    true
                }
                None => false,
            }
        };
        drop(arc);
        r
    };
    if ok { 0 } else { 1 }
}

/// Export the doc's own updates as a hex string (Kotlin marshals bytes as hex).
/// Returns null on failure; the caller frees with `zla_free_string`.
#[unsafe(no_mangle)]
pub extern "C" fn zla_export_hex(handle: *mut std::ffi::c_void) -> *mut std::ffi::c_char {
    if handle.is_null() { return std::ptr::null_mut() }
    let arc: Arc<ZeronLoroDoc> = {
        let ptr = handle as *const ZeronLoroDoc;
        unsafe { Arc::increment_strong_count(ptr); Arc::from_raw(ptr) }
    };
    let bytes = match doc_export_snapshot(arc) {
        Ok(b) => b,
        Err(_) => return std::ptr::null_mut(),
    };
    let mut hex = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        hex.push_str(&format!("{b:02x}"));
    }
    std::ffi::CString::new(hex).unwrap_or_default().into_raw()
}

#[unsafe(no_mangle)]
pub extern "C" fn zla_free(handle: *mut std::ffi::c_void) {
    if handle.is_null() { return }
    let ptr = handle as *const ZeronLoroDoc;
    unsafe { Arc::from_raw(ptr) }; // drops the owned ref from the Kotlin side
}

// ── JNI exports — the Kotlin `NativeDocBridge` object calls these after
//    `System.loadLibrary("zeron_loro_android")`. Handles cross as `jlong`,
//    bytes via `GetByteArrayElements`, strings via Java String.
#[cfg(any(target_os = "android", target_os = "linux"))]
pub mod jni {
    use jni::objects::JClass;
    use jni::sys::{jint, jlong, jstring};
    use jni::JNIEnv;

    use super::{zla_append_command, zla_create, zla_export_hex, zla_free, zla_import, zla_read};

    fn zla_import_hex(handle: jlong, hex: &str) -> jint {
        let mut out = Vec::with_capacity(hex.len() / 2);
        let b = hex.as_bytes();
        let mut i = 0;
        while i + 1 < b.len() {
            let hi = (b[i] as char).to_digit(16).unwrap_or(0) as u8;
            let lo = (b[i + 1] as char).to_digit(16).unwrap_or(0) as u8;
            out.push((hi << 4) | lo);
            i += 2;
        }
        if out.is_empty() { return zla_import(handle as *mut std::ffi::c_void, std::ptr::null(), 0) as jint; }
        zla_import(handle as *mut std::ffi::c_void, out.as_ptr(), out.len()) as jint
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_sh_zeron_android_loro_NativeDocBridge_createDoc(
        _env: JNIEnv, _cls: JClass) -> jlong {
        zla_create() as jlong
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_sh_zeron_android_loro_NativeDocBridge_readJson(
        mut env: JNIEnv, _cls: JClass, handle: jlong) -> jstring {
        let ptr = zla_read(handle as *mut std::ffi::c_void);
        if ptr.is_null() { return env.new_string("{}").unwrap().into_raw(); }
        let s = unsafe { std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned() };
        unsafe { std::ffi::CString::from_raw(ptr) };
        env.new_string(&s).unwrap().into_raw()
    }

    /// Bytes are passed as a hexadecimal string to avoid byte-array marshaling.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_sh_zeron_android_loro_NativeDocBridge_exportHex(
        mut env: JNIEnv, _cls: JClass, handle: jlong) -> jstring {
        let ptr = zla_export_hex(handle as *mut std::ffi::c_void);
        if ptr.is_null() { return env.new_string("").unwrap().into_raw(); }
        let s = unsafe { std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned() };
        unsafe { std::ffi::CString::from_raw(ptr) };
        env.new_string(&s).unwrap().into_raw()
    }

    /// Bytes are passed as a hexadecimal string to avoid byte-array marshaling.
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_sh_zeron_android_loro_NativeDocBridge_import(
         mut env: JNIEnv, _cls: JClass, handle: jlong, hex: jstring) -> jint {
        if hex.is_null() { return 2 }
        let jstr = unsafe { jni::objects::JString::from_raw(hex) };
        let s: String = env.get_string(&jstr).unwrap().into();
        zla_import_hex(handle, &s)
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_sh_zeron_android_loro_NativeDocBridge_appendCommand(
         mut env: JNIEnv, _cls: JClass, handle: jlong, cmd: jstring) -> jint {
        if cmd.is_null() { return 2 }
        let jstr = unsafe { jni::objects::JString::from_raw(cmd) };
        let s: String = env.get_string(&jstr).unwrap().into();
        let c = std::ffi::CString::new(s).unwrap_or_default();
        zla_append_command(handle as *mut std::ffi::c_void, c.as_ptr()) as jint
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_sh_zeron_android_loro_NativeDocBridge_free(
        _env: JNIEnv, _cls: JClass, handle: jlong) {
        zla_free(handle as *mut std::ffi::c_void);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_roundtrip() {
        let doc = create_doc();
        let bytes = doc_export_snapshot(doc.clone()).unwrap();
        let doc2 = doc_from_bytes(bytes.clone()).unwrap();
        let bytes2 = doc_export_snapshot(doc2).unwrap();
        assert!(!bytes.is_empty());
        assert!(!bytes2.is_empty());
    }

    #[test]
    fn malformed_rejected() {
        assert!(doc_from_bytes(vec![0, 1, 2, 3]).is_err());
        let doc = create_doc();
        assert!(doc_import_bytes(doc, vec![0xFF, 0xFF]).is_err());
    }

    #[test]
    fn cabi_roundtrip() {
        let h = zla_create();
        assert!(!h.is_null());
        // zla_read on empty doc returns valid JSON
        let s = zla_read(h);
        assert!(!s.is_null());
        let json = unsafe { std::ffi::CStr::from_ptr(s).to_string_lossy().into_owned() };
        assert_eq!("{}", json);
        unsafe { std::ffi::CString::from_raw(s) };
        zla_free(h);
    }

    #[test]
    fn cabi_append_command_roundtrip() {
        use std::ffi::CString;
        let h = zla_create();
        let cmd = CString::new(r#"{"id":"cmd1","kind":"run","payload":"hi","issuedBy":"android","issuedAt":0,"status":"pending","resolution":null}"#).unwrap();
        assert_eq!(0, zla_append_command(h, cmd.as_ptr()));
        let s = zla_read(h);
        let json = unsafe { std::ffi::CStr::from_ptr(s).to_string_lossy().into_owned() };
        unsafe { std::ffi::CString::from_raw(s) };
        assert!(json.contains("cmd1"), "deep value should contain the appended command: {json}");
        zla_free(h);
    }
}
