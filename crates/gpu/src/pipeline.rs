// SPDX-License-Identifier: GPL-3.0-or-later

//! The compute-pipeline plumbing every kernel family builds through: the
//! `Pipeline` type, bind-group-layout-entry helpers, the pipeline builder,
//! the dispatch-and-submit helpers and the uniform-buffer upload helper.

/// One compute pipeline plus the bind group layout it was built with.
pub(crate) struct Pipeline {
    compute: wgpu::ComputePipeline,
    bind_group_layout: wgpu::BindGroupLayout,
}

pub(crate) fn storage_entry(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

pub(crate) fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

/// A write-only `Rgba8Unorm` storage-texture binding -- the display kernel's
/// `dst` (`crate::display`), the only kernel in this crate that writes a
/// texture rather than a buffer. `downlevel_defaults()` allows 4 storage
/// textures per stage (inherited from `defaults()`), so the device limits
/// carry this unchanged.
pub(crate) fn storage_texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::StorageTexture {
            access: wgpu::StorageTextureAccess::WriteOnly,
            format: wgpu::TextureFormat::Rgba8Unorm,
            view_dimension: wgpu::TextureViewDimension::D2,
        },
        count: None,
    }
}

/// Builds one [`Pipeline`]: shader module, bind group layout, pipeline
/// layout, compute pipeline. `wgsl_src` is always an `include_str!` at the
/// call site, never a string literal. Every pipeline in this crate --
/// `crate::harness::GpuBackend`'s own harness kernel included -- is built
/// through this one function.
pub(crate) fn build_pipeline(
    device: &wgpu::Device,
    wgsl_src: &'static str,
    label: &str,
    entry_point: &str,
    bindings: &[wgpu::BindGroupLayoutEntry],
) -> Pipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(wgsl_src.into()),
    });
    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries: bindings,
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(&bind_group_layout)],
        immediate_size: 0,
    });
    let compute = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(label),
        layout: Some(&pipeline_layout),
        module: &shader,
        entry_point: Some(entry_point),
        compilation_options: Default::default(),
        cache: None,
    });
    Pipeline {
        compute,
        bind_group_layout,
    }
}

/// Encodes one bind group over `entries`, dispatches `ceil(invocation_count
/// / 64)` workgroups of `pipeline` on x alone and submits it; the shared tail
/// of every 1-D kernel. x is capped at 65,535 workgroups, so
/// `invocation_count` must stay at or under 4,194,240. Zero dispatches nothing.
pub(crate) fn run_pass(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &Pipeline,
    entries: &[wgpu::BindGroupEntry],
    invocation_count: u32,
    label: &str,
) {
    if invocation_count == 0 {
        return;
    }
    dispatch(
        device,
        queue,
        pipeline,
        entries,
        [invocation_count.div_ceil(64), 1, 1],
        label,
    );
}

/// [`run_pass`] for a kernel with an 8 x 8 workgroup indexing
/// `(global_id.x, global_id.y)`: dispatches `ceil(width / 8) x ceil(height / 8)`
/// workgroups, so the ceiling is per axis rather than on the product. A zero
/// side dispatches nothing.
pub(crate) fn run_pass_2d(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &Pipeline,
    entries: &[wgpu::BindGroupEntry],
    width: u32,
    height: u32,
    label: &str,
) {
    if width == 0 || height == 0 {
        return;
    }
    dispatch(
        device,
        queue,
        pipeline,
        entries,
        [width.div_ceil(8), height.div_ceil(8), 1],
        label,
    );
}

fn dispatch(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipeline: &Pipeline,
    entries: &[wgpu::BindGroupEntry],
    [x, y, z]: [u32; 3],
    label: &str,
) {
    crate::errors::record_dispatch(label);
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(label),
        layout: &pipeline.bind_group_layout,
        entries,
    });
    let mut encoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some(label) });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some(label),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline.compute);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.dispatch_workgroups(x, y, z);
    }
    queue.submit(Some(encoder.finish()));
}

/// A uniform buffer of exactly `bytes.len()` bytes: `create_buffer` +
/// `queue.write_buffer`, never `mapped_at_creation` -- every uniform buffer
/// in this crate, `GpuBackend::self_check`'s own params buffer included, is
/// built this way.
pub(crate) fn upload_params(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    bytes: &[u8],
) -> wgpu::Buffer {
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("focusforge-gpu-params"),
        size: bytes.len() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&buffer, 0, bytes);
    buffer
}
