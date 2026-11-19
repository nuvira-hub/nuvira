//! Software H.264 with OpenH264 (Cisco, BSD licence).

use anyhow::{Context, Result, bail};
use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, FrameType, RateControlMode, UsageType};
use openh264::formats::{YUVSlices, YUVSource};

use crate::{BgraImage, EncodedFrame, EncoderSettings, SizeLimit, VideoDecoder, VideoEncoder, YuvFrame};

pub struct OpenH264Encoder {
    encoder: Encoder,
    bitrate: u32,
    /// The current bitrate has already been given to the encoder (done after the
    /// first frame: before that the encoder is not initialised).
    bitrate_applied: bool,
    initialized: bool,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl OpenH264Encoder {
    pub fn new(settings: EncoderSettings) -> Result<Self> {
        // "Screen content" mode compresses more but is ~5 times slower
        // (98 against 19 ms at 1080p on an i7-10710U): for remote control latency matters.
        let config = EncoderConfig::new()
            .usage_type(UsageType::CameraVideoRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(settings.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(settings.max_fps as f32))
            .skip_frames(true);
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .context("OpenH264 initialisation")?;
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

    fn max_size(&self) -> SizeLimit {
        SizeLimit::OPENH264
    }

    fn encode(&mut self, image: &BgraImage, force_keyframe: bool) -> Result<Option<EncodedFrame>> {
        // The crate refuses it anyway: without spending the conversion first.
        let (w, h) = crate::convert::even_size(image);
        if !SizeLimit::OPENH264.fits(w as u32, h as u32) {
            bail!("OpenH264 takes at most 3840x2160 (or 2160x3840), not {w}x{h}");
        }
        crate::convert::bgra_to_i420(image, &mut self.y, &mut self.u, &mut self.v);
        let yuv = YUVSlices::new((&self.y, &self.u, &self.v), (w, h), (w, w / 2, w / 2));

        if force_keyframe {
            self.encoder.force_intra_frame();
        }
        // The new bitrate is given only to an already initialised encoder (after the first frame).
        if !self.bitrate_applied && self.initialized {
            let mut info = openh264_sys2::SBitrateInfo {
                iLayer: openh264_sys2::SPATIAL_LAYER_ALL,
                iBitrate: self.bitrate as i32,
            };
            // SAFETY: documented OpenH264 option with the structure it expects.
            unsafe {
                self.encoder
                    .raw_api()
                    .set_option(openh264_sys2::ENCODER_OPTION_BITRATE, (&raw mut info).cast());
            }
            self.bitrate_applied = true;
        }
        let bitstream = self.encoder.encode(&yuv).context("OpenH264 encoding")?;
        self.initialized = true;
        let keyframe = match bitstream.frame_type() {
            FrameType::Skip | FrameType::Invalid => return Ok(None),
            FrameType::IDR | FrameType::I => true,
            _ => false,
        };
        Ok(Some(EncodedFrame { data: bitstream.to_vec(), keyframe, ..Default::default() }))
    }
}

pub struct OpenH264Decoder {
    decoder: Decoder,
    frame: YuvFrame,
}

impl OpenH264Decoder {
    pub fn new() -> Result<Self> {
        let decoder = Decoder::with_api_config(OpenH264API::from_source(), DecoderConfig::new())
            .context("OpenH264 decoder initialisation")?;
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
        let Some(yuv) = self.decoder.decode(data).context("OpenH264 decoding")? else {
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
        let first = enc.encode(&image, true).unwrap().expect("first frame");
        assert!(first.keyframe);

        let frame = dec.decode(&first.data).unwrap().expect("decoded image");
        assert_eq!((frame.width, frame.height), (w, h));
        assert_eq!(frame.y.len(), (w * h) as usize);
        assert_eq!(frame.u.len(), (w / 2 * h / 2) as usize);
    }

    /// The `openh264` crate's own size check (0.9.9 `encoder.rs` `reinit`), which
    /// [`SizeLimit::OPENH264`] follows: an update that lifts or changes it fails here.
    #[test]
    fn refuses_beyond_3840() {
        let encode = |w: usize, h: usize| {
            let mut enc =
                Encoder::with_api_config(OpenH264API::from_source(), EncoderConfig::new()).expect("encoder");
            let yuv = openh264::formats::YUVBuffer::new(w, h);
            enc.encode(&yuv).map(|_| ())
        };
        assert!(encode(3840, 2160).is_ok());
        assert!(encode(2160, 3840).is_ok(), "upright");
        assert!(encode(3842, 2160).is_err());
        assert!(encode(3840, 2162).is_err());
        assert!(encode(4096, 2304).is_err());
        assert!(encode(2162, 3840).is_err());
        for (w, h, fits) in [(3840, 2160, true), (2160, 3840, true), (3842, 2160, false), (3840, 2162, false)]
        {
            assert_eq!(SizeLimit::OPENH264.fits(w, h), fits, "{w}x{h}");
        }

        // The encoder refuses it before converting the picture.
        let data = vec![0u8; 4096 * 4 * 2304];
        let image = BgraImage { width: 4096, height: 2304, stride: 4096 * 4, data: &data };
        let mut ours = OpenH264Encoder::new(EncoderSettings::default()).unwrap();
        let Err(error) = ours.encode(&image, true) else { panic!("4096x2304 accepted") };
        let error = error.to_string();
        assert!(error.contains("3840x2160"), "{error}");
        assert!(ours.y.is_empty(), "converted anyway");
    }
}
