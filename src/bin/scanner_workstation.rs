#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
#[cfg(target_arch = "wasm32")]
fn main() {}

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result {
    use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|arg| arg == "--j2534-worker") {
        if let Err(error) = obd_dashboard::scanner::driver::serve() {
            eprintln!("J2534 worker: {error}");
            std::process::exit(1);
        }
        return Ok(());
    }
    #[cfg(windows)]
    obd_dashboard::scanner::updates::initialize();
    #[cfg(all(windows, feature = "scanner-update-smoke"))]
    {
        use std::path::Path;
        let request = match args.get(1) {
            Some(flag) if flag == "--verify-local-update" => match args.get(2..5) {
                Some([feed, output, data]) => Some((Some(Path::new(feed)), output, data, 5)),
                _ => std::process::exit(2),
            },
            Some(flag) if flag == "--verify-github-update" => match args.get(2..4) {
                Some([output, data]) => Some((None, output, data, 4)),
                _ => std::process::exit(2),
            },
            _ => None,
        };
        if let Some((feed, output, data, apply_index)) = request {
            let apply = feed.is_some() && args.get(apply_index).is_some_and(|arg| arg == "--apply");
            if let Err(err) = obd_dashboard::scanner::updates::verify_update(
                feed,
                Path::new(output),
                Path::new(data),
                apply,
            ) {
                let _ = std::fs::write(output, err.to_string());
                std::process::exit(1);
            }
            return Ok(());
        }
    }
    let root = args
        .windows(2)
        .find(|a| a[0] == "--data-dir")
        .map(|a| std::path::PathBuf::from(&a[1]))
        .unwrap_or_else(obd_dashboard::scanner::model::data_directory);
    let root = std::path::absolute(&root).unwrap_or(root);
    let _ = std::fs::create_dir_all(&root);
    let log_path = root.join("scanner.log");
    if std::fs::metadata(&log_path).is_ok_and(|m| m.len() > 8 * 1024 * 1024) {
        let _ = std::fs::rename(&log_path, root.join("scanner.previous.log"));
    }
    let writer = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(file) => tracing_subscriber::fmt::writer::BoxMakeWriter::new(file),
        Err(_) => tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::io::stderr),
    };
    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        ))
        .with(fmt::layer().with_ansi(false).with_writer(writer))
        .try_init();
    let demo = args.iter().any(|a| a == "--demo");
    tracing::info!(
        version = obd_dashboard::scanner::VERSION,
        storage = %root.display(),
        "Scanner Workbench starting"
    );
    let mut app =
        obd_dashboard::scanner::Workbench::with_native_driver(root, std::env::current_exe().ok());
    app.show_startup_splash();
    if demo {
        app.start_demo();
    }
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Scanner Workbench")
        .with_inner_size([1280.0, 820.0])
        .with_min_inner_size([960.0, 640.0]);
    if let Some(icon) = obd_dashboard::scanner::workbench_icon() {
        viewport = viewport.with_icon(icon);
    }
    let result = eframe::run_native(
        "Scanner Workbench",
        eframe::NativeOptions {
            viewport,
            ..Default::default()
        },
        Box::new(move |_| Ok(Box::new(app))),
    );
    tracing::info!(
        "Scanner Workbench closed: {}",
        if result.is_ok() {
            "clean exit"
        } else {
            "window error"
        }
    );
    result
}
