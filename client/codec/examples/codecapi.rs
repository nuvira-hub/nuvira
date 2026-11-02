//! Which loss-recovery features do the installed H.264 hardware encoders offer?
//! `cargo run --release -p nuvira-codec --example codecapi`
//!
//! Prints, for every hardware encoder, whether it supports gradual intra refresh
//! and long-term reference frames (LTR), and the values it reports.

#[cfg(windows)]
fn main() -> windows::core::Result<()> {
    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
    use windows::core::{GUID, Interface, PWSTR};

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_LITE)?;
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut list,
            &mut count,
        )?;
        if list.is_null() {
            println!("no hardware encoder");
            return Ok(());
        }
        let items = std::slice::from_raw_parts_mut(list, count as usize);
        for activate in items.iter_mut().filter_map(|a| a.take()) {
            let mut name = PWSTR::null();
            let mut len = 0;
            let name = match activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len) {
                Ok(()) => {
                    let s = name.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(name.0 as *const _));
                    s
                }
                Err(_) => "hardware encoder".into(),
            };
            println!("== {name}");
            let transform: IMFTransform = match activate.ActivateObject() {
                Ok(t) => t,
                Err(e) => {
                    println!("   not activated: {e}");
                    continue;
                }
            };
            if let Ok(attrs) = transform.GetAttributes() {
                let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
            }
            let Ok(api) = transform.cast::<ICodecAPI>() else {
                println!("   no ICodecAPI");
                let _ = activate.ShutdownObject();
                continue;
            };
            let props: [(&str, GUID); 5] = [
                ("GradualIntraRefresh", CODECAPI_AVEncVideoGradualIntraRefresh),
                ("LTRBufferControl", CODECAPI_AVEncVideoLTRBufferControl),
                ("MarkLTRFrame", CODECAPI_AVEncVideoMarkLTRFrame),
                ("UseLTRFrame", CODECAPI_AVEncVideoUseLTRFrame),
                ("MaxNumRefFrame", CODECAPI_AVEncVideoMaxNumRefFrame),
            ];
            for (what, key) in props {
                let supported = api.IsSupported(&key).is_ok();
                let current = match api.GetValue(&key) {
                    Ok(value) => match u32::try_from(&value) {
                        Ok(v) => format!("{v:#x}"),
                        Err(_) => "(not a number)".into(),
                    },
                    Err(e) => format!("({})", e.code()),
                };
                println!("   {what:20} supported={supported:5} value={current}");
            }
            let _ = activate.ShutdownObject();
        }
        CoTaskMemFree(Some(list as *const _));
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {}
