// SPDX-License-Identifier: GPL-3.0-or-later
//! Spike: prove a texture written by a wgpu compute shader on the *same* wgpu::Device Slint
//! renders with can be displayed in a Slint window with no CPU round-trip on the display path.
//! Throwaway spike — nothing here is meant to be reused.

mod pattern_cpu;
mod readback;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use slint::wgpu_29::wgpu;

const WIDTH: u32 = pattern_cpu::WIDTH;
const HEIGHT: u32 = pattern_cpu::HEIGHT;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DeviceOwner {
    Spike,
    Slint,
}

struct Config {
    device_owner: DeviceOwner,
    headless_frames: Option<u32>,
    dump: Option<PathBuf>,
    verify_pattern: bool,
}

fn parse_args() -> Config {
    let mut device_owner = DeviceOwner::Spike;
    let mut headless_frames = None;
    let mut dump = None;
    let mut verify_pattern = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--device-owner" => {
                let v = args.next().expect("--device-owner needs a value");
                device_owner = match v.as_str() {
                    "spike" => DeviceOwner::Spike,
                    "slint" => DeviceOwner::Slint,
                    other => panic!("unknown --device-owner {other} (expected spike|slint)"),
                };
            }
            "--headless-frames" => {
                let v = args.next().expect("--headless-frames needs a value");
                headless_frames = Some(v.parse().expect("--headless-frames needs an integer"));
            }
            "--dump" => {
                let v = args.next().expect("--dump needs a path");
                dump = Some(PathBuf::from(v));
            }
            "--verify-pattern" => verify_pattern = true,
            other => panic!("unknown argument {other}"),
        }
    }

    Config {
        device_owner,
        headless_frames,
        dump,
        verify_pattern,
    }
}

/// The device/queue/texture/pipeline the compute shader dispatches into. Owned on the
/// `--device-owner spike` path from startup; captured lazily from the rendering notifier on
/// the `--device-owner slint` path, since Slint itself owns the device there.
struct SharedGpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    texture: wgpu::Texture,
    uniform_buf: wgpu::Buffer,
    pipeline: wgpu::ComputePipeline,
    bind_group: wgpu::BindGroup,
}

impl SharedGpu {
    fn new(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("spike-pattern-texture"),
            size: wgpu::Extent3d {
                width: WIDTH,
                height: HEIGHT,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("spike-pattern-uniform"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("spike-pattern-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: wgpu::TextureFormat::Rgba8Unorm,
                        view_dimension: wgpu::TextureViewDimension::D2,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("spike-pattern-bind-group"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Buffer(uniform_buf.as_entire_buffer_binding()),
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("spike-pattern-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("pattern.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("spike-pattern-pipeline-layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("spike-pattern-pipeline"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });

        Self {
            device,
            queue,
            texture,
            uniform_buf,
            pipeline,
            bind_group,
        }
    }

    /// Writes the `t` uniform, dispatches the compute pass and submits — all on the shared
    /// queue, no staging buffer anywhere in this path.
    fn dispatch(&self, t: f32) {
        let mut params = [0u8; 16];
        params[0..4].copy_from_slice(&t.to_le_bytes());
        self.queue.write_buffer(&self.uniform_buf, 0, &params);

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("spike-pattern-encoder"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("spike-pattern-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.dispatch_workgroups(WIDTH.div_ceil(8), HEIGHT.div_ceil(8), 1);
        }
        self.queue.submit(Some(encoder.finish()));
    }
}

/// Same trick `i-slint-core`'s wgpu_29 backend uses (`i-slint-core-1.17.1/graphics/wgpu_29.rs`,
/// `poll_once`): wgpu's adapter/device request futures are ready on first poll on every native
/// platform, so no async executor dependency is needed for a spike.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(v) => return v,
            std::task::Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn create_manual_device() -> (wgpu::Instance, wgpu::Adapter, wgpu::Device, wgpu::Queue) {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::from_env().unwrap_or_default(),
        flags: wgpu::InstanceFlags::from_build_config().with_env(),
        backend_options: wgpu::BackendOptions::from_env_or_default(),
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        display: None,
    });
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::default(),
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .expect("failed to find a wgpu adapter");
    let (device, queue) = block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("spike-device"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        memory_hints: wgpu::MemoryHints::default(),
        trace: wgpu::Trace::default(),
    }))
    .expect("failed to create a wgpu device");
    (instance, adapter, device, queue)
}

slint::slint! {
    export component AppWindow inherits Window {
        preferred-width: 512px;
        preferred-height: 512px;
        title: "slint-wgpu-spike";
        in-out property <image> spike-texture;
        Image { source: root.spike-texture; }
    }
}

fn main() {
    let config = parse_args();
    println!("device-owner: {:?}", config.device_owner);

    // t is pinned at 0.0 for --dump / --verify-pattern so the readback is deterministic.
    let static_t = config.verify_pattern || config.dump.is_some();

    let shared: Rc<RefCell<Option<SharedGpu>>> = Rc::new(RefCell::new(None));
    let exit_code: Rc<std::cell::Cell<i32>> = Rc::new(std::cell::Cell::new(0));

    match config.device_owner {
        DeviceOwner::Spike => {
            let (instance, adapter, device, queue) = create_manual_device();
            let info = adapter.get_info();
            println!(
                "adapter: name={:?} backend={:?} driver={:?} driver_info={:?}",
                info.name, info.backend, info.driver, info.driver_info
            );
            *shared.borrow_mut() = Some(SharedGpu::new(device.clone(), queue.clone()));

            let select_result = slint::BackendSelector::new()
                .require_wgpu_29(slint::wgpu_29::WGPUConfiguration::Manual {
                    instance,
                    adapter,
                    device,
                    queue,
                })
                .select();
            if let Err(e) = select_result {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        DeviceOwner::Slint => {
            // WGPUSettings is #[non_exhaustive]: outside its crate a struct literal (even with
            // `..Default::default()`) is rejected, so build the default and mutate the one field
            // that matters (the default's compute limits are zero).
            let mut settings = slint::wgpu_29::WGPUSettings::default();
            settings.device_required_limits = wgpu::Limits::default();
            let select_result = slint::BackendSelector::new()
                .require_wgpu_29(slint::wgpu_29::WGPUConfiguration::Automatic(settings))
                .select();
            if let Err(e) = select_result {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
    }

    let app = AppWindow::new().expect("failed to instantiate AppWindow");
    let app_weak = app.as_weak();

    if config.device_owner == DeviceOwner::Spike {
        let g = shared.borrow();
        let g = g.as_ref().unwrap();
        g.dispatch(0.0);
        let image = slint::Image::try_from(g.texture.clone())
            .expect("Image::try_from(wgpu::Texture) rejected the spike's own texture");
        app.set_spike_texture(image);
    }

    let frame_count = Rc::new(std::cell::Cell::new(0u32));

    {
        let shared = shared.clone();
        let app_weak = app_weak.clone();
        let frame_count = frame_count.clone();
        let exit_code = exit_code.clone();
        let headless_frames = config.headless_frames;
        let dump_path = config.dump.clone();
        let verify_pattern = config.verify_pattern;

        let notifier_result = app.window().set_rendering_notifier(move |state, graphics_api| {
            if let slint::RenderingState::RenderingSetup = state {
                println!("rendering notifier: graphics_api={:?}", graphics_api);
                if let slint::GraphicsAPI::WGPU29 { device, queue, .. } = graphics_api {
                    if shared.borrow().is_none() {
                        // --device-owner slint: this is the only place device/queue exist.
                        let new_shared = SharedGpu::new(device.clone(), queue.clone());
                        new_shared.dispatch(0.0);
                        if let Some(app) = app_weak.upgrade() {
                            let image = slint::Image::try_from(new_shared.texture.clone())
                                .expect("Image::try_from(wgpu::Texture) rejected the Automatic-path texture");
                            app.set_spike_texture(image);
                        }
                        *shared.borrow_mut() = Some(new_shared);
                    }
                }
            }

            if let slint::RenderingState::AfterRendering = state {
                let n = frame_count.get() + 1;
                frame_count.set(n);

                if let Some(target) = headless_frames {
                    if n == target {
                        let g = shared.borrow();
                        if let Some(g) = g.as_ref() {
                            if verify_pattern {
                                let result =
                                    readback::verify_pattern(&g.device, &g.queue, &g.texture, WIDTH, HEIGHT, 0.0);
                                println!(
                                    "verify-pattern: worst |delta|={} at pixel={:?} channel={}",
                                    result.worst_delta, result.worst_pixel, result.worst_channel
                                );
                                if result.worst_delta > 1 {
                                    exit_code.set(1);
                                }
                            }
                            if let Some(path) = &dump_path {
                                if let Err(e) =
                                    readback::dump_png(&g.device, &g.queue, &g.texture, WIDTH, HEIGHT, path)
                                {
                                    eprintln!("--dump failed: {e}");
                                    exit_code.set(1);
                                } else {
                                    println!("dumped {}", path.display());
                                }
                            }
                        }
                        let _ = slint::quit_event_loop();
                    }
                }
            }
        });

        if let Err(e) = notifier_result {
            eprintln!("set_rendering_notifier failed: {e}");
            std::process::exit(1);
        }
    }

    {
        let shared = shared.clone();
        let app_weak = app_weak.clone();
        let start = std::time::Instant::now();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(16),
            move || {
                let g = shared.borrow();
                if let Some(g) = g.as_ref() {
                    let t = if static_t {
                        0.0
                    } else {
                        (start.elapsed().as_secs_f32() * 0.1) % 1.0
                    };
                    g.dispatch(t);
                }
                if let Some(app) = app_weak.upgrade() {
                    app.window().request_redraw();
                }
            },
        );

        app.run().expect("event loop exited with an error");
    }

    std::process::exit(exit_code.get());
}
