//! A captured picture left in the GPU's memory (DMA-BUF), as the compositor shares
//! it through PipeWire: a VA-API encoder imports it and converts it on the GPU
//! with no copy through RAM; any other encoder gets it read into RAM ([`DmaBuf::read`],
//! linear buffers only, which is all the capture asks for).

use std::os::fd::{AsRawFd, OwnedFd};

use anyhow::{Result, bail, ensure};

/// `DRM_FORMAT_MOD_LINEAR`: rows one after the other, readable by the CPU.
pub const MODIFIER_LINEAR: u64 = 0;

/// DRM fourcc codes (drm_fourcc.h) of the 32-bit formats the capture takes.
pub const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
pub const DRM_FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
pub const DRM_FORMAT_XBGR8888: u32 = u32::from_le_bytes(*b"XB24");
pub const DRM_FORMAT_ABGR8888: u32 = u32::from_le_bytes(*b"AB24");

pub struct DmaBuf {
    /// A copy of the compositor's file descriptor, ours to close.
    pub fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    /// DRM fourcc: XRGB8888 is the bytes B, G, R, X (PipeWire's BGRx).
    pub drm_format: u32,
    pub modifier: u64,
    pub offset: u32,
    pub stride: u32,
    /// Size of the whole buffer object.
    pub size: u32,
}

/// `DMA_BUF_IOCTL_SYNC` (linux/dma-buf.h): `_IOW('b', 0, struct dma_buf_sync)`.
const DMA_BUF_IOCTL_SYNC: libc::c_ulong = 0x4008_6200;
const DMA_BUF_SYNC_READ: u64 = 1;
const DMA_BUF_SYNC_END: u64 = 4;

impl DmaBuf {
    /// The picture into RAM as BGRA rows of `width * 4` bytes (R and B swapped for
    /// the RGB formats). Only linear buffers can be read this way.
    pub fn read(&self, out: &mut Vec<u8>) -> Result<()> {
        ensure!(self.modifier == MODIFIER_LINEAR, "DMA-BUF with modifier {:#x}: not readable by the CPU", self.modifier);
        let (w, h, stride) = (self.width as usize, self.height as usize, self.stride as usize);
        let end = self.offset as usize + stride * h.saturating_sub(1) + w * 4;
        let len = end.max(self.size as usize);
        ensure!(stride >= w * 4 && end <= len, "DMA-BUF smaller than its picture");
        let fd = self.fd.as_raw_fd();
        // SAFETY: a read-only shared mapping of `len` bytes of our own descriptor,
        // unmapped below; the sync ioctls bracket the CPU's reads as the kernel
        // asks for buffers a GPU writes.
        unsafe {
            let ptr = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, fd, 0);
            if ptr == libc::MAP_FAILED {
                bail!("mmap of the DMA-BUF: {}", std::io::Error::last_os_error());
            }
            let start = DMA_BUF_SYNC_READ;
            libc::ioctl(fd, DMA_BUF_IOCTL_SYNC, &start);
            let src = std::slice::from_raw_parts(ptr.cast::<u8>(), len);
            out.clear();
            out.reserve(w * h * 4);
            for row in 0..h {
                let at = self.offset as usize + row * stride;
                out.extend_from_slice(&src[at..at + w * 4]);
            }
            let finish = DMA_BUF_SYNC_READ | DMA_BUF_SYNC_END;
            libc::ioctl(fd, DMA_BUF_IOCTL_SYNC, &finish);
            libc::munmap(ptr, len);
        }
        if matches!(self.drm_format, DRM_FORMAT_XBGR8888 | DRM_FORMAT_ABGR8888) {
            for px in out.as_chunks_mut::<4>().0 {
                px.swap(0, 2);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A memfd stands in for the DMA-BUF (the sync ioctl fails on it, harmlessly).
    #[test]
    fn reads_rows_and_swaps_rgb() {
        let (w, h, stride, offset) = (3u32, 2u32, 16u32, 8u32);
        let mut bytes = vec![0u8; (offset + stride * h) as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let at = offset as usize + y * stride as usize + x * 4;
                bytes[at..at + 4].copy_from_slice(&[x as u8, y as u8, 200, 255]);
            }
        }
        // SAFETY: a new anonymous file, written and handed to OwnedFd.
        let fd = unsafe {
            let fd = libc::memfd_create(c"dmabuf-test".as_ptr(), 0);
            assert!(fd >= 0);
            assert_eq!(libc::write(fd, bytes.as_ptr().cast(), bytes.len()), bytes.len() as isize);
            <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd)
        };
        let mut buf = DmaBuf {
            fd,
            width: w,
            height: h,
            drm_format: DRM_FORMAT_XRGB8888,
            modifier: MODIFIER_LINEAR,
            offset,
            stride,
            size: bytes.len() as u32,
        };
        let mut out = Vec::new();
        buf.read(&mut out).unwrap();
        assert_eq!(out.len(), 3 * 2 * 4);
        assert_eq!(&out[4..8], &[1, 0, 200, 255]);
        assert_eq!(&out[16..20], &[1, 1, 200, 255]);
        buf.drm_format = DRM_FORMAT_XBGR8888;
        buf.read(&mut out).unwrap();
        assert_eq!(&out[4..8], &[200, 0, 1, 255]);
        buf.modifier = 0x0100_0000_0000_0002;
        assert!(buf.read(&mut out).is_err());
    }
}
