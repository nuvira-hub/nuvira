//! B-MICRO-05 hardware encode with the image in RAM:
//! `create_encoder(EncoderPlan::Hardware, settings, None)` (Media Foundation on
//! Windows, NVENC or VA-API on Linux pinned with `NUVIRA_HW_ENCODER`, VideoToolbox
//! on macOS), same harness and metrics as B-MICRO-04 plus `wait_ready` time.
//!
//! Every backend is registered on every machine (same case keys everywhere): on
//! another OS it is `not-runnable`; on the right OS without the hardware the
//! encoder probe at run time makes it `not-runnable` with the error.

use nuvira_codec::EncoderPlan;

use super::codec_sw::{Backend, register_backend};
use crate::fixtures::Scene;
use crate::registry::Registry;

const QUICK: &[Scene] = &[Scene::Video, Scene::Drag];

pub fn register(reg: &mut Registry) {
    let base = Backend {
        id: "B-MICRO-05",
        prefix: "",
        plan: EncoderPlan::Hardware,
        expect_name: None,
        quick_scenes: QUICK,
        runnable: super::always,
        env: &[],
        config: None,
    };
    register_backend(
        reg,
        Backend {
            prefix: "mf",
            expect_name: Some("mediafoundation"),
            runnable: super::only_windows,
            config: Some(&["GPU-INTEL-QSV", "GPU-NVENC", "GPU-AMD-AMF"]),
            ..base
        },
    );
    register_backend(
        reg,
        Backend {
            prefix: "nvenc",
            expect_name: Some("nvenc"),
            runnable: super::only_linux,
            env: &[("NUVIRA_HW_ENCODER", "nvenc")],
            config: Some(&["GPU-NVENC"]),
            ..base
        },
    );
    register_backend(
        reg,
        Backend {
            prefix: "vaapi",
            expect_name: Some("vaapi"),
            runnable: super::only_linux,
            env: &[("NUVIRA_HW_ENCODER", "vaapi")],
            config: Some(&["LINUX-VAAPI"]),
            ..base
        },
    );
    register_backend(
        reg,
        Backend {
            prefix: "vt",
            expect_name: Some("videotoolbox"),
            runnable: super::only_macos,
            config: Some(&["MAC-VIDEOTOOLBOX"]),
            ..base
        },
    );
}
