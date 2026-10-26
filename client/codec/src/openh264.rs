//! H.264 software con OpenH264 (Cisco, licenza BSD).

use anyhow::{Context, Result};
use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, FrameType, RateControlMode, UsageType};
use openh264::formats::{YUVSlices, YUVSource};

use crate::{BgraImage, EncodedFrame, EncoderSettings, VideoDecoder, VideoEncoder, YuvFrame};

pub struct OpenH264Encoder {
    encoder: Encoder,
    bitrate: u32,
    /// Il bitrate corrente è già stato passato all'encoder (lo si fa dopo il
    /// primo frame: prima l'encoder non è inizializzato).
    bitrate_applied: bool,
    initialized: bool,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl OpenH264Encoder {
    pub fn new(settings: EncoderSettings) -> Result<Self> {
        // La modalità "contenuto schermo" comprime di più ma è ~5 volte più lenta
        // (98 contro 19 ms a 1080p su un i7-10710U): per il controllo remoto conta la latenza.
        let config = EncoderConfig::new()
            .usage_type(UsageType::CameraVideoRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(settings.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(settings.max_fps as f32))
            .skip_frames(true);
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .context("inizializzazione OpenH264")?;
        Ok(Self {
            encoder,
            bitrate: settings.bitrate_bps,
            bitrate_applied: true,
            initialized: false,
            y: Vec::new(),
            u: Vec::new(),
            v: Vec::new(),
        })
    }
}

impl VideoEncoder for OpenH264Encoder {
    fn name(&self) -> &'static str {
        "openh264"
    }

    fn set_bitrate(&mut self, bps: u32) {
        self.bitrate = bps;
        self.bitrate_applied = false;
    }

    fn encode(&mut self, image: &BgraImage, force_keyframe: bool) -> Result<Option<EncodedFrame>> {
        crate::convert::bgra_to_i420(image, &mut self.y, &mut self.u, &mut self.v);
        let (w, h) = crate::convert::even_size(image);
        let yuv = YUVSlices::new((&self.y, &self.u, &self.v), (w, h), (w, w / 2, w / 2));

        if force_keyframe {
            self.encoder.force_intra_frame();
        }
        // Il nuovo bitrate si passa solo a encoder già inizializzato (dopo il primo frame).
        if !self.bitrate_applied && self.initialized {
            let mut info = openh264_sys2::SBitrateInfo {
                iLayer: openh264_sys2::SPATIAL_LAYER_ALL,
                iBitrate: self.bitrate as i32,
            };
            // SAFETY: opzione documentata di OpenH264 con la struttura che si aspetta.
            unsafe {
                self.encoder
                    .raw_api()
                    .set_option(openh264_sys2::ENCODER_OPTION_BITRATE, (&raw mut info).cast());
            }
            self.bitrate_applied = true;
        }
        let bitstream = self.encoder.encode(&yuv).context("compressione OpenH264")?;
        self.initialized = true;
        let keyframe = match bitstream.frame_type() {
            FrameType::Skip | FrameType::Invalid => return Ok(None),
            FrameType::IDR | FrameType::I => true,
            _ => false,
        };
        Ok(Some(EncodedFrame { data: bitstream.to_vec(), keyframe }))
    }
}

pub struct OpenH264Decoder {
    decoder: Decoder,
    frame: YuvFrame,
}

impl OpenH264Decoder {
    pub fn new() -> Result<Self> {
        let decoder = Decoder::with_api_config(OpenH264API::from_source(), DecoderConfig::new())
            .context("inizializzazione decoder OpenH264")?;
        Ok(Self { decoder, frame: YuvFrame::default() })
    }
}

fn copy_plane(dst: &mut Vec<u8>, src: &[u8], stride: usize, width: usize, height: usize) {
    dst.clear();
    for y in 0..height {
        dst.extend_from_slice(&src[y * stride..][..width]);
    }
}

impl VideoDecoder for OpenH264Decoder {
    fn name(&self) -> &'static str {
        "openh264"
    }

    fn decode(&mut self, data: &[u8]) -> Result<Option<&YuvFrame>> {
        let Some(yuv) = self.decoder.decode(data).context("decompressione OpenH264")? else {
            return Ok(None);
        };
        let (w, h) = yuv.dimensions();
        let (ys, us, vs) = yuv.strides();
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let f = &mut self.frame;
        f.width = w as u32;
        f.height = h as u32;
        copy_plane(&mut f.y, yuv.y(), ys, w, h);
        copy_plane(&mut f.u, yuv.u(), us, cw, ch);
        copy_plane(&mut f.v, yuv.v(), vs, cw, ch);
        Ok(Some(&self.frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trip() {
        let (w, h) = (322u32, 180u32);
        let stride = w as usize * 4 + 16;
        let mut data = vec![0u8; stride * h as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let p = &mut data[y * stride + x * 4..][..4];
                p.copy_from_slice(&[(x % 256) as u8, (y % 256) as u8, 128, 255]);
            }
        }
        let image = BgraImage { width: w, height: h, stride, data: &data };

        let mut enc = OpenH264Encoder::new(EncoderSettings::default()).unwrap();
        let mut dec = OpenH264Decoder::new().unwrap();
        let first = enc.encode(&image, true).unwrap().expect("primo frame");
        assert!(first.keyframe);

        let frame = dec.decode(&first.data).unwrap().expect("immagine decompressa");
        assert_eq!((frame.width, frame.height), (w, h));
        assert_eq!(frame.y.len(), (w * h) as usize);
        assert_eq!(frame.u.len(), (w / 2 * h / 2) as usize);
    }
}
