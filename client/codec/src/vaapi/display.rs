//! The VA display: the render node opened once for the process, its driver's
//! profile, entrypoint and rate control, and its video processing configuration.

use std::ffi::{CStr, c_int};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use tracing::{debug, info, warn};

use super::{
    ATTRIB_ENC_PACKED_HEADERS, ATTRIB_NOT_SUPPORTED, ATTRIB_RATE_CONTROL, ATTRIB_RT_FORMAT, ENTRY_ENC_SLICE,
    ENTRY_ENC_SLICE_LP, ENTRY_VIDEO_PROC, PACKED_PICTURE, PACKED_SEQUENCE, PROFILE_CONSTRAINED_BASELINE,
    PROFILE_HIGH, PROFILE_MAIN, PROFILE_NONE, RC_CBR, RC_CQP, RC_VBR, RT_FORMAT_YUV420, VA_INVALID_ID,
    VA_STATUS_SUCCESS, VAConfigAttrib, VADisplay, VAId, Va, va,
};
use super::vpp::{VPP_TOLERANCE, dmabuf_modifiers, vpp_check};

/// An initialised VA display on a render node, with the chosen profile.
pub(super) struct Display {
    pub(super) va: &'static Va,
    pub(super) dpy: VADisplay,
    _file: File,
    pub(super) profile: c_int,
    pub(super) entrypoint: c_int,
    pub(super) rate_control: u32,
    /// The driver takes SPS and PPS from us.
    pub(super) packed_headers: bool,
    /// Video processing configuration, if the driver has one.
    pub(super) vpp_config: VAId,
    /// Whether the GPU conversion gave the CPU's colours (checked once).
    vpp_verdict: OnceLock<bool>,
    pub(super) vendor: String,
    pub(super) path: String,
}

// SAFETY: a VA display may be used from any thread. The drivers serialise the
// calls on one display (Mesa holds a lock for each call, iHD one for each context),
// and every encoder has its own configuration, context and surfaces.
unsafe impl Send for Display {}
unsafe impl Sync for Display {}

/// The display of this process, opened at the first encoder and never closed
/// (`vaTerminate`): Mesa's D3D12 driver (WSL) leaves a destructor for the thread
/// that used it, which crashes when the thread ends if the driver was unloaded in
/// the meantime. Only a display that opened is kept: a later encoder tries again.
pub(super) fn shared_display() -> Result<&'static Display> {
    static DISPLAY: std::sync::Mutex<Option<&'static Display>> = std::sync::Mutex::new(None);
    let mut slot = DISPLAY.lock().expect("VA display");
    if let Some(display) = *slot {
        return Ok(display);
    }
    let display: &'static Display = Box::leak(Box::new(Display::open()?));
    *slot = Some(display);
    Ok(display)
}

/// The render nodes to try, `renderD128` to `renderD135` under `dev`: those of
/// Intel's and AMD's kernel drivers (`i915`, `xe`, `amdgpu`) first, the others
/// after them in their order. Nodes driven by `nouveau` are left out: Mesa's
/// nouveau VA driver encodes no H.264, and initialising it on a hybrid laptop
/// (MX150 next to an Intel UHD 620) put warnings in the kernel log and then made
/// the encoder test time out (2026-11-17). `NUVIRA_VAAPI_DEVICE` can still name
/// one. The kernel driver is read from `sys` (`renderDN/device/driver`).
fn render_nodes(dev: &std::path::Path, sys: &std::path::Path) -> Vec<String> {
    let mut nodes: Vec<(u8, String)> = Vec::new();
    for n in 128..136 {
        let name = format!("renderD{n}");
        let path = dev.join(&name);
        if !path.exists() {
            continue;
        }
        let driver = std::fs::read_link(sys.join(&name).join("device/driver"))
            .ok()
            .and_then(|link| link.file_name().map(|d| d.to_string_lossy().into_owned()));
        let rank = match driver.as_deref() {
            Some("nouveau") => {
                debug!("VA-API: {} left out (nouveau encodes no H.264)", path.display());
                continue;
            }
            Some("i915" | "xe" | "amdgpu") => 0,
            _ => 1,
        };
        nodes.push((rank, path.to_string_lossy().into_owned()));
    }
    nodes.sort_by_key(|(rank, _)| *rank);
    nodes.into_iter().map(|(_, path)| path).collect()
}

/// Render nodes whose driver initialised but was refused for encoding, with the
/// reason. Their display is not terminated (`vaTerminate` unloads the driver, and
/// Mesa's D3D12 driver, a node refused here in WSL, then crashes when the thread
/// that used it ends): remembering them bounds that to one driver instance per
/// node and process, instead of one more at every encoder. Only encoders skip
/// them: the conversion tests open such drivers on purpose.
static REFUSED: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

thread_local! {
    /// `vaInitialize` calls made by this thread (for the tests: per thread, so
    /// tests opening drivers in parallel do not change another test's count).
    static INITIALISED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn refused(path: &str) -> Option<String> {
    let refused = REFUSED.lock().unwrap_or_else(|e| e.into_inner());
    refused.iter().find(|(p, _)| p == path).map(|(_, why)| why.clone())
}

fn refuse(path: &str, why: &str) {
    let mut refused = REFUSED.lock().unwrap_or_else(|e| e.into_inner());
    if !refused.iter().any(|(p, _)| p == path) {
        refused.push((path.into(), why.into()));
    }
}

impl Display {
    pub(super) fn open() -> Result<Self> {
        Self::open_with(true)
    }

    /// `encoder`: false only for the tests of the conversion, on drivers that
    /// process video without encoding H.264 (Mesa's D3D12 driver on an Intel UHD).
    pub(super) fn open_with(encoder: bool) -> Result<Self> {
        let va = va()?;
        let paths: Vec<String> = match std::env::var("NUVIRA_VAAPI_DEVICE") {
            Ok(path) => vec![path],
            Err(_) => render_nodes(std::path::Path::new("/dev/dri"), std::path::Path::new("/sys/class/drm")),
        };
        Self::open_paths(va, paths, encoder)
    }

    fn open_paths(va: &'static Va, paths: Vec<String>, encoder: bool) -> Result<Self> {
        let mut errors = Vec::new();
        for path in paths {
            if !std::path::Path::new(&path).exists() {
                continue;
            }
            if encoder && let Some(why) = refused(&path) {
                errors.push(format!("{path}: {why} (not tried again)"));
                continue;
            }
            match Self::open_one(va, &path, encoder) {
                Ok(display) => return Ok(display),
                Err(e) => errors.push(format!("{path}: {e:#}")),
            }
        }
        if errors.is_empty() {
            bail!("no DRM render node");
        }
        bail!("{}", errors.join("; "))
    }

    fn open_one(va: &'static Va, path: &str, encoder: bool) -> Result<Self> {
        let file = File::options().read(true).write(true).open(path).context("not accessible")?;
        // SAFETY: plain libva calls. A display that initialised is never terminated
        // (see `shared_display`), also when its driver is refused here: that one
        // is remembered (`refuse`), so it is initialised at most once per process.
        unsafe {
            let dpy = (va.get_display_drm)(file.as_raw_fd());
            if dpy.is_null() {
                bail!("no VA display");
            }
            let (mut major, mut minor) = (0, 0);
            INITIALISED.set(INITIALISED.get() + 1);
            let status = (va.initialize)(dpy, &mut major, &mut minor);
            if status != VA_STATUS_SUCCESS {
                (va.terminate)(dpy);
                return Err(va.check(status, "vaInitialize").unwrap_err());
            }
            let vendor_ptr = (va.vendor)(dpy);
            let vendor = if vendor_ptr.is_null() {
                String::new()
            } else {
                CStr::from_ptr(vendor_ptr).to_string_lossy().into()
            };
            let mut display = Self {
                va,
                dpy,
                _file: file,
                profile: 0,
                entrypoint: 0,
                rate_control: 0,
                packed_headers: false,
                vpp_config: VA_INVALID_ID,
                vpp_verdict: OnceLock::new(),
                vendor,
                path: path.into(),
            };
            if encoder && let Err(e) = display.choose_profile() {
                refuse(path, &format!("{e:#}"));
                return Err(e);
            }
            display.vpp_config = display.find_vpp();
            Ok(display)
        }
    }

    /// The video processing configuration, if the driver offers one and
    /// `NUVIRA_VAAPI_VPP` is not `0`.
    fn find_vpp(&self) -> VAId {
        if std::env::var("NUVIRA_VAAPI_VPP").is_ok_and(|v| v == "0") {
            return VA_INVALID_ID;
        }
        let va = self.va;
        // SAFETY: buffers sized as libva asks.
        unsafe {
            let max = (va.max_entrypoints)(self.dpy).max(1) as usize;
            let mut list = vec![0 as c_int; max];
            let mut n = 0;
            if (va.query_entrypoints)(self.dpy, PROFILE_NONE, list.as_mut_ptr(), &mut n) != VA_STATUS_SUCCESS
                || !list[..n.max(0) as usize].contains(&ENTRY_VIDEO_PROC)
            {
                return VA_INVALID_ID;
            }
            let mut config = VA_INVALID_ID;
            let mut attrib = VAConfigAttrib { type_: ATTRIB_RT_FORMAT, value: RT_FORMAT_YUV420 };
            let status =
                (va.create_config)(self.dpy, PROFILE_NONE, ENTRY_VIDEO_PROC, &mut attrib, 1, &mut config);
            if status != VA_STATUS_SUCCESS {
                debug!("VA-API: no video processing configuration ({status:#x})");
                return VA_INVALID_ID;
            }
            config
        }
    }

    /// Converts a test picture on the GPU once and compares it with the CPU's
    /// conversion: a driver whose colours differ (another matrix or range) keeps
    /// the CPU conversion.
    pub(super) fn vpp_usable(&self) -> bool {
        if self.vpp_config == VA_INVALID_ID {
            return false;
        }
        *self.vpp_verdict.get_or_init(|| match vpp_check(self, 256, 64) {
            Ok(diff) if diff <= VPP_TOLERANCE => {
                info!(diff, "VA-API: colour conversion on the GPU");
                let modifiers = dmabuf_modifiers(self);
                info!(?modifiers, "VA-API: DMA-BUF layouts it imports");
                crate::set_dmabuf_import_modifiers(modifiers);
                true
            }
            Ok(diff) => {
                warn!(diff, "VA-API: the GPU conversion gives other colours: on the CPU");
                false
            }
            Err(e) => {
                warn!("VA-API: no colour conversion on the GPU ({e:#}): on the CPU");
                false
            }
        })
    }
}

impl Display {
    /// The first profile and entrypoint whose rate control follows a bitrate (CBR,
    /// else VBR); failing that, one with only constant QP, the bitrate then followed
    /// here. iHD on Intel Gen9 (Skylake to Comet Lake) gives only constant QP on the
    /// low-power entrypoint unless the HuC firmware is loaded (i915.enable_guc=2),
    /// which the kernel does not do there by default (Kaby Lake-R, 2026-11-12).
    fn choose_profile(&mut self) -> Result<()> {
        if self.find_profile(false) || self.find_profile(true) {
            return Ok(());
        }
        bail!("the driver does not encode H.264")
    }

    fn find_profile(&mut self, cqp: bool) -> bool {
        let va = self.va;
        // SAFETY: buffers sized as libva asks.
        unsafe {
            let max = (va.max_entrypoints)(self.dpy).max(1) as usize;
            for profile in [PROFILE_MAIN, PROFILE_HIGH, PROFILE_CONSTRAINED_BASELINE] {
                let mut list = vec![0 as c_int; max];
                let mut n = 0;
                if (va.query_entrypoints)(self.dpy, profile, list.as_mut_ptr(), &mut n) != VA_STATUS_SUCCESS {
                    continue;
                }
                let list = &list[..n.max(0) as usize];
                for entrypoint in [ENTRY_ENC_SLICE, ENTRY_ENC_SLICE_LP] {
                    if !list.contains(&entrypoint) {
                        continue;
                    }
                    let mut attribs = [
                        VAConfigAttrib { type_: ATTRIB_RT_FORMAT, value: 0 },
                        VAConfigAttrib { type_: ATTRIB_RATE_CONTROL, value: 0 },
                        VAConfigAttrib { type_: ATTRIB_ENC_PACKED_HEADERS, value: 0 },
                    ];
                    let status =
                        (va.get_config_attributes)(self.dpy, profile, entrypoint, attribs.as_mut_ptr(), 3);
                    if status != VA_STATUS_SUCCESS
                        || attribs[0].value == ATTRIB_NOT_SUPPORTED
                        || attribs[0].value & RT_FORMAT_YUV420 == 0
                    {
                        continue;
                    }
                    let rc = attribs[1].value;
                    let rate_control = if cqp {
                        if rc == ATTRIB_NOT_SUPPORTED || rc & RC_CQP == 0 {
                            continue;
                        }
                        RC_CQP
                    } else if rc == ATTRIB_NOT_SUPPORTED {
                        0
                    } else if rc & RC_CBR != 0 {
                        RC_CBR
                    } else if rc & RC_VBR != 0 {
                        RC_VBR
                    } else {
                        continue; // only constant QP: second pass
                    };
                    self.profile = profile;
                    self.entrypoint = entrypoint;
                    self.rate_control = rate_control;
                    let packed = attribs[2].value;
                    let both = PACKED_SEQUENCE | PACKED_PICTURE;
                    self.packed_headers = packed != ATTRIB_NOT_SUPPORTED && packed & both == both;
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::{Display, INITIALISED, refuse, refused, render_nodes};
    use crate::vaapi::va;

    /// A fake `/dev/dri` and `/sys/class/drm`: nouveau's node is left out, Intel's
    /// and AMD's come first, a node whose driver is unknown stays, in its order.
    #[test]
    fn render_node_order() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("nuvira-render-nodes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (dev, sys) = (root.join("dev"), root.join("sys"));
        std::fs::create_dir_all(&dev).unwrap();
        let node = |n: u32, driver: Option<&str>| {
            std::fs::write(dev.join(format!("renderD{n}")), b"").unwrap();
            if let Some(driver) = driver {
                let device = sys.join(format!("renderD{n}/device"));
                std::fs::create_dir_all(&device).unwrap();
                symlink(format!("../../../bus/pci/drivers/{driver}"), device.join("driver")).unwrap();
            }
        };
        let names = |paths: Vec<String>| -> Vec<String> {
            paths.iter().map(|p| p.rsplit('/').next().unwrap().to_string()).collect()
        };

        // PC-LENOVO-ALE: the MX150 (nouveau) before the UHD 620.
        node(128, Some("nouveau"));
        node(129, Some("i915"));
        assert_eq!(names(render_nodes(&dev, &sys)), ["renderD129"]);

        // Unknown or missing drivers after Intel's and AMD's, in their order; a
        // node only in sysfs is not tried.
        node(130, Some("virtio_gpu"));
        node(131, None);
        node(132, Some("amdgpu"));
        node(133, Some("xe"));
        let device = sys.join("renderD134/device");
        std::fs::create_dir_all(&device).unwrap();
        symlink("../../../bus/pci/drivers/i915", device.join("driver")).unwrap();
        assert_eq!(
            names(render_nodes(&dev, &sys)),
            ["renderD129", "renderD132", "renderD133", "renderD130", "renderD131"]
        );

        // No render node at all.
        assert!(render_nodes(&root.join("none"), &sys).is_empty());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A node refused for encoding is not initialised again (VID-5): its driver
    /// instance is never terminated, so a second `vaInitialize` would leak another.
    /// The real refusal (nouveau, D3D12) needs that hardware; here a node is
    /// marked refused by hand and must not reach `vaInitialize`.
    #[test]
    fn refused_node_not_opened_again() {
        let Ok(va) = va() else { return eprintln!("no libva: skipped") };
        let node = std::env::temp_dir().join(format!("nuvira-refused-node-{}", std::process::id()));
        std::fs::write(&node, b"").unwrap();
        let path = node.to_string_lossy().into_owned();
        refuse(&path, "the driver does not encode H.264");
        let before = INITIALISED.get();
        for _ in 0..20 {
            let e = Display::open_paths(va, vec![path.clone()], true).err().expect("refused");
            assert!(format!("{e:#}").contains("does not encode H.264 (not tried again)"), "{e:#}");
        }
        assert_eq!(INITIALISED.get(), before);
        // The first reason stays.
        assert_eq!(refused(&path).as_deref(), Some("the driver does not encode H.264"));
        refuse(&path, "another reason");
        assert_eq!(refused(&path).as_deref(), Some("the driver does not encode H.264"));
        std::fs::remove_file(&node).unwrap();
    }
}
