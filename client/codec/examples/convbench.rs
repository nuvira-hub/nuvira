use std::time::Instant;
fn main() {
    let (w, h) = (1920usize, 1080usize);
    let data = vec![100u8; w * h * 4];
    let img = nexodesk_codec::BgraImage { width: w as u32, height: h as u32, stride: w * 4, data: &data };
    let mut out = vec![0u8; w * h * 3 / 2];
    let (mut y, mut u, mut v) = (vec![], vec![], vec![]);
    for _ in 0..5 {
        nexodesk_codec::convert::bgra_to_nv12(&img, &mut out, w, h);
    }
    let t = Instant::now();
    for _ in 0..50 {
        nexodesk_codec::convert::bgra_to_nv12(&img, &mut out, w, h);
    }
    println!("nv12 {:.2} ms", t.elapsed().as_secs_f64() * 1000.0 / 50.0);
    let t = Instant::now();
    for _ in 0..50 {
        nexodesk_codec::convert::bgra_to_i420(&img, &mut y, &mut u, &mut v);
    }
    println!("i420 {:.2} ms", t.elapsed().as_secs_f64() * 1000.0 / 50.0);
}
