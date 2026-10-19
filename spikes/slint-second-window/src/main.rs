// SPDX-License-Identifier: GPL-3.0-or-later
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::wgpu_29::wgpu;
use slint::winit_030::{SlintEvent, WinitWindowAccessor};
use winit::platform::wayland::EventLoopBuilderExtWayland;
use winit::platform::wayland::WindowExtWayland;
use winit::platform::x11::EventLoopBuilderExtX11;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Manual,
    Automatic,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    Display,
    EarlySecond,
    AfterMainClose,
    CloseCycle,
    Interactive,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextureKind {
    Shared,
    Separate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DisplayBackend {
    Wayland,
    X11,
}

struct Config {
    mode: Mode,
    scenario: Scenario,
    texture: TextureKind,
    size: u32,
    output: PathBuf,
    display_backend: DisplayBackend,
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> String {
    args.next()
        .unwrap_or_else(|| panic!("{flag} needs a value"))
}

fn parse_args() -> Config {
    let mut mode = None;
    let mut scenario = None;
    let mut texture = None;
    let mut size = None;
    let mut output = None;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--mode" => {
                mode = Some(match value(&mut args, "--mode").as_str() {
                    "manual" => Mode::Manual,
                    "automatic" => Mode::Automatic,
                    other => panic!("unknown --mode {other}"),
                })
            }
            "--scenario" => {
                scenario = Some(match value(&mut args, "--scenario").as_str() {
                    "display" => Scenario::Display,
                    "early-second" => Scenario::EarlySecond,
                    "after-main-close" => Scenario::AfterMainClose,
                    "close-cycle" => Scenario::CloseCycle,
                    "interactive" => Scenario::Interactive,
                    other => panic!("unknown --scenario {other}"),
                })
            }
            "--texture" => {
                texture = Some(match value(&mut args, "--texture").as_str() {
                    "shared" => TextureKind::Shared,
                    "separate" => TextureKind::Separate,
                    other => panic!("unknown --texture {other}"),
                })
            }
            "--size" => {
                let parsed = value(&mut args, "--size")
                    .parse::<u32>()
                    .expect("--size needs an integer");
                assert!(matches!(parsed, 64 | 96), "--size must be 64 or 96");
                size = Some(parsed);
            }
            "--output" => output = Some(PathBuf::from(value(&mut args, "--output"))),
            "--help" => {
                println!(
                    "--mode manual|automatic --scenario display|early-second|after-main-close|close-cycle|interactive --texture shared|separate --size 64|96 --output <directory>"
                );
                std::process::exit(0);
            }
            other => panic!("unknown argument {other}"),
        }
    }
    Config {
        mode: mode.expect("--mode is required"),
        scenario: scenario.expect("--scenario is required"),
        texture: texture.expect("--texture is required"),
        size: size.expect("--size is required"),
        output: output.expect("--output is required"),
        display_backend: match std::env::var("FF_PROBE_DISPLAY_BACKEND").as_deref() {
            Ok("wayland") => DisplayBackend::Wayland,
            Ok("x11") => DisplayBackend::X11,
            Ok(other) => panic!("unknown FF_PROBE_DISPLAY_BACKEND {other}"),
            Err(_) => panic!("FF_PROBE_DISPLAY_BACKEND must be wayland or x11"),
        },
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(result) => return result,
            std::task::Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn manual_device() -> (wgpu::Instance, wgpu::Adapter, wgpu::Device, wgpu::Queue) {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::from_env().unwrap_or_default(),
        flags: wgpu::InstanceFlags::from_build_config().with_env(),
        backend_options: wgpu::BackendOptions::from_env_or_default(),
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        display: None,
    });
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        .expect("no wgpu adapter");
    let limits = focusforge_gpu::limits::device_limits(&adapter.limits());
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("second-window-device"),
        required_features: focusforge_gpu::limits::REQUIRED_FEATURES,
        required_limits: limits,
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        memory_hints: wgpu::MemoryHints::default(),
        trace: wgpu::Trace::default(),
    }))
    .expect("wgpu device creation failed");
    (instance, adapter, device, queue)
}

fn automatic_settings() -> slint::wgpu_29::WGPUSettings {
    let mut settings = slint::wgpu_29::WGPUSettings::default();
    settings.device_required_features = focusforge_gpu::limits::REQUIRED_FEATURES;
    settings.device_required_limits = focusforge_gpu::limits::required_limits();
    settings
}

fn event_loop_builder(display_backend: DisplayBackend) -> slint::winit_030::EventLoopBuilder {
    let mut builder = winit::event_loop::EventLoop::<SlintEvent>::with_user_event();
    match display_backend {
        DisplayBackend::Wayland => {
            builder.with_wayland();
        }
        DisplayBackend::X11 => {
            builder.with_x11();
        }
    }
    builder
}

fn choose_backend(
    config: &Config,
) -> (Option<wgpu::Instance>, Option<(wgpu::Device, wgpu::Queue)>) {
    let selector = slint::BackendSelector::new()
        .backend_name("winit".into())
        .with_winit_event_loop_builder(event_loop_builder(config.display_backend));
    match config.mode {
        Mode::Manual => {
            let (instance, adapter, device, queue) = manual_device();
            selector
                .require_wgpu_29(slint::wgpu_29::WGPUConfiguration::Manual {
                    instance: instance.clone(),
                    adapter,
                    device: device.clone(),
                    queue: queue.clone(),
                })
                .select()
                .expect("backend selection failed");
            (Some(instance), Some((device, queue)))
        }
        Mode::Automatic => {
            selector
                .require_wgpu_29(slint::wgpu_29::WGPUConfiguration::Automatic(
                    automatic_settings(),
                ))
                .select()
                .expect("backend selection failed");
            (None, None)
        }
    }
}

fn pattern_bytes(size: u32, variant: u8) -> Vec<u8> {
    let colors = match variant {
        0 => [
            [240, 40, 40, 255],
            [40, 220, 50, 255],
            [40, 90, 240, 255],
            [240, 220, 40, 255],
        ],
        _ => [
            [40, 220, 50, 255],
            [240, 40, 40, 255],
            [240, 220, 40, 255],
            [40, 90, 240, 255],
        ],
    };
    let mut bytes = vec![0; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let index = ((y * size + x) * 4) as usize;
            let quadrant = (usize::from(y >= size / 2) * 2) + usize::from(x >= size / 2);
            bytes[index..index + 4].copy_from_slice(&colors[quadrant]);
        }
    }
    bytes
}

fn create_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    size: u32,
    variant: u8,
) -> wgpu::Texture {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("second-window-pattern"),
        size: wgpu::Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &pattern_bytes(size, variant),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(size * 4),
            rows_per_image: Some(size),
        },
        wgpu::Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 1,
        },
    );
    texture
}

slint::slint! {
    export component ProbeWindow inherits Window {
        width: 320px; height: 256px; no-frame: true; title: "Second window texture probe";
        in-out property <image> texture; in-out property <bool> main-window: false;
        callback drag-requested(); callback close-requested(); callback reopen-requested();
        Rectangle { background: #20242cff; }
        Image { y: 32px; width: parent.width; height: parent.height - 32px; source: root.texture; }
        Rectangle {
            x: 0px; y: 0px; width: parent.width; height: 32px; background: #39445aff;
            Text { x: 12px; width: parent.width - 32px; height: parent.height; vertical-alignment: center; text: "Texture probe"; color: #f4f7ffff; }
            TouchArea { width: parent.width - 32px; height: parent.height; changed pressed => { if (self.pressed) { root.drag-requested(); } } }
            Rectangle { x: parent.width - 32px; width: 32px; height: parent.height; background: #b9434dff;
                Text { width: parent.width; height: parent.height; horizontal-alignment: center; vertical-alignment: center; text: "×"; color: #ffffffff; }
                TouchArea { width: parent.width; height: parent.height; clicked => { root.close-requested(); } }
            }
        }
        Rectangle {
            visible: root.main-window; x: 8px; y: parent.height - 30px; width: 88px; height: 24px;
            background: #39445aff;
            Text { width: parent.width; height: parent.height; horizontal-alignment: center; vertical-alignment: center; text: "Reopen"; color: #f4f7ffff; }
            TouchArea { width: parent.width; height: parent.height; clicked => { root.reopen-requested(); } }
        }
    }
}

fn snapshot_samples(buffer: &slint::SharedPixelBuffer<slint::Rgba8Pixel>) -> [[u8; 4]; 4] {
    let width = buffer.width();
    let height = buffer.height();
    let ys = [height / 3, height * 2 / 3];
    let xs = [width / 4, width * 3 / 4];
    let pixels = buffer.as_bytes();
    let mut samples = [[0; 4]; 4];
    for (row, y) in ys.into_iter().enumerate() {
        for (column, x) in xs.into_iter().enumerate() {
            let start = ((y * width + x) * 4) as usize;
            samples[row * 2 + column].copy_from_slice(&pixels[start..start + 4]);
        }
    }
    samples
}

fn pattern_matches(samples: [[u8; 4]; 4]) -> bool {
    const FIRST: [[u8; 4]; 4] = [
        [240, 40, 40, 255],
        [40, 220, 50, 255],
        [40, 90, 240, 255],
        [240, 220, 40, 255],
    ];
    const SECOND: [[u8; 4]; 4] = [
        [40, 220, 50, 255],
        [240, 40, 40, 255],
        [240, 220, 40, 255],
        [40, 90, 240, 255],
    ];
    samples == FIRST || samples == SECOND
}

fn write_png(
    buffer: &slint::SharedPixelBuffer<slint::Rgba8Pixel>,
    path: &Path,
) -> Result<(), image::ImageError> {
    image::RgbaImage::from_raw(buffer.width(), buffer.height(), buffer.as_bytes().to_vec())
        .expect("snapshot dimensions match bytes")
        .save(path)
}

fn capture(name: &str, window: &ProbeWindow, output: &Path) -> bool {
    match window.window().take_snapshot() {
        Ok(buffer) => {
            let samples = snapshot_samples(&buffer);
            let matches = pattern_matches(samples);
            println!(
                "snapshot window={name} size={}x{} samples={samples:?} pattern={matches}",
                buffer.width(),
                buffer.height()
            );
            let write_ok =
                if let Err(error) = write_png(&buffer, &output.join(format!("{name}.png"))) {
                    println!("snapshot-write window={name} error={error}");
                    false
                } else {
                    true
                };
            matches && write_ok
        }
        Err(error) => {
            println!("snapshot window={name} error={error}");
            false
        }
    }
}

fn complete_capture(
    main: slint::Weak<ProbeWindow>,
    second: slint::Weak<ProbeWindow>,
    output: PathBuf,
    snapshots_ok: Rc<Cell<bool>>,
    started: Instant,
    label: &'static str,
) {
    let mut complete = true;
    if let Some(main) = main.upgrade() {
        complete &= capture("main", &main, &output);
    } else {
        complete = false;
    }
    if let Some(second) = second.upgrade() {
        complete &= capture("second", &second, &output);
    } else {
        complete = false;
    }
    snapshots_ok.set(complete);
    println!(
        "event={label} ms={} snapshots-ok={complete}",
        started.elapsed().as_millis()
    );
    let _ = slint::quit_event_loop();
}

fn log_window_position(name: &str, window: &ProbeWindow) {
    let position = window.window().position();
    println!(
        "window-position window={name} x={} y={}",
        position.x, position.y
    );
}

fn show_second_after_main_setup(
    scenario: Scenario,
    second: slint::Weak<ProbeWindow>,
    started: Instant,
) {
    if matches!(scenario, Scenario::EarlySecond | Scenario::AfterMainClose) {
        return;
    }
    if let Some(second) = second.upgrade() {
        second
            .window()
            .show()
            .expect("second show after main setup failed");
        log_window_position("second", &second);
        println!(
            "event=show-second-after-main-setup ms={} deferred=false",
            started.elapsed().as_millis()
        );
    }
}

fn install_callbacks(
    name: &'static str,
    window: &ProbeWindow,
    peer: Option<slint::Weak<ProbeWindow>>,
    closes_program: bool,
) {
    window.on_drag_requested({
        let window = window.as_weak();
        move || {
            let result = window.upgrade().map(|app| {
                app.window()
                    .with_winit_window(|native| native.drag_window())
            });
            println!("drag window={name} result={result:?}");
        }
    });
    window.on_close_requested({
        let window = window.as_weak();
        let peer = peer.clone();
        move || {
            println!("drawn-close window={name}");
            if closes_program {
                if let Some(peer) = peer.as_ref().and_then(slint::Weak::upgrade) {
                    let _ = peer.window().hide();
                }
                let _ = slint::quit_event_loop();
                return;
            }
            if let Some(app) = window.upgrade() {
                let _ = app.window().hide();
            }
        }
    });
    let peer = peer.clone();
    window.window().on_close_requested(move || {
        println!("native-close window={name}");
        if closes_program {
            if let Some(peer) = peer.as_ref().and_then(slint::Weak::upgrade) {
                let _ = peer.window().hide();
            }
            let _ = slint::quit_event_loop();
        }
        slint::CloseRequestResponse::HideWindow
    });
}

fn main() {
    let config = parse_args();
    std::fs::create_dir_all(&config.output).expect("output directory creation failed");
    println!(
        "start mode={:?} scenario={:?} texture={:?} size={} forced-display={:?} session={} wayland-display={} display={} slint-backend={}",
        config.mode,
        config.scenario,
        config.texture,
        config.size,
        config.display_backend,
        std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unset".into()),
        std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "unset".into()),
        std::env::var("DISPLAY").unwrap_or_else(|_| "unset".into()),
        std::env::var("SLINT_BACKEND").unwrap_or_else(|_| "unset".into())
    );
    let (provided_manual_instance, selected_gpu) = choose_backend(&config);
    let shared_gpu = Rc::new(RefCell::new(selected_gpu));
    let main_notifier_instance = Rc::new(RefCell::new(provided_manual_instance));
    let textures = Rc::new(RefCell::new(Vec::new()));
    let main = ProbeWindow::new().expect("main window creation failed");
    let second = ProbeWindow::new().expect("second window creation failed");
    let main_weak = main.as_weak();
    let second_weak = second.as_weak();
    let snapshots_ok = Rc::new(Cell::new(config.scenario == Scenario::Interactive));
    let prepared = Rc::new(Cell::new(false));
    let main_setup = Rc::new(Cell::new(false));
    let second_setup = Rc::new(Cell::new(false));
    let main_teardown = Rc::new(Cell::new(false));
    let second_teardown = Rc::new(Cell::new(false));
    let initial_capture_ok = Rc::new(Cell::new(false));
    let close_cycle_closed = Rc::new(Cell::new(false));
    let close_cycle_reopened = Rc::new(Cell::new(false));
    let after_main_operation_ok = Rc::new(Cell::new(false));
    let started = Instant::now();
    let output = config.output.clone();
    let scenario = config.scenario;
    let capture_delay = if config.mode == Mode::Automatic {
        100
    } else {
        700
    };
    main.set_main_window(true);
    install_callbacks("main", &main, Some(second_weak.clone()), true);
    install_callbacks("second", &second, None, false);
    main.on_reopen_requested({
        let second = second_weak.clone();
        move || {
            println!("drawn-reopen window=second");
            if let Some(second) = second.upgrade() {
                second
                    .window()
                    .show()
                    .expect("second reopen from main failed");
                log_window_position("second", &second);
            }
        }
    });
    if config.display_backend == DisplayBackend::X11 && config.scenario == Scenario::Interactive {
        main.window()
            .set_position(slint::PhysicalPosition::new(100, 100));
        second
            .window()
            .set_position(slint::PhysicalPosition::new(900, 100));
        println!("window-position-requested backend=x11 main=100,100 second=900,100");
    }
    let set_images = {
        let main_weak = main_weak.clone();
        let second_weak = second_weak.clone();
        let shared_gpu = shared_gpu.clone();
        let prepared = prepared.clone();
        let textures = textures.clone();
        let output = output.clone();
        let snapshots_ok = snapshots_ok.clone();
        let initial_capture_ok = initial_capture_ok.clone();
        let close_cycle_closed = close_cycle_closed.clone();
        let close_cycle_reopened = close_cycle_reopened.clone();
        let after_main_operation_ok = after_main_operation_ok.clone();
        move || {
            if prepared.replace(true) {
                return;
            }
            let Some((device, queue)) = shared_gpu.borrow().as_ref().cloned() else {
                println!("texture setup unavailable: no main rendering device");
                return;
            };
            let primary = create_texture(&device, &queue, config.size, 0);
            let secondary = if config.texture == TextureKind::Shared {
                primary.clone()
            } else {
                create_texture(&device, &queue, config.size, 1)
            };
            textures
                .borrow_mut()
                .extend([primary.clone(), secondary.clone()]);
            main_weak
                .upgrade()
                .expect("main window dropped")
                .set_texture(slint::Image::try_from(primary).expect("main texture import failed"));
            second_weak
                .upgrade()
                .expect("second window dropped")
                .set_texture(
                    slint::Image::try_from(secondary).expect("second texture import failed"),
                );
            println!(
                "event=texture-setup ms={} device=main allocation={:?}",
                started.elapsed().as_millis(),
                config.texture
            );
            if scenario == Scenario::Interactive {
                println!(
                    "event=interactive-ready ms={} automatic-quit=false",
                    started.elapsed().as_millis()
                );
                return;
            }
            match scenario {
                Scenario::Display | Scenario::EarlySecond => {
                    let main = main_weak.clone();
                    let second = second_weak.clone();
                    let output = output.clone();
                    let snapshots_ok = snapshots_ok.clone();
                    slint::Timer::single_shot(Duration::from_millis(capture_delay), move || {
                        complete_capture(
                            main,
                            second,
                            output,
                            snapshots_ok,
                            started,
                            "capture-complete",
                        );
                    });
                }
                Scenario::CloseCycle => {
                    let main = main_weak.clone();
                    let second = second_weak.clone();
                    let initial_output = output.clone();
                    let initial_capture_for_timer = initial_capture_ok.clone();
                    slint::Timer::single_shot(Duration::from_millis(500), move || {
                        let initial_ok = match (main.upgrade(), second.upgrade()) {
                            (Some(main), Some(second)) => {
                                let main_ok = capture("main", &main, &initial_output);
                                let second_ok = capture("second", &second, &initial_output);
                                println!(
                                    "event=initial-capture-before-close ms={} main={main_ok} second={second_ok}",
                                    started.elapsed().as_millis()
                                );
                                main_ok && second_ok
                            }
                            _ => false,
                        };
                        initial_capture_for_timer.set(initial_ok);
                    });
                    let second = second_weak.clone();
                    let close_cycle_closed_for_timer = close_cycle_closed.clone();
                    let initial_capture_for_close = initial_capture_ok.clone();
                    slint::Timer::single_shot(Duration::from_millis(800), move || {
                        if !initial_capture_for_close.get() {
                            println!("event=close-second-skipped initial=false");
                            return;
                        }
                        if let Some(second) = second.upgrade() {
                            second.window().hide().expect("second hide failed");
                            close_cycle_closed_for_timer.set(true);
                            println!(
                                "event=close-second-after-capture ms={}",
                                started.elapsed().as_millis()
                            );
                        }
                    });
                    let second = second_weak.clone();
                    let close_cycle_reopened_for_timer = close_cycle_reopened.clone();
                    let close_cycle_closed_for_reopen = close_cycle_closed.clone();
                    slint::Timer::single_shot(Duration::from_millis(1000), move || {
                        if !close_cycle_closed_for_reopen.get() {
                            println!("event=reopen-second-skipped closed=false");
                            return;
                        }
                        if let Some(second) = second.upgrade() {
                            second.window().show().expect("second reopen failed");
                            close_cycle_reopened_for_timer.set(true);
                            println!("event=reopen-second ms={}", started.elapsed().as_millis());
                        }
                    });
                    let main = main_weak.clone();
                    let second = second_weak.clone();
                    let output = output.clone();
                    let snapshots_ok = snapshots_ok.clone();
                    let initial_capture_ok = initial_capture_ok.clone();
                    let close_cycle_closed = close_cycle_closed.clone();
                    let close_cycle_reopened = close_cycle_reopened.clone();
                    slint::Timer::single_shot(Duration::from_millis(1400), move || {
                        let final_ok = match (main.upgrade(), second.upgrade()) {
                            (Some(main), Some(second)) => {
                                capture("main", &main, &output)
                                    && capture("second", &second, &output)
                            }
                            _ => false,
                        };
                        let complete = initial_capture_ok.get()
                            && close_cycle_closed.get()
                            && close_cycle_reopened.get()
                            && final_ok;
                        snapshots_ok.set(complete);
                        println!(
                            "event=close-cycle-complete ms={} initial={} closed={} reopened={} final={final_ok} snapshots-ok={complete}",
                            started.elapsed().as_millis(),
                            initial_capture_ok.get(),
                            close_cycle_closed.get(),
                            close_cycle_reopened.get()
                        );
                        let _ = slint::quit_event_loop();
                    });
                }
                Scenario::AfterMainClose => {
                    let main = main_weak.clone();
                    let initial_output = output.clone();
                    let initial_capture_for_timer = initial_capture_ok.clone();
                    slint::Timer::single_shot(Duration::from_millis(100), move || {
                        let initial_ok = main
                            .upgrade()
                            .is_some_and(|main| capture("main", &main, &initial_output));
                        initial_capture_for_timer.set(initial_ok);
                        println!(
                            "event=initial-main-capture ms={} ok={initial_ok}",
                            started.elapsed().as_millis()
                        );
                    });
                    let main = main_weak.clone();
                    let second = second_weak.clone();
                    let after_main_operation_for_timer = after_main_operation_ok.clone();
                    slint::Timer::single_shot(Duration::from_millis(200), move || {
                        let mut operation_ok = false;
                        if let Some(main) = main.upgrade() {
                            main.window().hide().expect("main hide failed");
                            operation_ok = true;
                        }
                        if let Some(second) = second.upgrade() {
                            second
                                .window()
                                .show()
                                .expect("second show after main hide failed");
                        } else {
                            operation_ok = false;
                        }
                        after_main_operation_for_timer.set(operation_ok);
                        println!(
                            "event=after-main-close ms={} main=hidden second=shown",
                            started.elapsed().as_millis()
                        );
                    });
                    let second = second_weak.clone();
                    let output = output.clone();
                    let snapshots_ok = snapshots_ok.clone();
                    let initial_capture_ok = initial_capture_ok.clone();
                    let after_main_operation_ok = after_main_operation_ok.clone();
                    slint::Timer::single_shot(Duration::from_millis(500), move || {
                        let final_ok = second
                            .upgrade()
                            .is_some_and(|second| capture("second", &second, &output));
                        let complete =
                            initial_capture_ok.get() && after_main_operation_ok.get() && final_ok;
                        snapshots_ok.set(complete);
                        println!(
                            "event=after-main-close-complete ms={} initial={} operation={} final={final_ok} snapshots-ok={complete}",
                            started.elapsed().as_millis(),
                            initial_capture_ok.get(),
                            after_main_operation_ok.get()
                        );
                        let _ = slint::quit_event_loop();
                    });
                }
                Scenario::Interactive => unreachable!(),
            }
        }
    };
    let second_notifier_device = Rc::new(RefCell::new(None::<wgpu::Device>));
    let second_notifier_instance = Rc::new(RefCell::new(None::<wgpu::Instance>));
    {
        let second_setup = second_setup.clone();
        let second_teardown = second_teardown.clone();
        let shared_gpu = shared_gpu.clone();
        let main_notifier_instance = main_notifier_instance.clone();
        let second_notifier_device = second_notifier_device.clone();
        let second_notifier_instance = second_notifier_instance.clone();
        second
            .window()
            .set_rendering_notifier(move |state, api| match state {
                slint::RenderingState::RenderingSetup => {
                    second_setup.set(true);
                    let (main_available, device_id_eq, same_instance) = match api {
                        slint::GraphicsAPI::WGPU29 {
                            instance, device, ..
                        } => {
                            let device_comparison = shared_gpu
                                .borrow()
                                .as_ref()
                                .map(|(main_device, _)| device == main_device);
                            let instance_comparison = main_notifier_instance
                                .borrow()
                                .as_ref()
                                .map(|main_instance| instance == main_instance);
                            *second_notifier_device.borrow_mut() = Some(device.clone());
                            *second_notifier_instance.borrow_mut() = Some(instance.clone());
                            (
                                instance_comparison.is_some(),
                                device_comparison,
                                instance_comparison,
                            )
                        }
                        _ => (false, None, None),
                    };
                    println!(
                        "event=rendering-setup ms={} window=second",
                        started.elapsed().as_millis()
                    );
                    println!(
                        "event=device-identity window=second mode={:?} main-available={main_available} device-id-eq={device_id_eq:?} same-instance={same_instance:?}",
                        config.mode
                    );
                }
                slint::RenderingState::RenderingTeardown => {
                    second_teardown.set(true);
                    println!(
                        "event=rendering-teardown ms={} window=second",
                        started.elapsed().as_millis()
                    );
                }
                _ => {}
            })
            .expect("second notifier installation failed");
    }
    if config.mode == Mode::Automatic {
        let shared_gpu = shared_gpu.clone();
        let set_images = set_images.clone();
        let main_setup = main_setup.clone();
        let main_teardown = main_teardown.clone();
        let second = second_weak.clone();
        let main_notifier_instance = main_notifier_instance.clone();
        let second_notifier_device = second_notifier_device.clone();
        let second_notifier_instance = second_notifier_instance.clone();
        main.window()
            .set_rendering_notifier(move |state, api| {
                if let slint::RenderingState::RenderingSetup = state
                    && let slint::GraphicsAPI::WGPU29 {
                        instance,
                        device,
                        queue,
                        ..
                    } = api
                {
                    main_setup.set(true);
                    println!(
                        "event=rendering-setup ms={} window=main api=wgpu29",
                        started.elapsed().as_millis()
                    );
                    *shared_gpu.borrow_mut() = Some((device.clone(), queue.clone()));
                    *main_notifier_instance.borrow_mut() = Some(instance.clone());
                    let device_id_eq = second_notifier_device
                        .borrow()
                        .as_ref()
                        .map(|second_device| device == second_device);
                    let same_instance = second_notifier_instance
                        .borrow()
                        .as_ref()
                        .map(|second_instance| instance == second_instance);
                    println!(
                        "event=device-identity window=main mode=Automatic second-available={} device-id-eq={device_id_eq:?} same-instance={same_instance:?}",
                        same_instance.is_some()
                    );
                    show_second_after_main_setup(scenario, second.clone(), started);
                    let set_images = set_images.clone();
                    slint::Timer::single_shot(Duration::ZERO, set_images);
                }
                if let slint::RenderingState::RenderingTeardown = state {
                    main_teardown.set(true);
                    println!(
                        "event=rendering-teardown ms={} window=main",
                        started.elapsed().as_millis()
                    );
                }
            })
            .expect("main notifier installation failed");
    } else {
        let main_setup = main_setup.clone();
        let main_teardown = main_teardown.clone();
        let second = second_weak.clone();
        let shared_gpu = shared_gpu.clone();
        let main_notifier_instance = main_notifier_instance.clone();
        let second_notifier_device = second_notifier_device.clone();
        let second_notifier_instance = second_notifier_instance.clone();
        main.window()
            .set_rendering_notifier(move |state, api| match state {
                slint::RenderingState::RenderingSetup => {
                    main_setup.set(true);
                    println!(
                        "event=rendering-setup ms={} window=main api=manual-main-device",
                        started.elapsed().as_millis()
                    );
                    let (device_id_eq, same_instance) = match api {
                        slint::GraphicsAPI::WGPU29 {
                            instance, device, ..
                        } => {
                            *main_notifier_instance.borrow_mut() = Some(instance.clone());
                            (
                                shared_gpu
                                    .borrow()
                                    .as_ref()
                                    .map(|(main_device, _)| device == main_device),
                                second_notifier_instance
                                    .borrow()
                                    .as_ref()
                                    .map(|second_instance| instance == second_instance),
                            )
                        }
                        _ => (None, None),
                    };
                    println!(
                        "event=device-identity window=main mode=Manual second-available={} device-id-eq={device_id_eq:?} same-instance={same_instance:?}",
                        second_notifier_device.borrow().is_some()
                    );
                    show_second_after_main_setup(scenario, second.clone(), started);
                }
                slint::RenderingState::RenderingTeardown => {
                    main_teardown.set(true);
                    println!(
                        "event=rendering-teardown ms={} window=main",
                        started.elapsed().as_millis()
                    );
                }
                _ => {}
            })
            .expect("main notifier installation failed");
        set_images();
    }
    if config.scenario == Scenario::EarlySecond {
        second.window().show().expect("early second show failed");
        log_window_position("second", &second);
        println!("show second=early");
    }
    let main_for_backend = main_weak.clone();
    slint::Timer::single_shot(Duration::from_millis(150), move || {
        let resolved = main_for_backend
            .upgrade()
            .and_then(|main| {
                main.window()
                    .with_winit_window(|native| native.xdg_toplevel().is_some())
            })
            .map(|wayland| if wayland { "wayland" } else { "x11" })
            .unwrap_or("unavailable");
        println!("resolved-display={resolved}");
    });
    if config.scenario != Scenario::Interactive {
        let watchdog_ok = snapshots_ok.clone();
        let watchdog_started = started;
        slint::Timer::single_shot(Duration::from_secs(20), move || {
            watchdog_ok.set(false);
            println!(
                "event=watchdog-timeout ms={}",
                watchdog_started.elapsed().as_millis()
            );
            let _ = slint::quit_event_loop();
        });
    }
    main.window().show().expect("main show failed");
    log_window_position("main", &main);
    println!(
        "event=show-main-before-loop ms={}",
        started.elapsed().as_millis()
    );
    slint::run_event_loop_until_quit().expect("event loop failed");
    println!(
        "lifecycle main-setup={} second-setup={} main-teardown={} second-teardown={}",
        main_setup.get(),
        second_setup.get(),
        main_teardown.get(),
        second_teardown.get()
    );
    println!("exit snapshots-ok={}", snapshots_ok.get());
    std::process::exit(if snapshots_ok.get() { 0 } else { 1 });
}

#[cfg(test)]
mod tests {
    use super::{pattern_bytes, pattern_matches, snapshot_samples};
    use slint::{Rgba8Pixel, SharedPixelBuffer};
    #[test]
    fn pattern_checker_accepts_colored_quadrants() {
        assert!(pattern_matches([
            [240, 40, 40, 255],
            [40, 220, 50, 255],
            [40, 90, 240, 255],
            [240, 220, 40, 255]
        ]));
        assert!(pattern_matches([
            [40, 220, 50, 255],
            [240, 40, 40, 255],
            [240, 220, 40, 255],
            [40, 90, 240, 255]
        ]));
    }
    #[test]
    fn pattern_checker_rejects_uniform_or_changed_regions() {
        assert!(!pattern_matches([[40, 40, 40, 255]; 4]));
        assert!(!pattern_matches([
            [240, 40, 40, 255],
            [40, 220, 50, 255],
            [40, 90, 240, 255],
            [40, 90, 240, 255]
        ]));
    }
    #[test]
    fn snapshot_sampler_checks_actual_64_and_96_pixel_buffers() {
        for (size, variant) in [(64, 0), (96, 1)] {
            let mut bytes = pattern_bytes(size, variant);
            let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&bytes, size, size);
            assert!(pattern_matches(snapshot_samples(&buffer)), "size={size}");
            bytes.fill(40);
            let uniform = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&bytes, size, size);
            assert!(
                !pattern_matches(snapshot_samples(&uniform)),
                "uniform size={size}"
            );
        }
        let mut changed = pattern_bytes(64, 0);
        let sample = ((64 * 2 / 3 * 64 + 64 * 3 / 4) * 4) as usize;
        changed[sample..sample + 4].copy_from_slice(&[1, 2, 3, 255]);
        let changed = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&changed, 64, 64);
        assert!(!pattern_matches(snapshot_samples(&changed)));
    }
}
