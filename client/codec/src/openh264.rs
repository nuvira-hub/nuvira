//! H.264 software con OpenH264 (Cisco, licenza BSD).

use anyhow::{Context, Result};
use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, RateControlMode, UsageType,
};
use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};

use crate::{BgraImage, EncodedFrame, EncoderSettings, VideoDecoder, VideoEncoder, YuvFrame};

pub struct OpenH264Encoder {
    encoder: Encoder,
    /// Copia compatta dell'immagine quando lo stride o le dimensioni dispari lo richiedono.
    packed: Vec<u8>,
    yuv: Option<YUVBuffer>,
}

impl OpenH264Encoder {
    pub fn new(settings: EncoderSettings) -> Result<Self> {
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(settings.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(settings.max_fps as f32))
            .skip_frames(true);
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .context("inizializzazione OpenH264")?;
        Ok(Self { encoder, packed: Vec::new(), yuv: None })
    }
}

impl VideoEncoder for OpenH264Encoder {
    fn name(&self) -> &'static str {
        "openh264"
    }

    fn encode(&mut self, image: &BgraImage, force_keyframe: bool) -> Result<Option<EncodedFrame>> {
        // I420 richiede dimensioni pari: si scarta al massimo una riga/colonna.
        let w = (image.width & !1) as usize;
        let h = (image.height & !1) as usize;
        let row = w * 4;
        let bgra = if image.stride == row && image.height as usize == h {
            &image.data[..row * h]
        } else {
            self.packed.clear();
            for y in 0..h {
                self.packed.extend_from_slice(&image.data[y * image.stride..][..row]);
            }
            &self.packed[..]
        };

        let source = BgraSliceU8::new(bgra, (w, h));
        let yuv = match &mut self.yuv {
            Some(yuv) if yuv.dimensions() == (w, h) => {
                yuv.read_bgra8(source);
                yuv
            }
            slot => slot.insert(YUVBuffer::from_bgra8_source(source)),
        };

        if force_keyframe {
            self.encoder.force_intra_frame();
        }
        let bitstream = self.encoder.encode(&*yuv).context("compressione OpenH264")?;
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
