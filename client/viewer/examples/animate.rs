//! Full-screen window with a continuous animation, to measure capture of the
//! real screen with content that changes at every frame.
//!
//! `cargo run --release -p nexodesk-viewer --example animate -- [seconds]`

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Fullscreen, Window, WindowId};

struct App {
    until: Instant,
    started: Instant,
    window: Option<Arc<Window>>,
    surface: Option<softbuffer::Surface<Arc<Window>, Arc<Window>>>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("NexoDesk — test animation")
            .with_fullscreen(Some(Fullscreen::Borderless(None)));
        let window = Arc::new(el.create_window(attrs).expect("window"));
        let context = softbuffer::Context::new(window.clone()).expect("context");
        self.surface = Some(softbuffer::Surface::new(&context, window.clone()).expect("surface"));
        window.request_redraw();
        self.window = Some(window);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::RedrawRequested => {
                if Instant::now() > self.until {
                    el.exit();
                    return;
                }
                let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else { return };
                let size = window.inner_size();
                let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
                    return;
                };
                surface.resize(w, h).expect("resize");
                let mut buf = surface.buffer_mut().expect("buffer");
                let (w, h) = (size.width as usize, size.height as usize);
                // Scrolling striped background + vertical bar crossing the screen.
                let t = (self.started.elapsed().as_millis() / 16) as usize;
                let bar = (t * 12) % w;
                for y in 0..h {
                    let row = &mut buf[y * w..(y + 1) * w];
                    let base = if ((y + t) / 24).is_multiple_of(2) { 0x30_40_60 } else { 0x40_50_70 };
                    row.fill(base);
                    let end = (bar + 40).min(w);
                    row[bar..end].fill(0xff_ff_ff);
                }
                buf.present().expect("present");
                window.request_redraw();
            }
            _ => {}
        }
    }
}

fn main() {
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(30);
    let event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Poll);
    let now = Instant::now();
    let mut app = App { until: now + Duration::from_secs(secs), started: now, window: None, surface: None };
    event_loop.run_app(&mut app).expect("run");
}
