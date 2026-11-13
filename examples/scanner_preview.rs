//! Render-only UI inspection tool. It has no adapter and sends no OBD traffic.
#[cfg(debug_assertions)]
fn main() -> eframe::Result {
    struct Preview {
        panel: obd_dashboard::scanner::Workbench,
        frames: usize,
        path: String,
    }
    impl eframe::App for Preview {
        fn raw_input_hook(&mut self, ctx: &egui::Context, input: &mut egui::RawInput) {
            self.panel.raw_input_hook(ctx, input);
        }

        fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
            self.panel.update(ctx, frame);
            if self.frames == 3 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            }
            let capture = ctx.input(|i| {
                i.events.iter().find_map(|event| {
                    if let egui::Event::Screenshot { image, .. } = event {
                        Some(image.clone())
                    } else {
                        None
                    }
                })
            });
            if let Some(image) = capture {
                save_png(&self.path, &image).unwrap();
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            self.frames += 1;
            ctx.request_repaint();
        }
    }
    let args = std::env::args().collect::<Vec<_>>();
    let mut panel =
        obd_dashboard::scanner::Workbench::new(std::env::temp_dir().join("scanner-preview"));
    use obd_dashboard::scanner::Page;
    panel.prepare_preview(match args.get(1).map(String::as_str).unwrap_or("home") {
        "live" => Page::Live,
        "codes" => Page::Codes,
        "vehicle" => Page::VehicleInfo,
        "garage" => Page::Garage,
        "systems" => Page::Systems,
        "recordings" => Page::Recordings,
        "scan-history" => Page::ScanHistory,
        "reports" => Page::Reports,
        "emissions" => Page::Emissions,
        "freeze-frame" => Page::FreezeFrame,
        "monitors" => Page::MonitorTests,
        "connection" => Page::Connection,
        _ => Page::Home,
    });
    eframe::run_native(
        "UI preview - no vehicle",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 820.0]),
            ..Default::default()
        },
        Box::new(move |_| {
            Ok(Box::new(Preview {
                panel,
                frames: 0,
                path: args.get(2).cloned().unwrap_or_else(|| "preview.png".into()),
            }))
        }),
    )
}
#[cfg(not(debug_assertions))]
fn main() {}

#[cfg(debug_assertions)]
fn save_png(path: &str, image: &egui::ColorImage) -> std::io::Result<()> {
    fn crc(bytes: &[u8]) -> u32 {
        let mut c = !0u32;
        for b in bytes {
            c ^= u32::from(*b);
            for _ in 0..8 {
                c = (c >> 1) ^ if c & 1 != 0 { 0xedb88320 } else { 0 };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc(&out[start..]).to_be_bytes());
    }
    let mut raw = Vec::new();
    for row in image.pixels.chunks(image.size[0]) {
        raw.push(0);
        for color in row {
            raw.extend_from_slice(&color.to_array());
        }
    }
    let mut z = vec![0x78, 0x01];
    let mut position = 0;
    while position < raw.len() {
        let n = (raw.len() - position).min(65535);
        z.push(if position + n == raw.len() { 1 } else { 0 });
        z.extend_from_slice(&(n as u16).to_le_bytes());
        z.extend_from_slice(&(!(n as u16)).to_le_bytes());
        z.extend_from_slice(&raw[position..position + n]);
        position += n;
    }
    let (mut a, mut b) = (1u32, 0u32);
    for x in &raw {
        a = (a + u32::from(*x)) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend_from_slice(&(image.size[0] as u32).to_be_bytes());
    header.extend_from_slice(&(image.size[1] as u32).to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &header);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    std::fs::write(path, out)
}
