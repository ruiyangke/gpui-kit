use std::mem::ManuallyDrop;

use windows::{
    Win32::{
        Foundation::{HWND, PROPERTYKEY},
        System::{
            Com::StructuredStorage::{
                PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
            },
            Variant::VT_LPWSTR,
        },
        UI::Shell::{
            PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow},
            SHStrDupW,
        },
    },
    core::{GUID, HSTRING, Result},
};

// Copied from:
// https://github.com/microsoft/windows-rs/blob/0fc3c2e5a13d4316d242bdeb0a52af611eba8bd4/crates/libs/windows/src/Windows/Win32/Storage/EnhancedStorage/mod.rs
const PKEY_APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
    pid: 5,
};

/// Sets the window's AppUserModelID, the identity the taskbar groups it by,
/// or removes it with `None`. Windows keeps the value in window properties,
/// so a window that has one must remove it before it is destroyed.
pub(crate) fn set_window_app_id(hwnd: HWND, app_id: Option<&str>) -> Result<()> {
    let value = match app_id {
        Some(app_id) => string_value(app_id)?,
        None => PROPVARIANT::default(),
    };
    unsafe {
        let store: IPropertyStore = SHGetPropertyStoreForWindow(hwnd)?;
        store.SetValue(&PKEY_APP_USER_MODEL_ID, &value)?;
        store.Commit()
    }
}

/// A `VT_LPWSTR` value, the type the shell's AppUserModel properties take.
fn string_value(value: &str) -> Result<PROPVARIANT> {
    // Owned by the PROPVARIANT, which frees it with `PropVariantClear`.
    let text = unsafe { SHStrDupW(&HSTRING::from(value)) }?;
    Ok(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_LPWSTR,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 { pwszVal: text },
            }),
        },
    })
}
